import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { PiRpcProcess } from "../src/rpc-process.js";

test("RPC process reaches readiness, correlates commands, and preserves event isolation", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-rpc-"));
  const directories = ["sessions", "agent", "logs", "state", "tmp"];
  for (const directory of directories) mkdirSync(join(root, directory));
  const runtime = new PiRpcProcess({
    piCliPath: resolve("fixtures/fake-pi-rpc.js"),
    spec: {
      id: "agent-one",
      name: "agent one",
      cwd: root,
      sessionDir: join(root, "sessions"),
      sessionId: "00000000-0000-4000-8000-000000000001",
      agentDir: join(root, "agent"),
      logsDir: join(root, "logs"),
      stateDir: join(root, "state"),
      tmpDir: join(root, "tmp"),
      tools: ["read"],
      skills: [],
      extensions: [],
      provider: null,
      model: null,
      thinking: null,
    },
  });
  const events = [];
  runtime.on("event", (event) => events.push(event));
  const state = await runtime.start();
  assert.equal(state.sessionId, "fake-session");
  const response = await runtime.request("prompt", { message: "hello" });
  assert.equal(response.success, true);
  await new Promise((resolvePromise) => setTimeout(resolvePromise, 30));
  assert.ok(events.some((event) => event.type === "agent_settled"));
  assert.ok(events.some((event) => event.assistantMessageEvent?.delta === "fake hello"));
  await runtime.stop();
  rmSync(root, { recursive: true, force: true });
});
