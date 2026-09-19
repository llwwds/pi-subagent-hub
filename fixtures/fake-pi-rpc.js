let buffer = Buffer.alloc(0);

function emit(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

function handle(command) {
  if (command.type === "get_state") {
    emit({ id: command.id, type: "response", command: "get_state", success: true, data: {
      model: null,
      thinkingLevel: "off",
      isStreaming: false,
      sessionFile: null,
      sessionId: "fake-session",
      messageCount: 0,
    } });
    return;
  }
  if (command.type === "prompt" || command.type === "steer" || command.type === "follow_up") {
    emit({ id: command.id, type: "response", command: command.type, success: true });
    emit({ type: "agent_start" });
    emit({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: `fake ${command.message}` } });
    setTimeout(() => emit({ type: "agent_settled" }), 10);
    return;
  }
  emit({ id: command.id, type: "response", command: command.type, success: true, data: {} });
}

process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  let index;
  while ((index = buffer.indexOf(0x0a)) !== -1) {
    let line = buffer.subarray(0, index);
    buffer = buffer.subarray(index + 1);
    if (line.at(-1) === 0x0d) line = line.subarray(0, -1);
    if (line.length > 0) handle(JSON.parse(line.toString("utf8")));
  }
});
process.stdin.on("end", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
