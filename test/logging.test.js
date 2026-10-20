import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { Writable } from "node:stream";
import {
  DEFAULT_HUB_LOG_LIMITS,
  createAgentLogWriter,
  createHubLogWriter,
  isEphemeralStreamEvent,
  resolveHubLogDir,
  summarizeProtocol,
} from "../src/logging.js";
import { HubStore } from "../src/store.js";
import { AgentManager } from "../src/manager.js";
import { ensureHubLayout, resolveHubPaths } from "../src/paths.js";
import { resolveProfile } from "../src/profile.js";

function tempHome(label) {
  const root = mkdtempSync(join(tmpdir(), `pihub-${label}-`));
  return { root, logDir: join(root, "state", "logs") };
}

function readLines(path) {
  return readFileSync(path, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

function logFileNames(dir) {
  return readdirSync(dir).filter((name) => name !== ".hub.log.history");
}

class ManualStream extends Writable {
  constructor() {
    super({ highWaterMark: 8 });
    this.chunks = [];
    this.pending = [];
  }

  _write(chunk, _encoding, callback) {
    this.chunks.push(chunk.toString());
    this.pending.push(callback);
  }
}

async function releaseAll(stream) {
  for (let attempt = 0; attempt < 2_000; attempt += 1) {
    if (stream.pending.length === 0) {
      await new Promise((resolve) => setImmediate(resolve));
      if (stream.pending.length === 0) return;
    }
    const pending = stream.pending;
    stream.pending = [];
    for (const callback of pending) callback();
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error("manual stream never drained");
}

test("hub log is one private file family with scoped adapters and safe protocol summaries", async () => {
  assert.equal(DEFAULT_HUB_LOG_LIMITS.maxSegmentBytes, 10 * 1024 * 1024);
  assert.equal(DEFAULT_HUB_LOG_LIMITS.maxRotatedBytes, 50 * 1024 * 1024);
  assert.equal(DEFAULT_HUB_LOG_LIMITS.maxRotatedFiles, 5);
  const { root, logDir } = tempHome("log-basic");
  assert.equal(resolveHubLogDir({ stateDir: join(root, "state") }), logDir);
  const logger = createHubLogWriter({ dir: logDir, maxEntryBytes: 8 * 1024 });
  try {
    const adapter = logger.createAgentLogWriter({ agentId: "agent-a", carrier: "claude", source: "rpc" });
    adapter.protocol("in", { id: "1", type: "prompt", message: "sk-live-TOPSECRET", data: { apiKey: "sk-live-TOPSECRET" } });
    adapter.protocol("out", { type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "hello world" }] } });
    adapter.note("started", { pid: 42 });
    adapter.stderr.write("first line\nsecond line\n");
    await adapter.end();
    assert.equal(logger.stats().written, 5);
    await logger.close();
  } finally {
    await logger.close();
  }

  const file = join(logDir, "hub.log");
  assert.equal(statSync(file).mode & 0o777, 0o600);
  assert.equal(statSync(logDir).mode & 0o777, 0o700);
  const lines = readLines(file);
  assert.equal(lines.length, 5);
  for (const line of lines) {
    assert.equal(line.agentId, "agent-a");
    assert.equal(line.carrier, "claude");
    assert.equal(line.source, "rpc");
  }
  const protocol = lines.filter((line) => line.kind === "protocol");
  assert.equal(protocol.length, 2);
  assert.deepEqual(protocol[0].summary.keys.sort(), ["data", "id", "message", "type"]);
  assert.equal(protocol[0].summary.message.role, undefined);
  const raw = readFileSync(file, "utf8");
  assert.equal(raw.includes("TOPSECRET"), false, "default summaries must not contain message text");
  assert.equal(raw.includes("hello world"), false);
  assert.equal(lines.filter((line) => line.kind === "stderr").length, 2);
  rmSync(root, { recursive: true, force: true });
});

