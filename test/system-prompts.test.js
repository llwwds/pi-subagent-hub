import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { AgentManager } from "../src/manager.js";
import { ensureHubLayout, resolveHubPaths } from "../src/paths.js";
import {
  DEFAULT_PROMPT_PRINCIPLE_IDS,
  carrierPromptSupport,
  listPromptPrinciples,
  promptLaunchArgs,
  readFrozenPromptConfig,
  resolvePromptConfig,
} from "../src/prompts.js";
import { resolveProfile } from "../src/profile.js";
import { HubStore } from "../src/store.js";

// Records the exact argv the manager passed, then speaks the minimal Pi RPC the
// manager needs. Never starts a model; used with a temporary isolated home.
function fakePiProcess(root) {
  const path = join(root, "fake-pi.js");
  const program = `#!${process.execPath}\n` + String.raw`
const { appendFileSync } = require("node:fs");
const args = process.argv.slice(2);
appendFileSync(process.env.FAKE_ARGV_PATH, JSON.stringify(args) + "\n");
let buffer = "";
const emit = (value) => process.stdout.write(JSON.stringify(value) + "\n");
function handle(command) {
  if (command.type === "get_state") {
    emit({ id: command.id, type: "response", command: "get_state", success: true, data: {
      model: { provider: "fake", id: "fake-model" }, thinkingLevel: "off", isStreaming: false,
      sessionFile: null, sessionId: "fake-session", messageCount: 0,
    } });
    return;
  }
  emit({ id: command.id, type: "response", command: command.type, success: true });
  if (command.type === "prompt") { emit({ type: "agent_start" }); emit({ type: "agent_settled" }); }
}
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (line.trim()) handle(JSON.parse(line));
  }
});
process.stdin.on("end", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
`;
  writeFileSync(path, program);
  chmodSync(path, 0o700);
  return path;
}

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "pihub-prompts-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = fakePiProcess(root);
  ensureHubLayout(paths);
  const argvPath = join(root, "argv.jsonl");
  const previousArgvPath = process.env.FAKE_ARGV_PATH;
  process.env.FAKE_ARGV_PATH = argvPath;
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({ paths, store, profile });
  const argvRuns = () => (existsSync(argvPath)
    ? readFileSync(argvPath, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line))
    : []);
  const cleanup = async () => {
    // Deterministic handle release: the Hub log stream must not outlive the test.
    try { await manager.closeLogs(); } catch {}
    if (previousArgvPath === undefined) delete process.env.FAKE_ARGV_PATH;
    else process.env.FAKE_ARGV_PATH = previousArgvPath;
    try { store.close(); } catch {}
    // Frozen Skill snapshots are read-only; thaw before removal.
    const thaw = (dir) => {
      try { chmodSync(dir, 0o700); } catch { return; }
      for (const entry of readdirSync(dir, { withFileTypes: true })) if (entry.isDirectory()) thaw(join(dir, entry.name));
    };
    thaw(root);
    rmSync(root, { recursive: true, force: true });
  };
  return { root, paths, store, manager, argvRuns, cleanup };
}

function appendedPrompt(args) {
  const index = args.indexOf("--append-system-prompt");
  return index === -1 ? null : args[index + 1];
}

test("presets four coding principles and appends them by default", () => {
  const catalog = listPromptPrinciples();
  assert.deepEqual(catalog.map((principle) => principle.id), [...DEFAULT_PROMPT_PRINCIPLE_IDS]);
  assert.equal(catalog.length, 4);

  const frozen = resolvePromptConfig(undefined, { carrier: "pi", resolvedAt: "2026-01-01T00:00:00.000Z" });
  assert.equal(frozen.mode, "append");
  assert.deepEqual(frozen.principleIds, [...DEFAULT_PROMPT_PRINCIPLE_IDS]);
  assert.equal(frozen.customText, "");
  assert.match(frozen.digest, /^sha256:[a-f0-9]{64}$/);
  for (const principle of listPromptPrinciples()) assert.ok(frozen.prompt.includes(principle.title));
  assert.match(frozen.prompt, /只做追加/);

  const repeated = resolvePromptConfig({}, { carrier: "pi", resolvedAt: "2026-01-01T00:00:00.000Z" });
  assert.equal(repeated.digest, frozen.digest, "digest must be stable for the same resolved input");

  const args = promptLaunchArgs("pi", frozen);
  assert.deepEqual(args, ["--append-system-prompt", frozen.prompt]);
});

