import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  CarrierRegistry,
  PiCarrierAdapter,
  createDefaultCarrierRegistry,
} from "../src/carriers/index.js";

function fixture(root, name) {
  const path = join(root, name);
  const program = `#!${process.execPath}\n` + String.raw`
const { appendFileSync } = require("node:fs");
const args = process.argv.slice(2);
let prompt = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => { prompt += chunk; });
process.stdin.on("end", () => {
  appendFileSync(process.env.FAKE_ARGS_PATH, JSON.stringify({ args, prompt, cwd: process.cwd(), home: process.env.HOME, codexHome: process.env.CODEX_HOME, claudeHome: process.env.CLAUDE_CONFIG_DIR }) + "\n");
  if (prompt === "hang") return setInterval(() => {}, 1000);
  if (prompt === "empty") return;
  if (process.env.FAKE_KIND === "codex") {
    console.log(JSON.stringify({ type: "thread.started", thread_id: "11111111-1111-4111-8111-111111111111" }));
    console.log(JSON.stringify({ type: "item.completed", item: { type: "agent_message", text: "codex:" + prompt } }));
    console.log(JSON.stringify({ type: "turn.completed" }));
  } else {
    console.log(JSON.stringify({ type: "system", subtype: "init", session_id: "22222222-2222-4222-8222-222222222222" }));
    console.log(JSON.stringify({ type: "result", session_id: "22222222-2222-4222-8222-222222222222", result: "claude:" + prompt, is_error: false }));
  }
});
`;
  writeFileSync(path, program, { mode: 0o700 });
  chmodSync(path, 0o700);
  return path;
}

function runtimeSpec(root, skills = []) {
  for (const directory of ["state", "logs"]) mkdirSync(join(root, directory));
  return {
    id: "agent-test",
    name: "test",
    cwd: root,
    stateDir: join(root, "state"),
    logsDir: join(root, "logs"),
    sessionId: "33333333-3333-4333-8333-333333333333",
    model: null,
    provider: null,
    thinking: null,
    skills,
  };
}

function untilEvent(runtime, type) {
  return new Promise((resolvePromise, reject) => {
    const timeout = setTimeout(() => {
      runtime.off("event", listener);
      reject(new Error(`timed out waiting for ${type}`));
    }, 15000);
    const listener = (event) => {
      if (event.type !== type) return;
      clearTimeout(timeout);
      runtime.off("event", listener);
      resolvePromise(event);
    };
    runtime.on("event", listener);
  });
}