test("debug adapters record raw protocol but keep a hard per-entry cap", async () => {
  const { root, logDir } = tempHome("log-debug");
  const logger = createHubLogWriter({ dir: logDir, maxEntryBytes: 512, debug: true });
  try {
    const adapter = createAgentLogWriter(logger, { agentId: "agent-b", carrier: "pi", source: "pi-rpc", debug: true });
    adapter.protocol("in", { id: "2", type: "prompt", message: "x".repeat(4_000), marker: "RAW-MARKER" });
    await adapter.end();
  } finally {
    await logger.close();
  }
  const lines = readLines(join(logDir, "hub.log"));
  assert.equal(lines.length, 1);
  assert.equal(lines[0].kind, "protocol_raw");
  assert.equal(lines[0].truncated, true);
  assert.ok(Buffer.byteLength(readFileSync(join(logDir, "hub.log"))) <= 513);
  rmSync(root, { recursive: true, force: true });
});

test("rotation keeps the current segment and the rotated family inside configured limits", async () => {
  const { root, logDir } = tempHome("log-rotate");
  const logger = createHubLogWriter({
    dir: logDir,
    maxSegmentBytes: 1_024,
    maxRotatedBytes: 2_048,
    maxRotatedFiles: 2,
    maxEntryBytes: 512,
    maxStringChars: 256,
  });
  try {
    for (let index = 0; index < 80; index += 1) {
      assert.equal(logger.write({ kind: "agent_note", message: `entry-${index}-${"y".repeat(180)}` }), true);
    }
    await logger.close();
  } finally {
    await logger.close();
  }
  const names = logFileNames(logDir);
  assert.ok(names.includes("hub.log"));
  const rotated = names.filter((name) => /^hub\.\d+\.log$/.test(name));
  assert.equal(names.some((name) => name.endsWith(".gz")), false, "rotation must not compress");
  assert.ok(rotated.length <= 2, `rotated file count ${rotated.length}`);
  const rotatedBytes = rotated.reduce((total, name) => total + statSync(join(logDir, name)).size, 0);
  assert.ok(rotatedBytes <= 2_048, `rotated bytes ${rotatedBytes}`);
  assert.ok(statSync(join(logDir, "hub.log")).size <= 1_024 + 512);
  for (const name of names) assert.equal(statSync(join(logDir, name)).mode & 0o777, 0o600);
  assert.equal(statSync(join(logDir, ".hub.log.history")).mode & 0o777, 0o600);
  rmSync(root, { recursive: true, force: true });
});

test("backpressure drops diagnostics, keeps counters, and reports a bounded summary", async () => {
  const stream = new ManualStream();
  const logger = createHubLogWriter({
    stream,
    maxEntryBytes: 1_024,
    softLimitBytes: 64,
    hardLimitBytes: 256,
  });
  try {
    for (let index = 0; index < 200; index += 1) {
      logger.write({ kind: "agent_note", message: `entry-${index}-${"z".repeat(80)}` }, { droppable: true });
    }
    const stats = logger.stats();
    assert.ok(stats.dropped.total > 0);
    assert.ok(stats.dropped.byReason.soft_limit > 0 || stats.dropped.byReason.hard_limit > 0);
    assert.ok(stream.writableLength <= 256 + 1_024, `buffered ${stream.writableLength}`);

    await releaseAll(stream);
    const text = stream.chunks.join("");
    assert.ok(text.includes("log_dropped"), "drop summary must be written once the stream recovers");
  } finally {
    await releaseAll(stream).catch(() => {});
    await logger.close();
  }
  assert.equal(logger.stats().error, null);
});

