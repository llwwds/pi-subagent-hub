import { createHash, randomUUID } from "node:crypto";
import {
  chmodSync,
  closeSync,
  constants,
  existsSync,
  fstatSync,
  lstatSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  realpathSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { atomicWriteJson, nowIso } from "../utils.js";

const CATALOG_VERSION = 1;
const SHA256 = /^[a-f0-9]{64}$/;
const SKILL_ID = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,79}$/;
const BLOCKED_DIRECTORIES = new Set([
  ".ssh", ".aws", ".config",
  "secrets", "credentials",
]);
const OMIT_DIRECTORIES = new Set([
  ".git", ".svn", ".hg", ".github", ".cache", ".pytest_cache", ".mypy_cache",
  "node_modules", "__pycache__", ".venv", "venv", "cache", "caches",
  "sessions", "session", "state", "logs", "tmp", "temp", "tests", "test",
]);
const SEMANTIC_HIDDEN_DIRECTORIES = new Set([".signature", ".claude-plugin"]);
const OMIT_FILES = [
  /^\.DS_Store$/,
  /^\.git(?:ignore|keep)$/,
  /^\.[A-Za-z0-9._-]*(?:token|password|secret|credential|api[-_]?key)[A-Za-z0-9._-]*$/i,
  /\.pyc$/i,
];
const BLOCKED_FILES = [
  /^\.env(?:\..*)?$/i,
  /^\.npmrc$/i,
  /^(?:auth|credentials?|secrets?|tokens?|models)(?:[._-].*)?\.(?:json|ya?ml|toml|txt|env)$/i,
  /^id_(?:rsa|dsa|ecdsa|ed25519)(?:\.pub)?$/i,
  /\.(?:pem|p12|pfx|key|sqlite3?|db|log|pid|sock)$/i,
  /^\.DS_Store$/,
];
const SECRET_CONTENT = [
  /-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----/,
  /\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}\b/,
  /\bsk-[A-Za-z0-9_-]{30,}\b/,
];
const MAX_FILES = 1000;
const MAX_FILE_BYTES = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES = 50 * 1024 * 1024;

export class SkillRegistryError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "SkillRegistryError";
    this.code = code;
  }
}

function reject(code, message) {
  throw new SkillRegistryError(code, message);
}

function assertId(id) {
  if (typeof id !== "string" || !SKILL_ID.test(id) || id === "." || id === ".."
    || ["__proto__", "prototype", "constructor"].includes(id)) {
    reject("invalid_skill_id", "skill ID must be 1-80 safe letters, digits, dots, underscores, or hyphens");
  }
  return id;
}

function assertDigest(digest) {
  if (typeof digest !== "string" || !SHA256.test(digest)) {
    reject("invalid_skill_catalog", "skill catalog contains an invalid SHA-256 digest");
  }
  return digest;
}

