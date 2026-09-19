import { createHash, randomBytes, randomUUID } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

export function nowIso() {
  return new Date().toISOString();
}

export function newId() {
  return randomUUID();
}

export function newToken() {
  return randomBytes(32).toString("base64url");
}

export function assertAgentId(value) {
  if (typeof value !== "string" || !/^[a-zA-Z0-9][a-zA-Z0-9._-]{2,127}$/.test(value)) {
    throw new Error("agentId must be 3-128 characters using letters, digits, dot, underscore, or hyphen");
  }
  return value;
}

export function ensurePrivateDir(path) {
  mkdirSync(path, { recursive: true, mode: 0o700 });
  chmodSync(path, 0o700);
}

export function readJson(path, fallback = undefined) {
  if (!existsSync(path)) return fallback;
  return JSON.parse(readFileSync(path, "utf8"));
}

export function atomicWriteJson(path, value, mode = 0o600) {
  ensurePrivateDir(dirname(path));
  const tempPath = `${path}.tmp-${process.pid}-${randomBytes(4).toString("hex")}`;
  writeFileSync(tempPath, `${JSON.stringify(value, null, 2)}\n`, { mode });
  chmodSync(tempPath, mode);
  renameSync(tempPath, path);
  chmodSync(path, mode);
}

export function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

export function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

export function resolveExistingDirectory(path) {
  const resolved = resolve(path);
  if (!existsSync(resolved)) throw new Error(`directory does not exist: ${resolved}`);
  return resolved;
}

export function sleep(ms) {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, ms));
}

export function isProcessAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export function parseModel(value) {
  if (!value) return { provider: null, model: null };
  const slash = value.indexOf("/");
  if (slash <= 0) return { provider: null, model: value };
  return { provider: value.slice(0, slash), model: value.slice(slash + 1) };
}

export function publicAgent(agent) {
  if (!agent) return null;
  const {
    agent_dir: _agentDir,
    logs_dir: _logsDir,
    tools_json: _toolsJson,
    skills_json: _skillsJson,
    extensions_json: _extensionsJson,
    ...safe
  } = agent;
  return safe;
}
