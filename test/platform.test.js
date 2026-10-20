import test from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdirSync, mkdtempSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { AgentManager } from "../src/manager.js";
import { createDefaultCarrierRegistry } from "../src/carriers/index.js";
import { createHubServer } from "../src/http-server.js";
import { ensureHubLayout, resolveHubPaths } from "../src/paths.js";
import { resolveProfile } from "../src/profile.js";
import { SkillRegistry } from "../src/skills/registry.js";
import { HubStore } from "../src/store.js";
import { atomicWriteJson, publicAgent, publicAgentV2 } from "../src/utils.js";

const execFileAsync = promisify(execFile);

test("platform creation keeps v1 behavior and exposes frozen v2 Skill selections", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-platform-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = resolve("fixtures/fake-pi-rpc.js");
  ensureHubLayout(paths);
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({
    paths,
    store,
    profile,
    skillRegistry: new SkillRegistry({ home: root }),
    carrierRegistry: createDefaultCarrierRegistry({ codexCliPath: "/missing/codex", claudeCliPath: "/missing/claude" }),
  });
  const token = "platform-test-token";
  const server = createHubServer({ manager, store, paths, token, onStop() {} });
  try {
    const legacy = await manager.createAgent({ agentId: "legacy-pi", cwd: root });
    assert.equal(legacy.carrier, "pi");
    assert.equal(legacy.skill_authorization, null);
    assert.equal(Object.hasOwn(publicAgent(legacy), "carrier"), false);
    assert.equal(Object.hasOwn(publicAgent(legacy), "skillPolicy"), false);

    const platform = await manager.createPlatformAgent({ agentId: "platform-pi", cwd: root });
    assert.equal(platform.carrier, "pi");
    assert.equal(platform.skill_authorization.mode, "all");
    assert.deepEqual(platform.skill_authorization.grants, []);
    assert.deepEqual(publicAgentV2(platform).skillPolicy, { mode: "all", ids: [] });
    const selected = await manager.createPlatformAgent({ agentId: "selected-pi", cwd: root, skillPolicy: { mode: "only", ids: [] } });
    assert.equal(selected.skill_authorization.mode, "only");
    assert.deepEqual(selected.skill_authorization.grants, []);

    await new Promise((resolvePromise) => server.listen(0, "127.0.0.1", resolvePromise));
    const base = `http://127.0.0.1:${server.address().port}`;
    const get = async (path) => {
      const response = await fetch(`${base}${path}`, { headers: { authorization: `Bearer ${token}` } });
      assert.equal(response.status, 200);
      return (await response.json()).data;
    };
    const legacyCursor = store.appendEvent({ eventId: "platform-test-legacy-event", agentId: legacy.id, eventType: "test_event", payload: { kind: "legacy" } });
    const platformCursor = store.appendEvent({ eventId: "platform-test-v2-event", agentId: platform.id, eventType: "test_event", payload: { kind: "platform" } });
    const old = await get("/v1/snapshot");
    assert.equal(old.agents.length, 1);
    assert.equal(old.agents[0].id, "legacy-pi");
    assert.equal(Object.hasOwn(old.agents[0], "carrier"), false);
    assert.equal(old.eventCursor, legacyCursor);
    const next = await get("/v2/snapshot");
    assert.equal(next.eventCursor, platformCursor);
    assert.equal(next.activeCount, 3);
    assert.deepEqual(new Set(next.agents.map((agent) => agent.carrier)), new Set(["pi"]));
    const after = legacyCursor - 1;
    assert.deepEqual((await get(`/v1/events?after=${after}&limit=2`)).map((event) => event.agent_id), [legacy.id]);
    assert.deepEqual((await get(`/v2/events?after=${after}&limit=2`)).map((event) => event.agent_id), [legacy.id, platform.id]);
    assert.deepEqual(await get(`/v2/events?after=${platformCursor}&limit=2`), []);
    assert.deepEqual((await get("/v2/carriers")).map((carrier) => carrier.id), ["pi", "codex", "claude"]);
    assert.deepEqual(await get("/v2/skills"), []);

    atomicWriteJson(paths.daemonPath, { pid: process.pid, port: server.address().port });
    atomicWriteJson(paths.controlPath, { token });
    const cli = async (...args) => {
      const { stdout } = await execFileAsync(process.execPath, [resolve("bin/pihub.js"), ...args, "--json"], {
        cwd: resolve("."),
        env: { ...process.env, PIHUB_HOME: root },
      });
      const envelope = JSON.parse(stdout);
      assert.equal(envelope.ok, true);
      return envelope.data;
    };
    const carriers = await cli("carriers", "list");
    assert.deepEqual(carriers.map((carrier) => carrier.id), ["pi", "codex", "claude"]);
    const created = await cli("agents", "create", "--carrier", "pi", "--name", "via-cli", "--cwd", root);
    assert.equal(created.carrier, "pi");
    assert.equal(created.skillPolicy.mode, "all");
    assert.deepEqual((await cli("list")).map((agent) => agent.id), ["legacy-pi"]);
    await cli("agents", "prompt", created.id, "test-prompt");
    await cli("agents", "wait", created.id, "--timeout", "5");
    const logs = await cli("agents", "logs", created.id);
    assert.ok(logs.some((event) => JSON.stringify(event.payload).includes("test-prompt")));
  } finally {
    await manager.stopAll();
    await manager.closeLogs();
    if (server.listening) await new Promise((resolvePromise) => server.close(resolvePromise));
    store.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("failed platform runtime startup releases its slot so the agent can restart", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-platform-restart-"));
  const paths = resolveHubPaths(root);
  paths.codexCliPath = "/bin/echo";
  paths.ccSwitchCliPath = null;
  ensureHubLayout(paths);
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({
    paths, store, profile,
    skillRegistry: new SkillRegistry({ home: root }),
    carrierRegistry: createDefaultCarrierRegistry({ codexCliPath: "/bin/echo", claudeCliPath: "/missing/claude" }),
  });
  try {
    await assert.rejects(
      manager.createPlatformAgent({ agentId: "failed-codex", carrier: "codex", cwd: root, thinking: "off" }),
      /does not support thinking level off/,
    );
    assert.equal(store.getAgent("failed-codex").status, "failed");
    assert.equal(manager.runtimes.has("failed-codex"), false);
    store.updateAgent("failed-codex", { thinking: "high" });
    const restarted = await manager.restartAgent("failed-codex");
    assert.equal(restarted.status, "idle");
  } finally {
    await manager.stopAll();
    await manager.closeLogs();
    store.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("v2 Pi selects a private CC Switch route while v1 keeps its original configuration", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-platform-switch-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = resolve("fixtures/fake-pi-rpc.js");
  ensureHubLayout(paths);
  mkdirSync(paths.ccSwitchPiDir, { recursive: true, mode: 0o700 });
  const modelsPath = join(paths.ccSwitchPiDir, "models.json");
  writeFileSync(modelsPath, JSON.stringify({ providers: { demo: { baseUrl: "http://127.0.0.1:9", api: "openai-completions", models: [{ id: "m1" }] } } }), { mode: 0o600 });
  const provider = { id: "demo", name: "Demo", baseUrl: "http://127.0.0.1:9", modelList: ["m1"], defaultModel: "m1", isCurrent: null, configured: true };
  const ccSwitchService = {
    async diagnose() { return { available: true, version: "test" }; },
    async listProviders(app) { return app === "pi" ? [provider] : []; },
    async listAllProviders() { return { pi: [provider], codex: [], claude: [] }; },
    async addProvider() { return provider; },
    async switchProvider() { return provider; },
  };
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({
    paths, store, profile, ccSwitchService,
    skillRegistry: new SkillRegistry({ home: root }),
    carrierRegistry: createDefaultCarrierRegistry({ codexCliPath: "/missing/codex", claudeCliPath: "/missing/claude" }),
  });
  const server = createHubServer({ manager, store, paths, token: "switch-test-token", onStop() {} });
  try {
    const legacy = await manager.createAgent({ agentId: "old-pi", cwd: root });
    const next = await manager.createPlatformAgent({ agentId: "new-pi", carrier: "pi", cwd: root });
    assert.equal(legacy.provider, null);
    assert.equal(next.provider, "demo");
    assert.equal(next.model, "m1");
    assert.equal(readlinkSync(join(next.agent_dir, "models.json")), modelsPath);
    assert.equal(readlinkSync(join(legacy.agent_dir, "models.json")), paths.modelsPath);
    await new Promise((done) => server.listen(0, "127.0.0.1", done));
    const response = await fetch(`http://127.0.0.1:${server.address().port}/v2/providers`, {
      headers: { authorization: "Bearer switch-test-token" },
    });
    assert.equal(response.status, 200);
    assert.deepEqual((await response.json()).data.apps.pi.map((item) => item.id), ["demo"]);
  } finally {
    await manager.stopAll();
    await manager.closeLogs();
    if (server.listening) await new Promise((done) => server.close(done));
    store.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("v2 agent detail returns the frozen prompt text while v1 keeps its legacy shape", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-platform-prompt-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = resolve("fixtures/fake-pi-rpc.js");
  ensureHubLayout(paths);
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({
    paths,
    store,
    profile,
    skillRegistry: new SkillRegistry({ home: root }),
    carrierRegistry: createDefaultCarrierRegistry({ codexCliPath: "/missing/codex", claudeCliPath: "/missing/claude" }),
  });
  const token = "platform-prompt-token";
  const server = createHubServer({ manager, store, paths, token, onStop() {} });
  await new Promise((resolvePromise) => server.listen(0, "127.0.0.1", resolvePromise));
  const base = `http://127.0.0.1:${server.address().port}`;
  const get = async (path) => {
    const response = await fetch(`${base}${path}`, { headers: { authorization: `Bearer ${token}` } });
    assert.equal(response.status, 200, `${path} should answer 200`);
    return (await response.json()).data;
  };
  try {
    await manager.createPlatformAgent({
      agentId: "v2-prompt",
      cwd: root,
      promptConfig: { principleIds: ["first-principles", "no-unnecessary-entities"], customText: "多行\n自定义 $HOME `id`" },
    });
    await manager.createAgent({
      agentId: "v1-prompt",
      cwd: root,
      promptConfig: { principleIds: ["first-principles"], customText: "legacy text" },
    });

    const detail = await get("/v2/agents/v2-prompt");
    assert.equal(detail.promptConfig.mode, "append");
    assert.equal(detail.promptConfig.channel, "append", "pi appends to the native system prompt");
    assert.deepEqual(detail.promptConfig.principleIds, ["first-principles", "no-unnecessary-entities"]);
    assert.equal(detail.promptConfig.customText, "多行\n自定义 $HOME `id`", "custom text stays literal, never shell-expanded");
    assert.match(detail.promptConfig.prompt, /### 工作原则/);
    assert.match(detail.promptConfig.prompt, /多行\n自定义 \$HOME `id`/);
    assert.equal(detail.promptConfig.customTextChars, "多行\n自定义 $HOME `id`".length);
    assert.match(detail.promptConfig.digest, /^sha256:[a-f0-9]{64}$/);
    assert.equal(detail.promptConfig.digest, manager.promptConfigOf(store.getAgent("v2-prompt")).digest);
    assert.equal(detail.prompt_config_json, undefined, "raw store column must not leak");
    assert.equal(detail.prompt_config, undefined);

    await manager.restartAgent("v2-prompt");
    const restarted = await get("/v2/agents/v2-prompt");
    assert.equal(restarted.promptConfig.digest, detail.promptConfig.digest, "restart reuses the frozen digest");
    assert.equal(restarted.promptConfig.prompt, detail.promptConfig.prompt);

    const legacyDetail = await get("/v1/agents/v1-prompt");
    assert.equal(legacyDetail.id, "v1-prompt");
    assert.equal(Object.hasOwn(legacyDetail, "promptConfig"), false, "/v1 keeps its original shape");
    assert.equal(Object.hasOwn(legacyDetail, "prompt_config_json"), false);
    assert.equal(Object.hasOwn(legacyDetail, "prompt_config"), false);
    assert.equal(Object.hasOwn(legacyDetail, "skillPolicy"), false);

    // A legacy Agent still gets a normalized v2 view when asked through /v2.
    const legacyViaV2 = await get("/v2/agents/v1-prompt");
    assert.equal(legacyViaV2.promptConfig.mode, "append");
    assert.equal(legacyViaV2.promptConfig.digest, manager.promptConfigOf(store.getAgent("v1-prompt")).digest);
  } finally {
    await manager.stopAll();
    await manager.closeLogs();
    if (server.listening) await new Promise((resolvePromise) => server.close(resolvePromise));
    store.close();
    rmSync(root, { recursive: true, force: true });
  }
});
