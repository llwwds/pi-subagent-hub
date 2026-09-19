import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { HubStore } from "../src/store.js";

test("store keeps one row per agent and ordered event cursors", () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-store-"));
  const store = new HubStore(join(root, "hub.sqlite"));
  const now = new Date().toISOString();
  store.createAgent({
    id: "agent-one", name: "one", cwd: root, workspace_mode: "shared", provider: null, model: null, thinking: null,
    status: "idle", desired_state: "running", pid: 123, session_id: "session-one", session_file: null,
    session_dir: join(root, "sessions"), agent_dir: join(root, "agent"), logs_dir: join(root, "logs"),
    profile_digest: "sha256:test", tools_json: '["read"]', skills_json: "[]", extensions_json: "[]",
    last_error: null, created_at: now, updated_at: now,
  });
  store.appendEvent({ eventId: "event-one", agentId: "agent-one", eventType: "agent_ready", payload: { n: 1 } });
  store.appendEvent({ eventId: "event-two", agentId: "agent-one", eventType: "agent_settled", payload: { n: 2 } });
  assert.equal(store.listAgents().length, 1);
  assert.deepEqual(store.getAgent("agent-one").tools, ["read"]);
  assert.deepEqual(store.listEvents({ agentId: "agent-one", after: 1 }).map((item) => item.payload.n), [2]);
  store.close();
  rmSync(root, { recursive: true, force: true });
});