test("supports deselecting everything, combining subsets, and custom multi-line text", () => {
  const none = resolvePromptConfig({ principleIds: [], customText: "" });
  assert.equal(none.mode, "none");
  assert.equal(none.prompt, "");
  assert.deepEqual(promptLaunchArgs("pi", none), []);

  const subset = resolvePromptConfig({ principleIds: ["no-unnecessary-entities", "first-principles"] });
  assert.deepEqual(subset.principleIds, ["first-principles", "no-unnecessary-entities"], "canonical order");

  const custom = resolvePromptConfig({ principleIds: [], customText: "line one\r\nline two\r\n" });
  assert.equal(custom.customText, "line one\nline two");
  assert.ok(custom.prompt.includes("line one\nline two"));
  assert.ok(!custom.prompt.includes("工作原则"));

  assert.throws(() => resolvePromptConfig({ principleIds: ["nope"] }), /unknown prompt principle/);
  assert.throws(() => resolvePromptConfig({ customText: "x".repeat(8001) }), /at most 8000 characters/);
  assert.throws(() => resolvePromptConfig({ customText: 42 }), /must be a string/);
});

test("each carrier uses its real additive channel and no carrier is rejected by default", () => {
  // Pi and Claude append to the system prompt; Codex adds developer instructions.
  assert.equal(carrierPromptSupport("pi"), "append");
  assert.equal(carrierPromptSupport("claude"), "append");
  assert.equal(carrierPromptSupport("codex"), "developer");
  assert.equal(carrierPromptSupport("unknown-carrier"), null);

  const piArgs = promptLaunchArgs("pi", resolvePromptConfig(undefined, { carrier: "pi" }));
  assert.equal(piArgs[0], "--append-system-prompt");
  const claudeArgs = promptLaunchArgs("claude", resolvePromptConfig(undefined, { carrier: "claude" }));
  assert.equal(claudeArgs[0], "--append-system-prompt");

  const codexFrozen = resolvePromptConfig(undefined, { carrier: "codex" });
  assert.equal(codexFrozen.mode, "append");
  assert.equal(codexFrozen.channel, "developer");
  assert.equal(codexFrozen.principleIds.length, 4, "default payloads inject for Codex too");
  const codexArgs = promptLaunchArgs("codex", codexFrozen);
  assert.equal(codexArgs.length, 2);
  assert.equal(codexArgs[0], "-c");
  const prefix = "developer_instructions=";
  assert.ok(codexArgs[1].startsWith(prefix));
  // The -c value is TOML-parsed; a JSON string is valid TOML and round-trips.
  assert.equal(JSON.parse(codexArgs[1].slice(prefix.length)), codexFrozen.prompt);

  const multiline = resolvePromptConfig({ principleIds: [], customText: '第一行\n第二行 "q" \\ end' }, { carrier: "codex" });
  assert.equal(JSON.parse(promptLaunchArgs("codex", multiline)[1].slice(prefix.length)), multiline.prompt);

  // Never a replace-style flag, on any carrier.
  for (const carrier of ["pi", "claude", "codex"]) {
    const frozen = resolvePromptConfig(undefined, { carrier });
    assert.ok(!promptLaunchArgs(carrier, frozen).join(" ").includes("--system-prompt"));
  }

  // A carrier with no channel rejects explicit injection but stays creatable.
  assert.throws(() => promptLaunchArgs("unknown-carrier", { mode: "append", prompt: "x" }), /cannot inject/);
});