test("registry validates adapters and diagnoses all built-in carriers without executing them", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-carriers-registry-"));
  try {
    const registry = createDefaultCarrierRegistry({
      codexCliPath: fixture(root, "fake-codex"),
      claudeCliPath: join(root, "missing-claude"),
    });
    assert.deepEqual(registry.list().map((item) => item.id), ["pi", "codex", "claude"]);
    assert.equal((await registry.diagnose("codex")).available, true);
    assert.match((await registry.diagnose("claude")).reason, /unavailable/);
    const presentClaude = createDefaultCarrierRegistry({ claudeCliPath: fixture(root, "fake-claude") });
    assert.equal((await presentClaude.diagnose("claude", { env: { PATH: "" } })).available, true);
    assert.equal((await registry.diagnose("pi", { paths: { piCliPath: resolve("fixtures/fake-pi-rpc.js") } })).available, true);
    assert.equal(registry.get("codex").capabilities.skillLoadingSelection, true);
    assert.equal(registry.get("codex").capabilities.skillIsolation, false);
    assert.throws(() => registry.register(new PiCarrierAdapter()), /already registered/);
    assert.throws(() => registry.get("missing"), /unknown carrier/);
    const extra = new CarrierRegistry();
    assert.throws(() => extra.register({ id: "bad id" }), /lowercase id/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

for (const carrier of ["codex", "claude"]) {
  test(`${carrier} CLI runtime accepts prompts, resumes its private session, and emits normalized results`, async () => {
    const root = mkdtempSync(join(tmpdir(), `pihub-carriers-${carrier}-`));
    try {
      const cliPath = fixture(root, `fake-${carrier}`);
      const skillDir = join(root, "gallery-skill");
      mkdirSync(skillDir);
      writeFileSync(join(skillDir, "SKILL.md"), "---\nname: test-skill\ndescription: test\n---\n");
      const spec = runtimeSpec(root, [{ id: "test-skill", path: skillDir }]);
      const argsPath = join(root, "args.jsonl");
      const registry = createDefaultCarrierRegistry({ codexCliPath: cliPath, claudeCliPath: cliPath });
      const runtime = registry.createRuntime(carrier, {
        spec,
        env: { ...process.env, FAKE_ARGS_PATH: argsPath, FAKE_KIND: carrier },
      });
      const messages = [];
      runtime.on("event", (event) => { if (event.type === "message_end") messages.push(event); });
      const state = await runtime.start();
      assert.equal(state.isStreaming, false);
      assert.equal(runtime.alive, true);
      for (const prompt of ["first", "second"]) {
        const settled = untilEvent(runtime, "agent_settled");
        const response = await runtime.request("prompt", { message: prompt });
        assert.equal(response.success, true);
        await settled;
      }
      assert.deepEqual(messages.map((event) => event.message.content[0].text), [`${carrier}:first`, `${carrier}:second`]);
      const calls = readFileSync(argsPath, "utf8").trim().split("\n").map(JSON.parse);
      assert.equal(calls.length, 2);
      assert.equal(calls[0].prompt, "first");
      assert.equal(calls[1].prompt, "second");
      assert.ok(calls.every((call) => call.home.startsWith(spec.stateDir)));
      if (carrier === "codex") {
        assert.equal(realpathSync(calls[0].cwd), realpathSync(calls[0].home));
        assert.ok(calls[0].args.includes("--add-dir"));
        assert.equal(calls[0].args[calls[0].args.indexOf("--add-dir") + 1], spec.cwd);
        assert.match(readFileSync(join(calls[0].home, "AGENTS.md"), "utf8"), /user-selected project directory/);
        assert.ok(calls[0].args.includes("exec"));
        assert.ok(calls[1].args.includes("resume"));
        assert.ok(calls[1].args.includes("11111111-1111-4111-8111-111111111111"));
        assert.ok(calls[0].codexHome.startsWith(spec.stateDir));
        assert.ok(existsSync(join(calls[0].home, ".agents", "skills", "test-skill", "SKILL.md")));
      } else {
        assert.equal(calls[0].args[calls[0].args.indexOf("--permission-mode") + 1], "dontAsk");
        const approvedTools = calls[0].args[calls[0].args.indexOf("--allowedTools") + 1].split(",");
        assert.ok(["Read", "Edit", "Write", "Bash", "Skill"].every((tool) => approvedTools.includes(tool)));
        assert.ok(calls[0].args.includes("--session-id"));
        assert.ok(calls[1].args.includes("--resume"));
        assert.ok(calls[1].args.includes("22222222-2222-4222-8222-222222222222"));
        assert.ok(calls[0].claudeHome.startsWith(spec.stateDir));
        const addedDirectory = calls[0].args[calls[0].args.indexOf("--add-dir") + 1];
        assert.ok(existsSync(join(addedDirectory, ".claude", "skills", "test-skill", "SKILL.md")));
        assert.notEqual(addedDirectory, calls[0].claudeHome);
      }
      await assert.rejects(runtime.request("steer", { message: "x" }), /does not support steer/);
      await runtime.stop();
      assert.equal(runtime.alive, false);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}

test("CLI carrier aborts an active turn without ending the resumable runtime", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-carriers-abort-"));
  try {
    const cliPath = fixture(root, "fake-codex");
    const spec = runtimeSpec(root);
    const runtime = createDefaultCarrierRegistry({ codexCliPath: cliPath }).createRuntime("codex", {
      spec,
      env: { ...process.env, FAKE_ARGS_PATH: join(root, "args.jsonl"), FAKE_KIND: "codex" },
    });
    await runtime.start();
    const settled = untilEvent(runtime, "agent_settled");
    await runtime.request("prompt", { message: "hang" });
    await runtime.request("abort");
    assert.equal((await settled).aborted, true);
    assert.equal(runtime.alive, true);
    await runtime.stop();
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("CLI carrier fails a turn that exits without a final result", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-carriers-empty-"));
  try {
    const cliPath = fixture(root, "fake-codex");
    const runtime = createDefaultCarrierRegistry({ codexCliPath: cliPath }).createRuntime("codex", {
      spec: runtimeSpec(root),
      env: { ...process.env, FAKE_ARGS_PATH: join(root, "args.jsonl"), FAKE_KIND: "codex" },
    });
    await runtime.start();
    const failed = new Promise((resolvePromise) => runtime.once("process_error", resolvePromise));
    await runtime.request("prompt", { message: "empty" });
    assert.match((await failed).message, /no final result/);
    assert.equal(runtime.alive, false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("CC Switch launches Codex and Claude with private routes and no inherited API key", async () => {
  for (const carrier of ["codex", "claude"]) {
    const root = mkdtempSync(join(tmpdir(), `pihub-switch-runtime-${carrier}-`));
    try {
      const records = join(root, "switch-calls.jsonl");
      const switchPath = join(root, "fake-cc-switch");
      const code = `#!${process.execPath}\n` + `const fs = require("node:fs");\n` +
        `const args = process.argv.slice(2);\n` +
        `fs.appendFileSync(${JSON.stringify(records)}, JSON.stringify({args, home: process.env.HOME, hubHome: process.env.PIHUB_HOME, pathHead: process.env.PATH.split(":")[0], configDir: process.env.CC_SWITCH_CONFIG_DIR, codexHome: process.env.CODEX_HOME, claudeHome: process.env.CLAUDE_CONFIG_DIR, key: process.env.ANTHROPIC_API_KEY}) + "\\n");\n` +
        `process.stdin.resume(); process.stdin.on("end", () => {\n` +
        `if (args[1] === "codex") { console.log(JSON.stringify({type:"thread.started",thread_id:"11111111-1111-4111-8111-111111111111"})); console.log(JSON.stringify({type:"item.completed",item:{type:"agent_message",text:"codex ok"}})); console.log(JSON.stringify({type:"turn.completed"})); }\n` +
        `else { console.log(JSON.stringify({type:"system",subtype:"init",session_id:"22222222-2222-4222-8222-222222222222"})); console.log(JSON.stringify({type:"result",session_id:"22222222-2222-4222-8222-222222222222",result:"claude ok",is_error:false})); }\n` +
        `});\n`;
      writeFileSync(switchPath, code, { mode: 0o700 });
      const spec = runtimeSpec(root);
      spec.provider = "route-a";
      spec.hubHome = join(root, "development");
      spec.hubCliPath = fixture(root, "fake-platform-pihub");
      spec.tmpDir = join(root, "tmp");
      spec.ccSwitch = {
        executable: switchPath,
        configDir: join(root, "cc-switch", "data"),
        home: join(root, "cc-switch", "home"),
        piDir: join(root, "cc-switch", "pi-agent"),
        codexCliPath: switchPath,
        claudeCliPath: switchPath,
      };
      const runtime = createDefaultCarrierRegistry({ codexCliPath: switchPath, claudeCliPath: switchPath }).createRuntime(carrier, {
        spec, env: { ...process.env, ANTHROPIC_API_KEY: "must-not-inherit" },
      });
      await runtime.start();
      const settled = untilEvent(runtime, "agent_settled");
      await runtime.request("prompt", { message: "hello" });
      await settled;
      const call = JSON.parse(readFileSync(records, "utf8").trim());
      assert.deepEqual(call.args.slice(0, 3), ["start", carrier, "route-a"]);
      assert.equal(call.key, undefined);
      assert.equal(call.hubHome, spec.hubHome);
      assert.equal(call.pathHead, join(call.home, ".bin"));
      assert.equal(realpathSync(join(call.pathHead, "pihub")), realpathSync(spec.hubCliPath));
      assert.equal(call.configDir, spec.ccSwitch.configDir);
      assert.ok(call.home.startsWith(spec.stateDir));
      assert.ok(call.codexHome.startsWith(spec.stateDir));
      assert.ok(call.claudeHome.startsWith(spec.stateDir));
      if (carrier === "codex") assert.ok(call.args.includes("--shared-sessions"));
      else assert.ok(call.args.includes("--bare"));
      await runtime.stop();
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
});

test("platform Pi carrier routes bundled pihub skill commands to its own home", () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-pi-private-route-"));
  try {
    const appRoot = join(root, "source");
    const binDir = join(appRoot, "bin");
    mkdirSync(appRoot);
    mkdirSync(binDir);
    const hubCliPath = fixture(binDir, "pihub.js");
    const spec = runtimeSpec(root);
    const paths = { piCliPath: fixture(root, "fake-pi"), appRoot, home: join(root, "development") };
    const runtime = new PiCarrierAdapter().createRuntime({ paths, spec, env: { PATH: "/host/v1/bin:/usr/bin" } });
    assert.equal(runtime.baseEnv.PIHUB_HOME, paths.home);
    const firstPath = runtime.baseEnv.PATH.split(":")[0];
    assert.equal(firstPath, join(spec.stateDir, "pi-home", ".bin"));
    assert.equal(realpathSync(join(firstPath, "pihub")), realpathSync(hubCliPath));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