test("stderr adapter splits lines across chunks, decodes multibyte text, and caps partial lines", async () => {
  const { root, logDir } = tempHome("log-stderr");
  const logger = createHubLogWriter({ dir: logDir, maxStderrLineBytes: 200 });
  try {
    const adapter = logger.createAgentLogWriter({ agentId: "agent-c", carrier: "codex", source: "stderr" });
    const encoded = Buffer.from("中文输出\n");
    adapter.stderr.write(encoded.subarray(0, 2));
    adapter.stderr.write(encoded.subarray(2));
    adapter.stderr.write("tail-without-newline");
    adapter.stderr.write("b".repeat(400));
    await adapter.end();
    await adapter.end();
  } finally {
    await logger.close();
  }
  const stderrLines = readLines(join(logDir, "hub.log")).filter((line) => line.kind === "stderr");
  assert.ok(stderrLines.some((line) => line.message === "中文输出"));
  const partial = stderrLines.find((line) => line.data?.truncated === true);
  assert.ok(partial, "oversized partial line must be truncated");
  assert.ok(partial.message.startsWith("tail-without-newlineb"));
  for (const line of stderrLines) assert.equal(line.agentId, "agent-c");
  rmSync(root, { recursive: true, force: true });
});

test("streaming deltas are filtered while final messages stay semantic", () => {
  assert.equal(isEphemeralStreamEvent({ type: "message_update", assistantMessageEvent: { type: "text_delta" } }), true);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "codex", value: { type: "message_update" } }), true);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "claude", value: { type: "text_delta", delta: "x" } }), true);
  assert.equal(isEphemeralStreamEvent({ type: "message_end" }), false);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "codex", value: { type: "message_end" } }), false);
  assert.equal(isEphemeralStreamEvent({ type: "agent_settled" }), false);
  assert.equal(isEphemeralStreamEvent(null), false);
  const summary = summarizeProtocol("in", { type: "prompt", message: "secret-text" });
  assert.equal(JSON.stringify(summary).includes("secret-text"), false);
});

test("partial tool output and codex item.updated count as stream deltas", () => {
  // Pi re-sends the cumulative partial result: progress, not history.
  assert.equal(isEphemeralStreamEvent({ type: "tool_execution_update", toolCallId: "c1", toolName: "bash", partialResult: "x".repeat(4096) }), true);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "pi", value: { type: "tool_execution_update", partialResult: "x" } }), true);
  // Codex streams one item.updated per chunk; only item.completed is semantic.
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "codex", value: { type: "item.updated", item: { type: "agent_message", text: "partial" } } }), true);
  // Boundaries and results stay persisted.
  assert.equal(isEphemeralStreamEvent({ type: "tool_execution_start", toolName: "bash" }), false);
  assert.equal(isEphemeralStreamEvent({ type: "tool_execution_end", toolCallId: "c1", isError: false }), false);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "codex", value: { type: "item.completed", item: { type: "agent_message", text: "final" } } }), false);
  // Unknown semantics are preserved rather than guessed as deltas.
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "codex", value: { type: "turn.started" } }), false);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", carrier: "codex", value: { type: "thread.started", thread_id: "t1" } }), false);
  assert.equal(isEphemeralStreamEvent({ type: "error", message: "boom" }), false);
});

test("carrier_event unwrapping follows real nesting, stays bounded, and survives cycles", () => {
  const wrap = (depth, inner) => {
    let value = inner;
    for (let index = 0; index < depth; index += 1) value = { type: "carrier_event", carrier: "codex", value };
    return value;
  };
  assert.equal(isEphemeralStreamEvent(wrap(1, { type: "message_update" })), true);
  assert.equal(isEphemeralStreamEvent(wrap(2, { type: "message_update" })), true);
  assert.equal(isEphemeralStreamEvent(wrap(3, { type: "message_update" })), true);
  assert.equal(isEphemeralStreamEvent(wrap(5, { type: "item.updated" })), true);
  assert.equal(isEphemeralStreamEvent(wrap(31, { type: "text_delta" })), true);
  // Beyond the bound the walk stops: fail closed to "persist" instead of guessing.
  assert.equal(isEphemeralStreamEvent(wrap(40, { type: "message_update" })), false);
  assert.equal(isEphemeralStreamEvent(wrap(3, { type: "message_end" })), false);

  const cyclic = { type: "carrier_event", carrier: "codex" };
  cyclic.value = cyclic;
  assert.equal(isEphemeralStreamEvent(cyclic), false);

  const twoNodeCycle = { type: "carrier_event", value: { type: "carrier_event" } };
  twoNodeCycle.value.value = twoNodeCycle;
  assert.equal(isEphemeralStreamEvent(twoNodeCycle), false);

  // Non-object and wrapper-only chains terminate without throwing.
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event", value: "message_update" }), false);
  assert.equal(isEphemeralStreamEvent({ type: "carrier_event" }), false);
  assert.equal(isEphemeralStreamEvent(undefined), false);
});

