import { chmodSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { nowIso } from "./utils.js";

function hydrateAgent(row) {
  if (!row) return null;
  return {
    ...row,
    tools: JSON.parse(row.tools_json || "[]"),
    skills: JSON.parse(row.skills_json || "[]"),
    extensions: JSON.parse(row.extensions_json || "[]"),
  };
}

export class HubStore {
  constructor(path) {
    this.db = new DatabaseSync(path);
    chmodSync(path, 0o600);
    this.db.exec(`
      PRAGMA journal_mode = WAL;
      PRAGMA busy_timeout = 5000;
      PRAGMA foreign_keys = ON;
      CREATE TABLE IF NOT EXISTS agents (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        cwd TEXT NOT NULL,
        workspace_mode TEXT NOT NULL,
        provider TEXT,
        model TEXT,
        thinking TEXT,
        status TEXT NOT NULL,
        desired_state TEXT NOT NULL,
        pid INTEGER,
        session_id TEXT NOT NULL,
        session_file TEXT,
        session_dir TEXT NOT NULL,
        agent_dir TEXT NOT NULL,
        logs_dir TEXT NOT NULL,
        profile_digest TEXT NOT NULL,
        tools_json TEXT NOT NULL,
        skills_json TEXT NOT NULL,
        extensions_json TEXT NOT NULL,
        last_error TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS events (
        cursor INTEGER PRIMARY KEY AUTOINCREMENT,
        event_id TEXT NOT NULL UNIQUE,
        agent_id TEXT,
        event_type TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY(agent_id) REFERENCES agents(id) ON DELETE CASCADE
      );
      CREATE INDEX IF NOT EXISTS idx_events_agent_cursor ON events(agent_id, cursor);
      CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);
    `);
  }

  createAgent(agent) {
    this.db.prepare(`
      INSERT INTO agents (
        id, name, cwd, workspace_mode, provider, model, thinking, status, desired_state, pid,
        session_id, session_file, session_dir, agent_dir, logs_dir, profile_digest,
        tools_json, skills_json, extensions_json, last_error, created_at, updated_at
      ) VALUES (
        @id, @name, @cwd, @workspace_mode, @provider, @model, @thinking, @status, @desired_state, @pid,
        @session_id, @session_file, @session_dir, @agent_dir, @logs_dir, @profile_digest,
        @tools_json, @skills_json, @extensions_json, @last_error, @created_at, @updated_at
      )
    `).run(agent);
    return this.getAgent(agent.id);
  }

  getAgent(id) {
    return hydrateAgent(this.db.prepare("SELECT * FROM agents WHERE id = ?").get(id));
  }

  listAgents({ includeStopped = true } = {}) {
    const rows = includeStopped
      ? this.db.prepare("SELECT * FROM agents ORDER BY created_at DESC").all()
      : this.db.prepare("SELECT * FROM agents WHERE status NOT IN ('stopped', 'failed') ORDER BY created_at DESC").all();
    return rows.map(hydrateAgent);
  }

  updateAgent(id, fields) {
    const allowed = new Set([
      "name", "status", "desired_state", "pid", "session_file", "last_error", "provider", "model", "thinking", "updated_at",
    ]);
    const entries = Object.entries(fields).filter(([key]) => allowed.has(key));
    if (!entries.some(([key]) => key === "updated_at")) entries.push(["updated_at", nowIso()]);
    if (entries.length === 0) return this.getAgent(id);
    const params = { id };
    const setters = entries.map(([key, value]) => {
      params[key] = value;
      return `${key} = @${key}`;
    });
    this.db.prepare(`UPDATE agents SET ${setters.join(", ")} WHERE id = @id`).run(params);
    return this.getAgent(id);
  }

  appendEvent({ eventId, agentId = null, eventType, payload, createdAt = nowIso() }) {
    const result = this.db.prepare(`
      INSERT INTO events (event_id, agent_id, event_type, payload_json, created_at)
      VALUES (?, ?, ?, ?, ?)
    `).run(eventId, agentId, eventType, JSON.stringify(payload), createdAt);
    return Number(result.lastInsertRowid);
  }

  listEvents({ agentId = null, after = 0, limit = 500 } = {}) {
    const safeLimit = Math.max(1, Math.min(Number(limit) || 500, 2000));
    const rows = agentId
      ? this.db.prepare("SELECT * FROM events WHERE agent_id = ? AND cursor > ? ORDER BY cursor ASC LIMIT ?").all(agentId, Number(after) || 0, safeLimit)
      : this.db.prepare("SELECT * FROM events WHERE cursor > ? ORDER BY cursor ASC LIMIT ?").all(Number(after) || 0, safeLimit);
    return rows.map((row) => ({ ...row, payload: JSON.parse(row.payload_json) }));
  }

  latestCursor() {
    return Number(this.db.prepare("SELECT COALESCE(MAX(cursor), 0) AS cursor FROM events").get().cursor);
  }

  markInterruptedAgents() {
    const active = ["provisioning", "starting", "idle", "running", "waiting_input", "stopping"];
    const placeholders = active.map(() => "?").join(",");
    const now = nowIso();
    this.db.prepare(`
      UPDATE agents
      SET status = 'crashed', pid = NULL, last_error = 'daemon restarted; previous RPC stdio cannot be reattached', updated_at = ?
      WHERE status IN (${placeholders})
    `).run(now, ...active);
  }

  close() {
    this.db.close();
  }
}
