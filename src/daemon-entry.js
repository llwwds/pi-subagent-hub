#!/usr/bin/env node
import { closeSync, existsSync, openSync, readFileSync, unlinkSync } from "node:fs";
import { HubStore } from "./store.js";
import { AgentManager } from "./manager.js";
import { APP_VERSION } from "./constants.js";
import { ensureDefaultProfile, resolveProfile } from "./profile.js";
import { createHubServer } from "./http-server.js";
import { assertStateHomeOutsideApp, ensureHubLayout, resolveHubPaths } from "./paths.js";
import { atomicWriteJson, isProcessAlive, newToken, nowIso, readJson } from "./utils.js";
import { ensurePiConfigFiles } from "./config-files.js";
import { createDefaultCarrierRegistry } from "./carriers/index.js";
import { SkillRegistry } from "./skills/registry.js";
import { CcSwitchService } from "./cc-switch/service.js";

const homeIndex = process.argv.indexOf("--home");
const home = homeIndex >= 0 ? process.argv[homeIndex + 1] : undefined;
const paths = resolveHubPaths(home);
assertStateHomeOutsideApp(paths);
ensureHubLayout(paths);
ensurePiConfigFiles(paths);

if (existsSync(paths.lockPath)) {
  const stale = readJson(paths.lockPath, {});
  if (isProcessAlive(stale.pid)) {
    process.stderr.write(`pi-subagent-hub daemon already running with pid ${stale.pid}\n`);
    process.exit(3);
  }
  unlinkSync(paths.lockPath);
}
const lockFd = openSync(paths.lockPath, "wx", 0o600);
atomicWriteJson(paths.lockPath, { pid: process.pid, startedAt: nowIso() });

const control = readJson(paths.controlPath) || { token: newToken(), createdAt: nowIso() };
atomicWriteJson(paths.controlPath, control);
const profile = resolveProfile(ensureDefaultProfile(paths.profilePath));
const store = new HubStore(paths.databasePath);
const carrierRegistry = createDefaultCarrierRegistry();
const skillRegistry = new SkillRegistry({ home: paths.home });
const ccSwitchService = new CcSwitchService(paths);
const manager = new AgentManager({ paths, store, profile, carrierRegistry, skillRegistry, ccSwitchService });
manager.recoverAfterDaemonRestart();

let closing = false;
let server;
async function shutdown() {
  if (closing) return;
  closing = true;
  try { await manager.stopAll(); } catch (error) { process.stderr.write(`${error.stack || error}\n`); }
  await new Promise((resolvePromise) => server?.close(resolvePromise));
  store.close();
  try { unlinkSync(paths.daemonPath); } catch {}
  try { closeSync(lockFd); } catch {}
  try { unlinkSync(paths.lockPath); } catch {}
  process.exit(0);
}

server = createHubServer({ manager, store, paths, token: control.token, onStop: shutdown });
server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  atomicWriteJson(paths.daemonPath, {
    pid: process.pid,
    host: "127.0.0.1",
    port: address.port,
    version: APP_VERSION,
    startedAt: nowIso(),
    appRoot: paths.appRoot,
  });
  process.stdout.write(`pi-subagent-hub daemon ${APP_VERSION} listening on 127.0.0.1:${address.port}\n`);
});

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
process.on("uncaughtException", (error) => {
  process.stderr.write(`${error.stack || error}\n`);
  shutdown();
});
process.on("unhandledRejection", (error) => {
  process.stderr.write(`${error?.stack || error}\n`);
  shutdown();
});