function streamingPiProgram(dir) {
  const path = join(dir, "fake-streaming-pi.js");
  writeFileSync(path, `#!/usr/bin/env node
let buffer = "";
const emit = (value) => process.stdout.write(JSON.stringify(value) + "\\n");
const marker = "PARTIAL_STREAM_MARKER";
function handle(command) {
  if (command.type === "get_state") {
    emit({ id: command.id, type: "response", command: "get_state", success: true, data: {
      model: null, thinkingLevel: "off", isStreaming: false, sessionFile: null, sessionId: "stream-session", messageCount: 0,
    } });
    return;
  }
  if (command.type === "prompt") {
    emit({ id: command.id, type: "response", command: "prompt", success: true });
    emit({ type: "agent_start" });
    for (let index = 0; index < 30; index += 1) {
      emit({ type: "tool_execution_update", toolCallId: "call-1", toolName: "bash",
        args: { command: "cat big-file" }, partialResult: marker.repeat(200) + ":" + index });
    }
    emit({ type: "tool_execution_end", toolCallId: "call-1", toolName: "bash", isError: false, result: "ok" });
    for (let index = 0; index < 10; index += 1) {
      emit({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: marker } });
    }
    emit({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "final answer" }] } });
    emit({ type: "agent_settled" });
    return;
  }
  emit({ id: command.id, type: "response", command: command.type, success: true, data: {} });
}
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (line.trim()) handle(JSON.parse(line));
  }
});
process.stdin.on("end", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
`);
  chmodSync(path, 0o700);
  return path;
}

