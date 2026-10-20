//! Fake Pi RPC peer used by `tests/pi_rpc.rs`.
//!
//! Speaks the same NDJSON protocol as the pinned Pi bundle for the verbs the
//! runtime uses, and records every received command into
//! `<--session-dir>/commands.jsonl` so a test can assert argv correlation
//! without reading any environment variable (the runtime deliberately strips
//! them).

import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const args = process.argv.slice(2);
const sessionDirIndex = args.indexOf("--session-dir");
const sessionDir = sessionDirIndex >= 0 ? args[sessionDirIndex + 1] : null;
const logPath = sessionDir ? join(sessionDir, "commands.jsonl") : null;
if (sessionDir) mkdirSync(sessionDir, { recursive: true });

let lastPrompt = "";
let buffer = "";

function record(command) {
  if (!logPath) return;
  appendFileSync(logPath, `${JSON.stringify({ ...command, _argv: args })}\n`);
}

function emit(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

function handle(command) {
  record(command);
  if (command.type === "get_state") {
    emit({
      id: command.id,
      type: "response",
      command: "get_state",
      success: true,
      data: {
        model: { provider: "fake", id: "fake-model" },
        thinkingLevel: "off",
        isStreaming: false,
        sessionFile: null,
        sessionId: "fake-session",
        messageCount: 0,
      },
    });
    return;
  }
  if (command.type === "prompt" || command.type === "steer" || command.type === "follow_up") {
    lastPrompt = command.message || "";
    if (lastPrompt === "crash") {
      // Exit before responding: the runtime must reject the pending request.
      process.exit(1);
    }
    if (lastPrompt === "garbage") {
      process.stdout.write("this is not json\n");
    }
    emit({ id: command.id, type: "response", command: command.type, success: true });
    emit({ type: "agent_start" });
    emit({
      type: "message_update",
      assistantMessageEvent: { type: "text_delta", delta: `fake ${lastPrompt}` },
    });
    emit({
      type: "message_end",
      message: { role: "assistant", content: [{ type: "text", text: `fake ${lastPrompt}` }] },
    });
    setTimeout(() => emit({ type: "agent_settled" }), 10);
    return;
  }
  if (command.type === "abort") {
    // A hang turn leaves abort unanswered so the request timeout is exercised.
    if (lastPrompt === "hang") return;
    emit({ id: command.id, type: "response", command: "abort", success: true, data: {} });
    return;
  }
  emit({ id: command.id, type: "response", command: command.type, success: true, data: {} });
}

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (!line) continue;
    handle(JSON.parse(line));
  }
});
process.stdin.on("end", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