function statWithoutFollowing(path) {
  try { return lstatSync(path); } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

function assertPrivateDirectory(path, create = false) {
  if (!statWithoutFollowing(path) && create) mkdirSync(path, { recursive: true, mode: 0o700 });
  const stat = statWithoutFollowing(path);
  if (!stat) return false;
  if (!stat.isDirectory() || stat.isSymbolicLink()) reject("unsafe_skill_path", `skill storage must be a real directory: ${path}`);
  if (create) chmodSync(path, 0o700);
  return true;
}

function assertSourcePath(sourcePath) {
  if (typeof sourcePath !== "string" || !isAbsolute(sourcePath) || sourcePath.split(/[\\/]/).includes("..")) {
    reject("unsafe_skill_source", "sourcePath must be an absolute directory path without traversal");
  }
  const path = resolve(sourcePath);
  if (!existsSync(path) || !lstatSync(path).isDirectory() || lstatSync(path).isSymbolicLink()) {
    reject("unsafe_skill_source", `skill source must be a real directory: ${path}`);
  }
  return realpathSync(path);
}

function isWithin(path, root) {
  const relation = relative(root, path);
  return relation === "" || (relation !== ".." && !relation.startsWith(`..${sep}`) && !isAbsolute(relation));
}

function canonicalizePotentialPath(path) {
  let existing = path;
  const missing = [];
  while (!statWithoutFollowing(existing)) {
    missing.unshift(basename(existing));
    existing = dirname(existing);
  }
  return join(realpathSync(existing), ...missing);
}

function assertSafeEntry(name, relativePath, directory) {
  if (name === "." || name === ".." || name.includes("\0") || name.includes(sep)) {
    reject("unsafe_skill_source", `invalid skill package entry: ${relativePath}`);
  }
  if (directory && ((name.startsWith(".") && !SEMANTIC_HIDDEN_DIRECTORIES.has(name)) || BLOCKED_DIRECTORIES.has(name.toLowerCase()))) {
    reject("sensitive_skill_content", `runtime or credential directory is not importable: ${relativePath}`);
  }
  if (!directory && BLOCKED_FILES.some((pattern) => pattern.test(name))) {
    reject("sensitive_skill_content", `runtime or credential file is not importable: ${relativePath}`);
  }
}

function copyPackage(source, target) {
  const count = { files: 0, bytes: 0 };
  function walk(from, to, prefix) {
    for (const name of readdirSync(from).sort()) {
      const relativePath = prefix ? `${prefix}/${name}` : name;
      if (OMIT_DIRECTORIES.has(name.toLowerCase()) || OMIT_FILES.some((pattern) => pattern.test(name))) continue;
      const input = join(from, name);
      const output = join(to, name);
      const stat = lstatSync(input);
      if (stat.isSymbolicLink()) reject("unsafe_skill_source", `symlinks are not importable: ${relativePath}`);
      if (name.toLowerCase() === "skill.md" && relativePath !== "SKILL.md") {
        reject("invalid_skill_package", `only the root SKILL.md is allowed in a skill package: ${relativePath}`);
      }
      assertSafeEntry(name, relativePath, stat.isDirectory());
      if (stat.isDirectory()) {
        mkdirSync(output, { mode: 0o700 });
        walk(input, output, relativePath);
        continue;
      }
      if (!stat.isFile()) reject("unsafe_skill_source", `special files are not importable: ${relativePath}`);
      const fd = openSync(input, constants.O_RDONLY | constants.O_NOFOLLOW);
      let data;
      let current;
      try {
        current = fstatSync(fd);
        if (!current.isFile() || current.size > MAX_FILE_BYTES) {
          reject("unsafe_skill_source", `skill file is not regular or is too large: ${relativePath}`);
        }
        data = readFileSync(fd);
      } finally {
        closeSync(fd);
      }
      count.files += 1;
      count.bytes += data.length;
      if (count.files > MAX_FILES || count.bytes > MAX_TOTAL_BYTES) {
        reject("unsafe_skill_source", "skill package exceeds import size limits");
      }
      if (SECRET_CONTENT.some((pattern) => pattern.test(data.toString("utf8")))) {
        reject("sensitive_skill_content", `possible credential material found in: ${relativePath}`);
      }
      writeFileSync(output, data, { mode: current.mode & 0o111 ? 0o500 : 0o400, flag: "wx" });
    }
  }
  walk(source, target, "");
  if (!existsSync(join(target, "SKILL.md")) || !lstatSync(join(target, "SKILL.md")).isFile()) {
    reject("invalid_skill_package", "skill package must contain a root SKILL.md file");
  }
  return count;
}

function hashPackage(root) {
  if (!lstatSync(root).isDirectory() || lstatSync(root).isSymbolicLink()) {
    reject("skill_snapshot_changed", "skill snapshot root is not a real directory");
  }
  const hash = createHash("sha256");
  function walk(dir, prefix) {
    for (const name of readdirSync(dir).sort()) {
      const relativePath = prefix ? `${prefix}/${name}` : name;
      const path = join(dir, name);
      const stat = lstatSync(path);
      if (stat.isSymbolicLink()) reject("skill_snapshot_changed", `symlink in skill snapshot: ${relativePath}`);
      if (stat.isDirectory()) {
        hash.update(JSON.stringify(["D", relativePath]));
        walk(path, relativePath);
      } else if (stat.isFile()) {
        const data = readFileSync(path);
        hash.update(JSON.stringify(["F", relativePath, Boolean(stat.mode & 0o111), data.length]));
        hash.update(data);
      } else reject("skill_snapshot_changed", `special file in skill snapshot: ${relativePath}`);
    }
  }
  walk(root, "");
  return hash.digest("hex");
}

function freezePackage(root) {
  for (const name of readdirSync(root)) {
    const path = join(root, name);
    if (lstatSync(path).isDirectory()) freezePackage(path);
  }
  chmodSync(root, 0o500);
}

function normalizePolicy(policy) {
  if (policy === undefined) return { mode: "all" };
  if (!policy || typeof policy !== "object" || Array.isArray(policy)) {
    reject("invalid_skill_policy", "skill policy must be {mode:'all'} or {mode:'only',ids:[...]}");
  }
  if (policy.mode === "all" && Object.keys(policy).length === 1) return { mode: "all" };
  if (policy.mode === "only" && Array.isArray(policy.ids) && Object.keys(policy).length === 2) {
    const ids = policy.ids.map(assertId);
    if (new Set(ids).size !== ids.length) reject("invalid_skill_policy", "duplicate skill IDs are not allowed");
    return { mode: "only", ids: [...ids].sort() };
  }
  reject("invalid_skill_policy", "skill policy must be {mode:'all'} or {mode:'only',ids:[...]}");
}

function authorizationDigest(mode, grants) {
  const value = JSON.stringify({ mode, grants: grants.map(({ id, sha256 }) => ({ id, sha256 })) });
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

export class SkillRegistry {
  constructor({ home }) {
    if (typeof home !== "string" || !isAbsolute(home)) reject("unsafe_skill_path", "home must be an absolute path");
    this.home = resolve(home);
    this.root = join(this.home, "skills");
    this.objectsRoot = join(this.root, "objects");
    this.catalogPath = join(this.root, "catalog.json");
    this.lockPath = join(this.root, ".catalog.lock");
  }

  #ensureStorage() {
    assertPrivateDirectory(this.home, true);
    assertPrivateDirectory(this.root, true);
    assertPrivateDirectory(this.objectsRoot, true);
    if (statWithoutFollowing(this.catalogPath)?.isSymbolicLink()) {
      reject("unsafe_skill_path", "skill catalog must not be a symlink");
    }
  }

  #readCatalog() {
    if (!assertPrivateDirectory(this.home)) return { schemaVersion: CATALOG_VERSION, skills: {} };
    if (!assertPrivateDirectory(this.root)) return { schemaVersion: CATALOG_VERSION, skills: {} };
    if (!assertPrivateDirectory(this.objectsRoot)) return { schemaVersion: CATALOG_VERSION, skills: {} };
    const catalogStat = statWithoutFollowing(this.catalogPath);
    if (!catalogStat) return { schemaVersion: CATALOG_VERSION, skills: {} };
    if (!catalogStat.isFile() || catalogStat.isSymbolicLink()) reject("unsafe_skill_path", "skill catalog must be a real file");
    const catalog = JSON.parse(readFileSync(this.catalogPath, "utf8"));
    if (catalog?.schemaVersion !== CATALOG_VERSION || !catalog.skills || typeof catalog.skills !== "object" || Array.isArray(catalog.skills)) {
      reject("invalid_skill_catalog", "unsupported skill catalog format");
    }
    for (const [id, record] of Object.entries(catalog.skills)) {
      assertId(id);
      assertDigest(record?.currentSha256);
      if (!Array.isArray(record.versions) || !record.versions.some((version) => version.sha256 === record.currentSha256)) {
        reject("invalid_skill_catalog", `invalid versions for skill: ${id}`);
      }
      for (const version of record.versions) assertDigest(version.sha256);
    }
    return catalog;
  }

  importSkill({ id, sourcePath, sourceLabel } = {}) {
    assertId(id);
    const source = assertSourcePath(sourcePath);
    if (sourceLabel != null && (typeof sourceLabel !== "string" || !sourceLabel.trim())) {
      reject("unsafe_skill_source", "sourceLabel must be a non-empty string");
    }
    const intendedStorage = canonicalizePotentialPath(this.root);
    if (isWithin(source, intendedStorage) || isWithin(intendedStorage, source)) {
      reject("unsafe_skill_source", "skill source must not overlap platform skill storage");
    }
    this.#ensureStorage();
    const storage = realpathSync(this.root);
    if (isWithin(source, storage) || isWithin(storage, source)) {
      reject("unsafe_skill_source", "skill source must not overlap platform skill storage");
    }
    let lockFd;
    try {
      lockFd = openSync(this.lockPath, "wx", 0o600);
    } catch (error) {
      reject("skill_registry_busy", `skill registry import is locked: ${error.message}`);
    }
    const staging = join(this.objectsRoot, `.staging-${randomUUID()}`);
    try {
      mkdirSync(staging, { mode: 0o700 });
      const size = copyPackage(source, staging);
      const sha256 = hashPackage(staging);
      const destination = join(this.objectsRoot, sha256);
      if (statWithoutFollowing(destination)) {
        if (hashPackage(destination) !== sha256) reject("skill_snapshot_changed", `existing skill snapshot is corrupt: ${sha256}`);
        rmSync(staging, { recursive: true, force: true });
      } else {
        renameSync(staging, destination);
        freezePackage(destination);
      }
      const catalog = this.#readCatalog();
      const record = Object.hasOwn(catalog.skills, id)
        ? catalog.skills[id]
        : { id, currentSha256: null, versions: [] };
      let version = record.versions.find((item) => item.sha256 === sha256);
      if (!version) {
        version = { sha256, source: sourceLabel || source, importedAt: nowIso(), fileCount: size.files, byteCount: size.bytes };
        record.versions.push(version);
      }
      record.currentSha256 = sha256;
      catalog.skills[id] = record;
      atomicWriteJson(this.catalogPath, catalog);
      return { id, ...version, path: destination };
    } finally {
      if (existsSync(staging)) rmSync(staging, { recursive: true, force: true });
      closeSync(lockFd);
      rmSync(this.lockPath, { force: true });
    }
  }

  listSkills() {
    const catalog = this.#readCatalog();
    return Object.values(catalog.skills)
      .map((record) => {
        const current = record.versions.find((version) => version.sha256 === record.currentSha256);
        return { id: record.id, currentSha256: record.currentSha256, source: current.source,
          importedAt: current.importedAt, versions: record.versions.map((version) => ({ ...version })) };
      })
      .sort((a, b) => a.id.localeCompare(b.id));
  }

  resolveAuthorization(policy) {
    const normalized = normalizePolicy(policy);
    const catalog = this.#readCatalog();
    const ids = normalized.mode === "all" ? Object.keys(catalog.skills).sort() : normalized.ids;
    const grants = ids.map((id) => {
      const record = catalog.skills[id];
      if (!record) reject("skill_not_found", `unknown skill ID: ${id}`);
      const sha256 = assertDigest(record.currentSha256);
      const path = join(this.objectsRoot, sha256);
      if (!statWithoutFollowing(path) || hashPackage(path) !== sha256) {
        reject("skill_snapshot_changed", `skill snapshot is missing or changed: ${id}@${sha256}`);
      }
      return { id, sha256, path };
    });
    return { mode: normalized.mode, grants, digest: authorizationDigest(normalized.mode, grants) };
  }

  verifyAuthorization(authorization) {
    if (!authorization || !["all", "only"].includes(authorization.mode) || !Array.isArray(authorization.grants)) {
      reject("invalid_skill_policy", "invalid frozen skill authorization");
    }
    const seen = new Set();
    for (const grant of authorization.grants) {
      assertId(grant.id);
      assertDigest(grant.sha256);
      if (seen.has(grant.id)) reject("invalid_skill_policy", `duplicate grant: ${grant.id}`);
      seen.add(grant.id);
      const expectedPath = join(this.objectsRoot, grant.sha256);
      if (grant.path !== expectedPath || !statWithoutFollowing(expectedPath) || hashPackage(expectedPath) !== grant.sha256) {
        reject("skill_snapshot_changed", `skill snapshot is missing or changed: ${grant.id}@${grant.sha256}`);
      }
    }
    const sorted = [...authorization.grants].sort((a, b) => a.id.localeCompare(b.id));
    if (authorization.digest !== authorizationDigest(authorization.mode, sorted)) {
      reject("invalid_skill_policy", "frozen skill authorization digest does not match grants");
    }
    return true;
  }
}