test("repeated partial tool output consumes no event cursor while final events stay queryable", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-stream-persist-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = streamingPiProgram(root);
  ensureHubLayout(paths);
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({ paths, store, profile });
  try {
    await manager.createAgent({ agentId: "agent-stream", name: "stream", cwd: root });
    const cursorBefore = store.latestCursor();
    await manager.send("agent-stream", "prompt", "go");

    const deadline = Date.now() + 5_000;
    let events = [];
    while (Date.now() < deadline) {
      events = store.listEvents({ agentId: "agent-stream", after: cursorBefore });
      if (events.some((event) => event.event_type === "agent_settled")) break;
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 20));
    }

    const types = events.map((event) => event.event_type);
    assert.ok(types.includes("agent_settled"), "the turn must settle");
    // 30 cumulative tool_execution_update + 10 message_update frames must not persist.
    assert.equal(types.includes("tool_execution_update"), false, "partial tool output must not consume a cursor");
    assert.equal(types.includes("message_update"), false);
    assert.ok(types.includes("tool_execution_end"), "the tool boundary stays semantic");
    assert.ok(types.includes("message_end"), "the final message stays semantic");

    // Cursor advance equals the number of persisted events: no hidden increments.
    assert.equal(store.latestCursor() - cursorBefore, events.length);
    assert.ok(events.length < 15, `expected only semantic events, got ${events.length}`);

    const toolEnd = events.find((event) => event.event_type === "tool_execution_end");
    assert.equal(toolEnd.payload.isError, false);
    assert.equal(toolEnd.payload.toolName, "bash");
    const messageEnd = events.find((event) => event.event_type === "message_end");
    assert.equal(messageEnd.payload.message.content[0].text, "final answer");

    // The diagnostic log keeps summaries only: no cumulative payload text.
    const logPath = join(paths.stateDir, "logs", "hub.log");
    assert.equal(existsSync(logPath), true);
    const logText = readFileSync(logPath, "utf8");
    assert.equal(logText.includes("PARTIAL_STREAM_MARKER"), false, "stream payload must not be copied into the log");
    assert.ok(statSync(logPath).size < 64 * 1024, `log should stay small, got ${statSync(logPath).size} bytes`);
  } finally {
    await manager.stopAll();
    await manager.closeLogs();
    store.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("adapter interface rejects missing agentId and never owns a file", () => {
  const stream = new ManualStream();
  const logger = createHubLogWriter({
    stream,
    maxEntryBytes: 1_024,
    softLimitBytes: 4_096,
    hardLimitBytes: 8_192,
  });
  try {
    assert.throws(() => createAgentLogWriter(logger, {}), /agentId/);
    const adapter = logger.createAgentLogWriter({ agentId: "agent-d", carrier: "pi", source: "agent" });
    assert.equal(adapter.agentId, "agent-d");
    assert.equal(adapter.stderr.writable, true);
    assert.equal(logger.filePath, null);
  } finally {
    void logger.close();
  }
});

test("existing databases gain prompt_config_json in place, keeping rows and events", () => {
  const { root } = tempHome("store-migrate");
  const path = join(root, "hub.sqlite");
  const now = new Date().toISOString();
  const legacy = new DatabaseSync(path);
  legacy.exec(`
    CREATE TABLE agents (
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
      carrier TEXT NOT NULL DEFAULT 'pi',
      skill_authorization_json TEXT,
      last_error TEXT,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
    CREATE TABLE events (
      cursor INTEGER PRIMARY KEY AUTOINCREMENT,
      event_id TEXT NOT NULL UNIQUE,
      agent_id TEXT,
      event_type TEXT NOT NULL,
      payload_json TEXT NOT NULL,
      created_at TEXT NOT NULL
    );
  `);
  legacy.prepare(`
    INSERT INTO agents VALUES (
      'legacy-agent', 'legacy', @cwd, 'shared', NULL, NULL, NULL, 'idle', 'stopped', NULL,
      'session-legacy', NULL, @sessionDir, @agentDir, @logsDir, 'sha256:legacy',
      '["read"]', '[]', '[]', 'pi', NULL, NULL, @now, @now
    )
  `).run({
    cwd: root,
    sessionDir: join(root, "sessions"),
    agentDir: join(root, "agent"),
    logsDir: join(root, "logs"),
    now,
  });
  legacy.prepare("INSERT INTO events (event_id, agent_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)")
    .run("legacy-event", "legacy-agent", "agent_ready", '{"pid":1}', now);
  legacy.close();

  const store = new HubStore(path);
  try {
    const agent = store.getAgent("legacy-agent");
    assert.equal(agent.prompt_config_json, "{}");
    assert.deepEqual(agent.tools, ["read"]);
    assert.equal(store.latestCursor(), 1);
    assert.equal(store.listEvents({ agentId: "legacy-agent" }).length, 1);

    const frozen = JSON.stringify({ schemaVersion: 1, mode: "none", prompt: "", digest: "sha256:x", principleIds: [], resolvedAt: now });
    store.createAgent({
      id: "new-agent", name: "new", cwd: root, workspace_mode: "shared", provider: null, model: null, thinking: null,
      status: "idle", desired_state: "running", pid: null, session_id: "session-new", session_file: null,
      session_dir: join(root, "sessions"), agent_dir: join(root, "agent"), logs_dir: join(root, "logs"),
      profile_digest: "sha256:test", tools_json: "[]", skills_json: "[]", extensions_json: "[]",
      prompt_config_json: frozen, last_error: null, created_at: now, updated_at: now,
    });
    assert.equal(store.getAgent("new-agent").prompt_config_json, frozen);
    store.close();
    const reopened = new HubStore(path);
    try {
      assert.equal(reopened.getAgent("new-agent").prompt_config_json, frozen);
      assert.equal(reopened.getAgent("legacy-agent").name, "legacy");
    } finally {
      reopened.close();
    }
  } finally {
    try { store.close(); } catch {}
    rmSync(root, { recursive: true, force: true });
  }
});