test("codex agent creation injects developer instructions and records the channel", { timeout: 20000 }, async () => {
  const { root, store, manager, cleanup } = fixture();
  const runtimes = [];
  try {
    const wired = new AgentManager({
      paths: manager.paths,
      store,
      profile: manager.profile,
      skillRegistry: { resolveAuthorization: () => ({ mode: "all", grants: [], digest: "sha256:test" }), verifyAuthorization: () => true },
      carrierRegistry: {
        diagnose: async () => ({ available: true }),
        get: () => ({ capabilities: { systemPrompt: null, skillLoadingSelection: true } }),
        createRuntime: (id, context) => {
          runtimes.push(context.spec.promptConfig);
          return {
            pid: 5150,
            alive: true,
            on() {},
            start: async () => ({ model: { provider: "fake", id: "fake" }, thinkingLevel: "off", isStreaming: false, sessionId: "fake", sessionFile: null }),
            request: async (type) => ({ id: "1", type: "response", command: type, success: true, data: {} }),
            stop: async () => {},
          };
        },
      },
    });
    const created = await wired.createPlatformAgent({ agentId: "agent-codex", name: "codex", cwd: root, carrier: "codex" });
    const frozen = wired.promptConfigOf(created);
    assert.equal(frozen.mode, "append");
    assert.equal(frozen.channel, "developer");
    assert.equal(frozen.principleIds.length, 4);
    assert.equal(runtimes.length, 1);
    assert.equal(runtimes[0].channel, "developer", "runtime receives the frozen developer-channel config");

    const explicit = await wired.createPlatformAgent({
      agentId: "agent-codex-2", name: "codex2", cwd: root, carrier: "codex",
      promptConfig: { principleIds: [], customText: "do more" },
    });
    const explicitFrozen = wired.promptConfigOf(explicit, { includeText: true });
    assert.equal(explicitFrozen.principleIds.length, 0);
    assert.equal(explicitFrozen.customTextChars, 7);
    assert.match(explicitFrozen.prompt, /do more/);
    await wired.stopAll();
    await wired.closeLogs();
  } finally {
    await cleanup();
  }
});

test("injects the frozen prompt as one argv element and freezes it across restarts", async () => {
  const { root, paths, store, manager, argvRuns, cleanup } = fixture();
  try {
    const marker = join(root, "shell-marker");
    const customText = `第一行\n第二行 $(touch ${marker}) \`touch ${marker}\` "quoted"`;
    const created = await manager.createAgent({
      agentId: "agent-prompt",
      name: "prompt",
      cwd: root,
      promptConfig: { principleIds: ["first-principles", "high-cohesion-low-coupling"], customText },
    });
    const firstRun = argvRuns()[0];
    const injected = appendedPrompt(firstRun);
    assert.ok(injected, "Pi must receive --append-system-prompt");
    assert.ok(injected.includes("第一性原理"));
    assert.ok(!injected.includes("如无必要勿增实体"), "unselected principle must not be injected");
    assert.ok(injected.includes(`第二行 $(touch ${marker})`), "shell metacharacters stay literal");
    assert.ok(!existsSync(marker), "prompt text must never be executed");

    const frozenFromFile = readFrozenPromptConfig(readFileSync(join(paths.agentsDir, "agent-prompt", "state", "prompt-config.json"), "utf8"));
    assert.equal(frozenFromFile.digest, manager.promptConfigOf(created).digest);
    assert.deepEqual(manager.promptConfigOf(created).principleIds, ["first-principles", "high-cohesion-low-coupling"]);

    const detail = manager.promptConfigOf(store.getAgent("agent-prompt"), { includeText: true });
    assert.equal(detail.mode, "append");
    assert.equal(detail.customText, customText);
    assert.match(detail.prompt, /只做追加/);

    await manager.restartAgent("agent-prompt");
    const runs = argvRuns();
    assert.ok(runs.length >= 2);
    assert.equal(appendedPrompt(runs.at(-1)), injected, "restart must reuse the frozen prompt");
    await manager.stopAll();
  } finally {
    await cleanup();
  }
});

test("records the frozen prompt in the store projection when the column exists", async () => {
  const { root, store, manager, cleanup } = fixture();
  try {
    await manager.createAgent({ agentId: "agent-column", name: "column", cwd: root, promptConfig: { principleIds: [] } });
    const agent = store.getAgent("agent-column");
    const raw = agent.prompt_config_json || JSON.stringify(agent.prompt_config || null);
    if (manager.promptConfigColumn) {
      const parsed = readFrozenPromptConfig(raw);
      assert.equal(parsed.mode, "none");
      assert.deepEqual(parsed.principleIds, []);
    }
    assert.equal(manager.promptConfigOf(agent).mode, "none");
    await manager.stopAll();
  } finally {
    await cleanup();
  }
});

