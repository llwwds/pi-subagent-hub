import { existsSync, mkdirSync } from "node:fs";
import { homedir, platform } from "node:os";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const sourceDir = dirname(fileURLToPath(import.meta.url));

export function getAppRoot() {
  return resolve(sourceDir, "..");
}

export function getDefaultHome() {
  if (platform() === "darwin") return join(homedir(), "Library", "Application Support", "pi-subagent-hub");
  if (platform() === "win32") return join(process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"), "pi-subagent-hub");
  return join(process.env.XDG_DATA_HOME || join(homedir(), ".local", "share"), "pi-subagent-hub");
}

export function resolveHubPaths(home = process.env.PIHUB_HOME || getDefaultHome(), appRoot = getAppRoot()) {
  const resolvedHome = resolve(home);
  return {
    appRoot: resolve(appRoot),
    home: resolvedHome,
    configDir: join(resolvedHome, "config"),
    sharedPiDir: join(resolvedHome, "config", "pi-shared"),
    modelsPath: join(resolvedHome, "config", "pi-shared", "models.json"),
    authPath: join(resolvedHome, "config", "pi-shared", "auth.json"),
    profilePath: join(resolvedHome, "config", "default-profile.json"),
    stateDir: join(resolvedHome, "state"),
    databasePath: join(resolvedHome, "state", "hub.sqlite"),
    daemonPath: join(resolvedHome, "state", "daemon.json"),
    controlPath: join(resolvedHome, "state", "control.json"),
    lockPath: join(resolvedHome, "state", "daemon.lock"),
    daemonStdoutPath: join(resolvedHome, "state", "daemon.stdout.log"),
    daemonStderrPath: join(resolvedHome, "state", "daemon.stderr.log"),
    agentsDir: join(resolvedHome, "agents"),
    piLockPath: join(resolve(appRoot), "vendor", "pi.lock.json"),
    piCliPath: process.env.PIHUB_PI_CLI || join(resolve(appRoot), "vendor", "pi-runtime", "node_modules", "@earendil-works", "pi-coding-agent", "dist", "bundle", "cli.js"),
  };
}

export function ensureHubLayout(paths) {
  for (const path of [paths.home, paths.configDir, paths.sharedPiDir, paths.stateDir, paths.agentsDir]) {
    mkdirSync(path, { recursive: true, mode: 0o700 });
  }
}

export function isStateHomeOutsideApp(paths) {
  const relation = relative(paths.appRoot, paths.home);
  return relation !== "" && (relation.startsWith("..") || isAbsolute(relation));
}

export function assertStateHomeOutsideApp(paths) {
  if (!isStateHomeOutsideApp(paths)) {
    throw new Error(`PIHUB_HOME must be outside the source tree: ${paths.appRoot}`);
  }
}

export function hasBuiltPi(paths) {
  return existsSync(paths.piCliPath);
}
