import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { AgentManager } from "../src/manager.js";
import { resolveHubPaths, ensureHubLayout } from "../src/paths.js";
import { resolveProfile } from "../src/profile.js";
import { HubStore } from "../src/store.js";

test("manager gives concurrent agents distinct process and context roots with one shared profile", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-manager-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = resolve("fixtures/fake-pi-rpc.js");
  ensureHubLayout(paths);
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({ paths, store, profile });
  const [first, second] = await Promise.all([
    manager.createAgent({ agentId: "agent-one", name: "one", cwd: root }),
    manager.createAgent({ agentId: "agent-two", name: "two", cwd: root, thinking: "high" }),
  ]);
  assert.notEqual(first.pid, second.pid);
  assert.notEqual(first.session_id, second.session_id);
  assert.notEqual(first.session_dir, second.session_dir);
  assert.notEqual(first.agent_dir, second.agent_dir);
  assert.equal(first.profile_digest, second.profile_digest);
  assert.equal(second.thinking, "high");
  await manager.send("agent-one", "prompt", "marker-one");
  await new Promise((resolvePromise) => setTimeout(resolvePromise, 30));
  const firstEvents = manager.listEvents({ agentId: "agent-one" });
  const secondEvents = manager.listEvents({ agentId: "agent-two" });
  assert.ok(firstEvents.some((event) => JSON.stringify(event.payload).includes("marker-one")));
  assert.ok(secondEvents.every((event) => !JSON.stringify(event.payload).includes("marker-one")));
  await manager.stopAll();
  store.close();
  rmSync(root, { recursive: true, force: true });
});

test("manager rejects workspace isolation it cannot actually provide", async () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-manager-mode-"));
  const paths = resolveHubPaths(root);
  paths.piCliPath = resolve("fixtures/fake-pi-rpc.js");
  ensureHubLayout(paths);
  const store = new HubStore(paths.databasePath);
  const profile = resolveProfile({ schemaVersion: 1, name: "test", tools: ["read"], skills: [], extensions: [] });
  const manager = new AgentManager({ paths, store, profile });
  await assert.rejects(
    manager.createAgent({ agentId: "agent-isolated", cwd: root, workspaceMode: "isolated" }),
    /not implemented yet/,
  );
  assert.equal(store.getAgent("agent-isolated"), null);
  store.close();
  rmSync(root, { recursive: true, force: true });
});