test("prompt catalog is exposed for the panel and CLI", async () => {
  const { paths, manager, cleanup } = fixture();
  try {
    const catalog = manager.getPromptCatalog();
    assert.equal(catalog.principles.length, 4);
    assert.deepEqual(catalog.defaultPrincipleIds, [...DEFAULT_PROMPT_PRINCIPLE_IDS]);
    assert.equal(typeof paths.home, "string");
  } finally {
    await cleanup();
  }
});

test("diagnostics land in the single Hub log family and survive closeLogs", { timeout: 30000 }, async () => {
  const { root, paths, store, manager, cleanup } = fixture();
  try {
    await manager.createAgent({ agentId: "agent-log", name: "log", cwd: root, promptConfig: { principleIds: [] } });
    await manager.send("agent-log", "prompt", "log-probe");
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
    await manager.stopAll();
    await manager.closeLogs();
    const logPath = join(paths.stateDir, "logs", "hub.log");
    assert.ok(existsSync(logPath), "one Hub log file, no per-agent file");
    const content = readFileSync(logPath, "utf8");
    assert.ok(content.trim().length > 0, "log stream flushed during closeLogs");
    for (const line of content.trim().split("\n")) {
      const record = JSON.parse(line);
      assert.ok(record.ts && record.kind, "every line is a tagged JSON record");
      assert.ok(!("message" in record) || typeof record.message !== "object", "no raw prompt body by default");
    }
    assert.ok(content.includes("agent-log"), "records carry the agent id");
    assert.equal(existsSync(join(paths.agentsDir, "agent-log", "logs", "protocol.jsonl")), false);
    assert.equal(existsSync(join(paths.agentsDir, "agent-log", "logs", "stderr.log")), false);
  } finally {
    await cleanup();
  }
});

test("public agent views never expose the raw frozen prompt column", { timeout: 20000 }, async () => {
  const { publicAgent, publicAgentV2 } = await import("../src/utils.js");
  const { root, store, manager, cleanup } = fixture();
  try {
    await manager.createAgent({ agentId: "agent-view", name: "view", cwd: root, promptConfig: { principleIds: ["first-principles"] } });
    const row = store.getAgent("agent-view");
    assert.ok(row.prompt_config_json, "store keeps the raw projection");
    for (const view of [publicAgent(row), publicAgentV2(row)]) {
      assert.equal(Object.hasOwn(view, "prompt_config_json"), false);
      assert.equal(Object.hasOwn(view, "prompt_config"), false);
      // v1 shape is unchanged: no v2-only fields on the plain view.
      assert.equal(Object.hasOwn(publicAgent(row), "carrier"), false);
    }
    // The normalized summary stays available through the manager view.
    assert.equal(manager.promptConfigOf(row).principleIds.length, 1);
    await manager.stopAll();
  } finally {
    await cleanup();
  }
});

test("dashboard and CLI expose the system prompt entry points", () => {  const html = readFileSync(resolve(import.meta.dirname, "..", "src", "dashboard", "index.html"), "utf8");
  const app = readFileSync(resolve(import.meta.dirname, "..", "src", "dashboard", "app.js"), "utf8");
  const cli = readFileSync(resolve(import.meta.dirname, "..", "src", "cli.js"), "utf8");
  assert.match(html, /id="create-principles"/);
  assert.match(html, /name="system-prompt"/);
  assert.match(html, /id="detail-prompt-summary"/);
  assert.match(app, /\/v2\/prompts/);
  assert.match(app, /body\.promptConfig/);
  assert.match(cli, /--system-prompt-file/);
  assert.match(cli, /prompts.*list/);
});

test("http API exposes the prompt catalog and the frozen prompt without raw columns", async () => {
  const { createHubServer } = await import("../src/http-server.js");
  const { root, paths, store, manager, cleanup } = fixture();
  const server = createHubServer({ manager, store, paths, token: "test-token" });
  await new Promise((resolvePromise) => server.listen(0, "127.0.0.1", resolvePromise));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = async (method, path, body) => {
    const response = await fetch(`${base}${path}`, {
      method,
      headers: { authorization: "Bearer test-token", "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
  };
  try {
    const catalog = await call("GET", "/v2/prompts");
    assert.equal(catalog.status, 200);
    assert.equal(catalog.body.data.principles.length, 4);
    assert.deepEqual(catalog.body.data.defaultPrincipleIds, [...DEFAULT_PROMPT_PRINCIPLE_IDS]);

    const created = await call("POST", "/v2/agents", {
      agentId: "agent-http",
      name: "http",
      cwd: root,
      carrier: "pi",
      skillPolicy: { mode: "all" },
      promptConfig: { principleIds: ["no-unnecessary-entities"], customText: "自定义一段" },
    });
    assert.equal(created.status, 400, "platform agent creation needs registries; the v1 route below is used instead");

    const legacy = await call("POST", "/v1/agents", {
      agentId: "agent-http",
      name: "http",
      cwd: root,
      promptConfig: { principleIds: ["no-unnecessary-entities"], customText: "自定义一段" },
    });
    assert.equal(legacy.status, 201);
    assert.equal(legacy.body.data.prompt_config_json, undefined, "raw store column must not leak");
    assert.equal(legacy.body.data.promptConfig, undefined, "/v1 keeps its original response shape");

    const detail = await call("GET", "/v1/agents/agent-http");
    assert.equal(detail.body.data.id, "agent-http");

    // The frozen prompt is inspectable through the v2 view, digest-only in lists.
    const snapshot = await call("GET", "/v2/snapshot");
    const view = snapshot.body.data.agents.find((agent) => agent.id === "agent-http");
    assert.equal(view.promptConfig.mode, "append");
    assert.deepEqual(view.promptConfig.principleIds, ["no-unnecessary-entities"]);
    assert.equal(view.promptConfig.prompt, undefined, "list view omits the long text");
    assert.equal(view.promptConfig.customTextChars, 5);
    assert.equal(view.promptConfig.digest, manager.promptConfigOf(store.getAgent("agent-http")).digest);
    assert.equal(snapshot.body.data.agents[0].prompt_config_json, undefined);

    const managerStatus = await call("GET", "/v2/skills/manager");
    assert.equal(managerStatus.status, 200);
    assert.equal(managerStatus.body.data.connected, false, "no automatic connection to a global Skill library");
    await manager.stopAll();
  } finally {
    await new Promise((resolvePromise) => server.close(resolvePromise));
    await cleanup();
  }
});

test("a connected Skill Manager snapshot is imported and frozen per agent", async () => {
  const { root, store, manager, cleanup } = fixture();
  const source = join(root, "manager-skill");
  try {
    mkdirSync(source, { recursive: true });
    writeFileSync(join(source, "SKILL.md"), "---\nname: managed\n---\nManaged skill.\n");
    const { SkillRegistry } = await import("../src/skills/registry.js");
    const registry = new SkillRegistry({ home: join(root, "hub-home-skills") });
    const fakeManager = {
      status: async () => ({ connected: true, snapshot: { skillCount: 1 } }),
      listSkills: () => [{ id: "managed", name: "managed" }],
      resolveSources: () => [{ id: "managed", name: "managed", sourcePath: source }],
    };
    let stopped = false;
    const wired = new AgentManager({
      paths: manager.paths,
      store,
      profile: manager.profile,
      skillManager: fakeManager,
      skillRegistry: registry,
      carrierRegistry: {
        diagnose: async () => ({ available: true }),
        get: () => ({ capabilities: { systemPrompt: "append", skillLoadingSelection: true } }),
        createRuntime: () => ({
          pid: 4242,
          alive: true,
          on() {},
          start: async () => ({ model: { provider: "fake", id: "fake" }, thinkingLevel: "off", isStreaming: false, sessionId: "fake", sessionFile: null }),
          request: async (type) => ({ id: "1", type: "response", command: type, success: true, data: {} }),
          stop: async () => { stopped = true; },
        }),
      },
    });
    const created = await wired.createPlatformAgent({
      agentId: "agent-managed",
      name: "managed",
      cwd: root,
      carrier: "claude",
      skillPolicy: { mode: "all" },
      promptConfig: { principleIds: [] },
    });
    assert.deepEqual(created.skill_authorization.grants.map((grant) => grant.id), ["managed"]);
    assert.equal(created.skill_authorization.grants[0].sha256.length, 64);
    assert.ok(registry.listSkills().some((skill) => skill.id === "managed"), "snapshot is copied into the private registry");
    assert.equal(wired.promptConfigOf(created).mode, "none");
    await wired.stopAll();
    await wired.closeLogs();
    assert.equal(stopped, true);
  } finally {
    await cleanup();
  }
});
