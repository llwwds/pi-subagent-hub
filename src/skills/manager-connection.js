import { execFile } from "node:child_process";
import { constants, accessSync, chmodSync, lstatSync, mkdirSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { homedir, platform as currentPlatform } from "node:os";
import { basename, delimiter, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { atomicWriteJson, canonicalJson, nowIso, sha256 } from "../utils.js";

/**
 * Manual connection to the real `skills-manager` desktop application.
 *
 * The manager stays the single owner of skill content and metadata. PiHub only:
 *   1. records an explicit connection in its own private home,
 *   2. keeps a catalog snapshot for listing, preset/tag selection and drift reporting,
 *   3. freezes skill content into its own content-addressed store at subagent creation,
 *   4. never deploys, re-tags, registers, or otherwise writes manager state.
 *
 * Freezing is what keeps existing subagents stable: a later sync changes the snapshot
 * (and therefore what NEW subagents may select) without touching frozen copies.
 */

const CONFIG_VERSION = 1;
const SNAPSHOT_VERSION = 1;
const SKILL_ID = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,79}$/;
const AGENT_KEY = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/;
const RELEASE = /^(\d+)\.(\d+)\.(\d+)$/;
const MIN_SUPPORTED = [1, 28, 0];
const DEFAULT_TIMEOUT_MS = 30_000;
const MAX_OUTPUT_BYTES = 16 * 1024 * 1024;
const STALE_AFTER_MS = 24 * 60 * 60 * 1000;
const MANAGER_DIR_NAME = ".skills-manager";
const CLI_NAME = "skills-manager-cli";

/** Every command PiHub may run against the manager. All are read-only queries. */
export const READ_ONLY_COMMANDS = Object.freeze([
  "repo status",
  "presets list",
  "skills list",
  "skills show",
  "agents list",
]);

export class ManagerConnectionError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "ManagerConnectionError";
    this.code = code;
  }
}

function reject(code, message) {
  throw new ManagerConnectionError(code, message);
}

function assertAbsolute(path, code, label) {
  if (typeof path !== "string" || !path || !isAbsolute(path) || path.split(/[\\/]/).includes("..")) {
    reject(code, `${label} must be an absolute path without traversal`);
  }
  return resolve(path);
}

function statWithoutFollowing(path) {
  try {
    return lstatSync(path);
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

function isWithin(path, root) {
  const relation = relative(root, path);
  return relation === "" || (relation !== ".." && !relation.startsWith(`..${sep}`) && !isAbsolute(relation));
}

function assertReadOnly(args) {
  const command = args.slice(0, 2).join(" ");
  if (!READ_ONLY_COMMANDS.includes(command)) {
    reject("manager_write_refused", `only read-only manager commands are allowed, refused: ${command}`);
  }
}

function executableFile(path) {
  if (typeof path !== "string" || !path || !isAbsolute(path)) return null;
  const stat = statWithoutFollowing(path);
  if (!stat || !stat.isFile()) return null;
  try {
    accessSync(path, constants.X_OK);
    return realpathSync(path);
  } catch {
    return null;
  }
}

function parseVersion(output) {
  for (const token of String(output).trim().split(/\s+/).reverse()) {
    if (RELEASE.test(token)) return token;
  }
  return null;
}

function versionAtLeast(version, minimum) {
  const parts = RELEASE.exec(version || "");
  if (!parts) return false;
  const current = [Number(parts[1]), Number(parts[2]), Number(parts[3])];
  for (let index = 0; index < 3; index += 1) {
    if (current[index] !== minimum[index]) return current[index] > minimum[index];
  }
  return true;
}

function run(cliPath, args, { timeoutMs, env }) {
  return new Promise((resolvePromise, rejectPromise) => {
    execFile(cliPath, args, { timeout: timeoutMs, maxBuffer: MAX_OUTPUT_BYTES, env, windowsHide: true },
      (error, stdout, stderr) => {
        if (error) {
          const detail = String(stderr || error.message).trim().split("\n").slice(0, 3).join(" ");
          rejectPromise(new ManagerConnectionError("manager_query_failed",
            `${CLI_NAME} ${args.join(" ")} failed: ${detail || "no output"}`));
          return;
        }
        resolvePromise(String(stdout));
      });
  });
}

/** Locate the real manager CLI without touching manager state. */
export async function discoverManager({ cliPath, env = process.env, platform = currentPlatform(), timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
  const userHome = env.HOME || homedir();
  const managerHome = resolve(env.SKILLS_MANAGER_HOME || join(userHome, MANAGER_DIR_NAME));
  const candidates = [];
  for (const candidate of [cliPath, env.PIHUB_SKILLS_MANAGER_CLI, env.SKILLS_MANAGER_CLI]) {
    if (candidate) candidates.push(resolve(candidate));
  }
  candidates.push(join(managerHome, "bin", CLI_NAME));
  if (platform === "darwin") candidates.push(`/Applications/skills-manager.app/Contents/MacOS/${CLI_NAME}`);
  for (const entry of String(env.PATH || "").split(delimiter).filter(Boolean)) {
    candidates.push(join(entry, CLI_NAME));
  }
  const unique = [...new Set(candidates)];
  const found = unique.map(executableFile).find(Boolean) || null;
  const result = {
    available: false,
    cliPath: found,
    version: null,
    managerBaseDir: managerHome,
    managerSkillsDir: join(managerHome, "skills"),
    candidates: unique,
    versionSupported: false,
    reason: null,
  };
  if (!found) {
    result.reason = `skills-manager CLI not found; set PIHUB_SKILLS_MANAGER_CLI or pass cliPath`;
    return result;
  }
  try {
    const version = parseVersion(await run(found, ["--version"], { timeoutMs, env }));
    if (!version) {
      result.reason = "skills-manager CLI did not report a release version";
      return result;
    }
    result.available = true;
    result.version = version;
    result.versionSupported = versionAtLeast(version, MIN_SUPPORTED);
    if (!result.versionSupported) result.reason = `skills-manager ${version} is older than ${MIN_SUPPORTED.join(".")}`;
  } catch (error) {
    result.reason = error.message;
  }
  return result;
}

function normalizeSelection(selection) {
  if (selection === undefined || selection === null) return { presetIds: [], tags: [], ids: [] };
  if (typeof selection !== "object" || Array.isArray(selection)) {
    reject("invalid_manager_selection", "selection must be an object with presetIds, tags, and ids arrays");
  }
  for (const key of Object.keys(selection)) {
    if (!["presetIds", "tags", "ids"].includes(key)) {
      reject("invalid_manager_selection", `unknown selection field: ${key}`);
    }
  }
  const pick = (key) => {
    const value = selection[key];
    if (value === undefined || value === null) return [];
    if (!Array.isArray(value) || value.some((item) => typeof item !== "string" || !item.trim())) {
      reject("invalid_manager_selection", `selection.${key} must be an array of non-empty strings`);
    }
    return [...new Set(value.map((item) => item.trim()))].sort();
  };
  return { presetIds: pick("presetIds"), tags: pick("tags"), ids: pick("ids") };
}

function filterSkills(skills, { query, preset, tag, deployedTo } = {}) {
  let result = skills;
  if (query) {
    const needle = String(query).toLowerCase();
    result = result.filter((skill) => [skill.id, skill.name, skill.description || ""]
      .some((field) => field.toLowerCase().includes(needle)));
  }
  if (preset) {
    result = result.filter((skill) => skill.presets.includes(preset) || skill.presetIds.includes(preset));
  }
  if (tag) result = result.filter((skill) => skill.tags.includes(tag));
  if (deployedTo) result = result.filter((skill) => skill.deployedTo.includes(deployedTo));
  return result;
}

function snapshotDigest(skills) {
  const canonical = skills.map(({ id, name, path, enabled, tags, presets, presetIds, deployedTo, sourceType }) =>
    ({ id, name, path, enabled, tags, presets, presetIds, deployedTo, sourceType }));
  return `sha256:${sha256(canonicalJson(canonical))}`;
}

function normalizePolicy(policy) {
  if (policy === undefined) return { mode: "all" };
  if (!policy || typeof policy !== "object" || Array.isArray(policy)) {
    reject("invalid_skill_policy", "skill policy must be {mode:'all'} or {mode:'only',ids:[...]}");
  }
  if (policy.mode === "all" && Object.keys(policy).length === 1) return { mode: "all" };
  if (policy.mode === "only" && Array.isArray(policy.ids) && Object.keys(policy).length === 2) {
    if (policy.ids.some((id) => typeof id !== "string" || !SKILL_ID.test(id))) {
      reject("invalid_skill_policy", "skill IDs must be 1-80 safe letters, digits, dots, underscores, or hyphens");
    }
    const ids = [...new Set(policy.ids)].sort();
    if (ids.length !== policy.ids.length) reject("invalid_skill_policy", "duplicate skill IDs are not allowed");
    return { mode: "only", ids };
  }
  reject("invalid_skill_policy", "skill policy must be {mode:'all'} or {mode:'only',ids:[...]}");
}

export class SkillManagerConnection {
  constructor({ home, cliPath, env = process.env, platform = currentPlatform(), timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
    this.home = assertAbsolute(home, "unsafe_manager_path", "home");
    this.cliPath = cliPath == null ? null : assertAbsolute(cliPath, "unsafe_manager_path", "cliPath");
    this.env = env;
    this.platform = platform;
    this.timeoutMs = timeoutMs;
    this.skillsRoot = join(this.home, "skills");
    this.configPath = join(this.skillsRoot, "manager-connection.json");
    this.snapshotPath = join(this.skillsRoot, "manager-catalog.json");
    /** PiHub-owned distribution target, used only if the user manually registers it. */
    this.targetSkillsDir = join(this.skillsRoot, "manager-target");
  }

  #privateDir(path, { create = false } = {}) {
    if (!statWithoutFollowing(path) && create) mkdirSync(path, { recursive: true, mode: 0o700 });
    const stat = statWithoutFollowing(path);
    if (!stat) return false;
    if (!stat.isDirectory() || stat.isSymbolicLink()) {
      reject("unsafe_manager_path", `skill manager storage must be a real directory: ${path}`);
    }
    if (create) chmodSync(path, 0o700);
    return true;
  }

  #readRecord(path, code, label) {
    const stat = statWithoutFollowing(path);
    if (!stat) return null;
    if (!stat.isFile() || stat.isSymbolicLink()) reject("unsafe_manager_path", `${label} must be a real file: ${path}`);
    try {
      return JSON.parse(readFileSync(path, "utf8"));
    } catch {
      reject(code, `${label} is not valid JSON: ${path}`);
    }
    return null;
  }

  #assertReplaceable(path) {
    const stat = statWithoutFollowing(path);
    if (stat && (stat.isSymbolicLink() || !stat.isFile())) {
      reject("unsafe_manager_path", `refusing to replace a non-regular manager file: ${path}`);
    }
  }

  #writeRecord(path, value) {
    this.#assertReplaceable(path);
    this.#privateDir(this.skillsRoot, { create: true });
    atomicWriteJson(path, value);
    chmodSync(path, 0o600);
  }

  #readConfig() {
    const config = this.#readRecord(this.configPath, "invalid_manager_config", "manager connection");
    if (!config) return null;
    if (config.schemaVersion !== CONFIG_VERSION || typeof config.cliPath !== "string") {
      reject("invalid_manager_config", "unsupported manager connection format");
    }
    return {
      ...config,
      managerBaseDir: assertAbsolute(config.managerBaseDir, "invalid_manager_config", "managerBaseDir"),
      managerSkillsDir: assertAbsolute(config.managerSkillsDir, "invalid_manager_config", "managerSkillsDir"),
      selection: normalizeSelection(config.selection),
    };
  }

  #readSnapshot() {
    const snapshot = this.#readRecord(this.snapshotPath, "invalid_manager_snapshot", "manager catalog snapshot");
    if (!snapshot) return null;
    if (snapshot.schemaVersion !== SNAPSHOT_VERSION || !Array.isArray(snapshot.skills) || typeof snapshot.digest !== "string") {
      reject("invalid_manager_snapshot", "unsupported manager catalog snapshot format");
    }
    for (const skill of snapshot.skills) {
      if (!skill || typeof skill.id !== "string" || !SKILL_ID.test(skill.id) || typeof skill.path !== "string") {
        reject("invalid_manager_snapshot", "manager catalog snapshot contains an invalid skill entry");
      }
    }
    return snapshot;
  }

  #publicConfig(config) {
    return {
      schemaVersion: config.schemaVersion,
      connectedAt: config.connectedAt,
      lastSyncedAt: config.lastSyncedAt,
      cliPath: config.cliPath,
      managerBaseDir: config.managerBaseDir,
      managerSkillsDir: config.managerSkillsDir,
      managerVersion: config.managerVersion,
      agentKey: config.agentKey,
      selection: config.selection,
      source: config.source,
    };
  }

  #sourceState(snapshot) {
    if (!snapshot) return null;
    const missing = [];
    const dirStat = statWithoutFollowing(snapshot.manager?.skillsDir || "");
    const skillsDirOk = Boolean(dirStat && dirStat.isDirectory() && !dirStat.isSymbolicLink());
    for (const skill of snapshot.skills) {
      const stat = statWithoutFollowing(skill.entryPath || skill.path);
      if (!stat || !stat.isDirectory() || stat.isSymbolicLink()) missing.push(skill.id);
    }
    return { skillsDir: snapshot.manager?.skillsDir || null, skillsDirOk, missing, checkedAt: nowIso() };
  }

  #snapshotSummary(snapshot, source) {
    if (!snapshot) return null;
    const synced = Date.parse(snapshot.syncedAt);
    const ageMs = Number.isFinite(synced) ? Date.now() - synced : null;
    const state = source === undefined ? this.#sourceState(snapshot) : source;
    const missing = state?.missing || [];
    const invalid = (snapshot.invalid || []).map((entry) => ({ id: entry.id, code: entry.code, reason: entry.reason }));
    const warnings = (snapshot.warnings || []).map((entry) => ({ id: entry.id, code: entry.code, reason: entry.reason }));
    const freshness = { ageMs, stale: ageMs === null || ageMs > STALE_AFTER_MS };
    const availability = {
      selectedInvalid: invalid,
      warnings,
      invalidCount: invalid.length,
      warningCount: warnings.length,
      checkedAt: state?.checkedAt || null,
    };
    const stale = freshness.stale || missing.length > 0 || invalid.length > 0 || state?.skillsDirOk === false;
    return {
      syncedAt: snapshot.syncedAt,
      skillCount: snapshot.skills.length,
      selectedIds: snapshot.skills.map((skill) => skill.id),
      digest: snapshot.digest,
      mode: snapshot.manager?.mode || "cli",
      managerVersion: snapshot.manager?.version || null,
      managerSkillsDir: snapshot.manager?.skillsDir || null,
      ageMs,
      freshness,
      availability,
      stale,
      staleReason: freshness.stale
        ? "snapshot is older than 24h"
        : (state?.skillsDirOk === false
          ? "manager skills directory is missing"
          : (missing.length > 0
            ? `${missing.length} selected skill source path(s) missing`
            : (invalid.length > 0 ? `${invalid.length} selected manager entry(ies) are not importable` : null))),
      missingSources: missing,
      invalidSources: invalid,
      warnings,
      skippedCount: invalid.length,
      warningCount: warnings.length,
    };
  }

  async #probe() {
    let configured = null;
    try {
      configured = this.#readConfig();
    } catch {
      configured = null;
    }
    const found = await discoverManager({
      cliPath: configured?.cliPath || this.cliPath, env: this.env, platform: this.platform, timeoutMs: this.timeoutMs,
    });
    return {
      available: found.available,
      cliPath: found.cliPath,
      version: found.version,
      versionSupported: found.versionSupported,
      baseDir: found.managerBaseDir,
      skillsDir: found.managerSkillsDir,
      reason: found.reason,
    };
  }

  async #json(cliPath, command) {
    const args = [...command.split(" "), "--json"];
    assertReadOnly(args);
    const raw = await run(cliPath, args, { timeoutMs: this.timeoutMs, env: this.env });
    try {
      return JSON.parse(raw);
    } catch {
      reject("manager_query_failed", `${command} did not return JSON`);
    }
    return null;
  }

  async #repoStatus(cliPath) {
    const parsed = await this.#json(cliPath, "repo status");
    if (!parsed || typeof parsed.base_dir !== "string" || typeof parsed.skills_dir !== "string") {
      reject("manager_query_failed", "repo status is missing base_dir/skills_dir");
    }
    return {
      baseDir: assertAbsolute(parsed.base_dir, "invalid_manager_config", "manager base directory"),
      skillsDir: assertAbsolute(parsed.skills_dir, "invalid_manager_config", "manager skills directory"),
      activePresetId: typeof parsed.active_preset_id === "string" ? parsed.active_preset_id : null,
    };
  }

  async #listPresets(cliPath) {
    const parsed = await this.#json(cliPath, "presets list");
    if (!Array.isArray(parsed)) reject("manager_query_failed", "presets list did not return an array");
    return parsed.map((preset) => ({
      id: String(preset.id),
      name: String(preset.name),
      skillCount: Number(preset.skill_count) || 0,
      active: Boolean(preset.active),
    }));
  }

  async #listSkills(cliPath) {
    const parsed = await this.#json(cliPath, "skills list");
    if (!Array.isArray(parsed)) reject("manager_query_failed", "skills list did not return an array");
    return parsed.map((skill) => {
      const name = String(skill.name || "");
      if (!SKILL_ID.test(name)) {
        reject("invalid_manager_snapshot", `manager returned an unusable skill name: ${JSON.stringify(name)}`);
      }
      return {
        id: name,
        name,
        description: typeof skill.description === "string" ? skill.description : null,
        path: assertAbsolute(skill.path, "invalid_manager_snapshot", "skill path"),
        enabled: skill.enabled !== false,
        tags: Array.isArray(skill.tags) ? skill.tags.map(String) : [],
        presets: Array.isArray(skill.presets) ? skill.presets.map(String) : [],
        presetIds: Array.isArray(skill.preset_ids) ? skill.preset_ids.map(String) : [],
        deployedTo: Array.isArray(skill.deployed_to) ? skill.deployed_to.map(String) : [],
        sourceType: typeof skill.source_type === "string" ? skill.source_type : "local",
      };
    });
  }

  #matches(skill, selection) {
    return (!selection.presetIds.length || skill.presetIds.some((id) => selection.presetIds.includes(id)))
      && (!selection.tags.length || skill.tags.some((tag) => selection.tags.includes(tag)))
      && (!selection.ids.length || selection.ids.includes(skill.id));
  }

  #select(skills, presets, selection, { validate = true } = {}) {
    if (validate && selection.presetIds.length) {
      const known = new Set(presets.map((preset) => preset.id));
      for (const id of selection.presetIds) {
        if (!known.has(id)) reject("invalid_manager_selection", `unknown manager preset ID: ${id}`);
      }
    }
    return skills.filter((skill) => this.#matches(skill, selection)).sort((a, b) => a.id.localeCompare(b.id));
  }

  /**
   * Canonicalises a path that must live inside the manager library, mapping the
   * literal library root onto its real path so any symlink *inside* the library is
   * detected instead of silently followed.
   */
  /**
   * Resolves a path that must live inside the manager library and returns its
   * canonical form.
   *
   * The same library can be addressed through a system alias (macOS maps
   * `/tmp` to `/private/tmp`), so a path is accepted when it falls under either
   * the configured literal root or its canonical root. Symlinks *inside* the
   * library stay rejected: every segment below the chosen root is checked with
   * lstat, and the realpath must match the expected canonical path exactly.
   */
  #canonicalEntry(literalPath, literalRoot, canonicalRoot, label) {
    const resolved = resolve(literalPath);
    let base = null;
    let segments = null;
    if (isWithin(resolved, literalRoot)) {
      base = literalRoot;
      segments = relative(literalRoot, resolved);
    } else if (isWithin(resolved, canonicalRoot)) {
      // Already canonical (e.g. snapshot.entryPath produced by realpathSync).
      base = canonicalRoot;
      segments = relative(canonicalRoot, resolved);
    } else {
      reject("unsafe_manager_skill_path", `${label} escapes the manager library`);
    }
    const parts = segments.split(sep).filter(Boolean);
    let cursor = base;
    for (const part of parts) {
      cursor = join(cursor, part);
      const stat = statWithoutFollowing(cursor);
      if (!stat) reject("manager_source_missing", `${label} is missing: ${cursor}`);
      if (stat.isSymbolicLink()) {
        reject("unsafe_manager_skill_path", `${label} crosses a symlink inside the manager library`);
      }
    }
    let canonical;
    try {
      canonical = realpathSync(resolved);
    } catch {
      reject("manager_source_missing", `${label} is missing: ${resolved}`);
    }
    const expected = join(canonicalRoot, segments);
    if (canonical !== expected) {
      reject("unsafe_manager_skill_path", `${label} crosses a symlink inside the manager library`);
    }
    if (!isWithin(canonical, canonicalRoot)) {
      reject("unsafe_manager_skill_path", `${label} escapes the manager library`);
    }
    return canonical;
  }

  /**
   * Resolves the directory that actually holds SKILL.md for one catalog entry.
   * A plain entry is its own directory; a manager-recognised nested entry is
   * resolved through the CLI's authoritative `skill_file`, never guessed.
   */
  async #resolveEntry(cliPath, skill, { literalRoot, canonicalRoot }) {
    const literalPath = assertAbsolute(skill.path, "invalid_manager_snapshot", `skill path for ${skill.id}`);
    const stat = statWithoutFollowing(literalPath);
    if (stat?.isSymbolicLink()) {
      reject("unsafe_manager_skill_path", `manager skill directory must not be a symlink: ${skill.id}`);
    }
    if (stat && stat.isDirectory() && statWithoutFollowing(join(literalPath, "SKILL.md"))) {
      return this.#canonicalEntry(literalPath, literalRoot, canonicalRoot, `skill ${skill.id}`);
    }
    let shown;
    try {
      shown = await this.#json(cliPath, `skills show ${skill.id}`);
    } catch (error) {
      if (!(error instanceof ManagerConnectionError)) throw error;
      reject("invalid_manager_skill", `manager cannot resolve a SKILL.md entry for ${skill.id}: ${error.message}`);
    }
    const skillFile = shown?.skill_file;
    if (typeof skillFile !== "string" || !isAbsolute(skillFile) || basename(skillFile) !== "SKILL.md") {
      reject("invalid_manager_skill", `manager did not report a SKILL.md entry for ${skill.id}`);
    }
    const fileStat = statWithoutFollowing(skillFile);
    if (!fileStat) {
      if (!stat || !stat.isDirectory()) {
        reject("manager_source_missing", `manager skill source is missing: ${skill.id}`);
      }
      reject("invalid_manager_skill", `manager-reported SKILL.md entry is missing: ${skill.id}`);
    }
    if (fileStat.isSymbolicLink() || !fileStat.isFile()) {
      reject("unsafe_manager_skill_path", `manager skill entry must be a regular SKILL.md file: ${skill.id}`);
    }
    const entryDir = dirname(assertAbsolute(skillFile, "unsafe_manager_skill_path", `skill_file for ${skill.id}`));
    const dirStat = statWithoutFollowing(entryDir);
    if (!dirStat || !dirStat.isDirectory() || dirStat.isSymbolicLink()) {
      reject("unsafe_manager_skill_path", `manager skill entry directory is not a real directory: ${skill.id}`);
    }
    return this.#canonicalEntry(entryDir, literalRoot, canonicalRoot, `skill ${skill.id}`);
  }

  async #buildSnapshot({ cliPath, skillsDir, baseDir, version, selection }) {
    const dirStat = statWithoutFollowing(skillsDir);
    if (!dirStat || !dirStat.isDirectory() || dirStat.isSymbolicLink()) {
      reject("manager_source_missing", `manager skills directory is missing: ${skillsDir}`);
    }
    const literalRoot = resolve(skillsDir);
    const canonicalRoot = realpathSync(skillsDir);
    const [skills, presets] = await Promise.all([this.#listSkills(cliPath), this.#listPresets(cliPath)]);
    const knownIds = new Set(skills.map((skill) => skill.id));
    const knownPresets = new Set(presets.map((preset) => preset.id));
    for (const id of selection.presetIds) {
      if (!knownPresets.has(id)) reject("invalid_manager_selection", `unknown manager preset ID: ${id}`);
    }
    for (const id of selection.ids) {
      if (!knownIds.has(id)) reject("invalid_manager_selection", `unknown manager skill: ${id}`);
    }
    const usable = [];
    const unusable = [];
    for (const skill of skills) {
      try {
        const entryPath = await this.#resolveEntry(cliPath, skill, { literalRoot, canonicalRoot });
        usable.push({ ...skill, entryPath });
      } catch (error) {
        if (!(error instanceof ManagerConnectionError)) throw error;
        unusable.push({ skill, code: error.code, reason: error.message });
      }
    }
    const selected = this.#select(usable, presets, selection, { validate: false });
    const invalid = unusable.filter((entry) => this.#matches(entry.skill, selection))
      .map((entry) => ({ id: entry.skill.id, code: entry.code, reason: entry.reason }));
    const warnings = unusable.filter((entry) => !this.#matches(entry.skill, selection))
      .map((entry) => ({ id: entry.skill.id, code: entry.code, reason: entry.reason }));
    for (const id of selection.ids) {
      const broken = invalid.find((entry) => entry.id === id);
      if (broken) reject(broken.code, broken.reason);
    }
    if (!selected.length) {
      if (invalid.length) reject(invalid[0].code, invalid[0].reason);
      reject("manager_catalog_empty", "manager catalog has no matching skills");
    }
    return {
      schemaVersion: SNAPSHOT_VERSION,
      syncedAt: nowIso(),
      manager: {
        cliPath,
        version,
        baseDir,
        skillsDir,
        mode: "cli",
        activePresetId: presets.find((preset) => preset.active)?.id || null,
        presets,
      },
      digest: snapshotDigest(selected),
      skills: selected,
      invalid,
      warnings,
    };
  }

  /**
   * Explicit, user-invoked connection. Records PiHub-side state only; the manager is
   * queried read-only. `agentKey` optionally records the manager agent that the user
   * manually registered for PiHub (see `manualRegistrationCommand`).
   */
  async connect({ cliPath, managerBaseDir, managerSkillsDir, agentKey, selection, source } = {}) {
    const discovery = await discoverManager({ cliPath: cliPath || this.cliPath, env: this.env, platform: this.platform, timeoutMs: this.timeoutMs });
    if (!discovery.available) reject("manager_unavailable", discovery.reason || "skills-manager CLI is unavailable");
    if (!discovery.versionSupported) {
      reject("manager_unavailable", discovery.reason || `skills-manager ${discovery.version} is not supported`);
    }
    const normalizedSelection = normalizeSelection(selection);
    if (agentKey != null && (typeof agentKey !== "string" || !AGENT_KEY.test(agentKey))) {
      reject("invalid_manager_agent", "agentKey must be 1-64 letters, digits, dots, underscores, or hyphens");
    }
    this.#assertReplaceable(this.configPath);
    this.#assertReplaceable(this.snapshotPath);
    const repo = await this.#repoStatus(discovery.cliPath);
    if (managerBaseDir != null) {
      const requested = assertAbsolute(managerBaseDir, "invalid_manager_config", "managerBaseDir");
      if (requested !== repo.baseDir && realpathSync(requested) !== realpathSync(repo.baseDir)) {
        reject("invalid_manager_config", `manager base directory mismatch: manager reports ${repo.baseDir}`);
      }
    }
    if (managerSkillsDir != null) {
      const requested = assertAbsolute(managerSkillsDir, "invalid_manager_config", "managerSkillsDir");
      if (requested !== repo.skillsDir && realpathSync(requested) !== realpathSync(repo.skillsDir)) {
        reject("invalid_manager_config", `manager skills directory mismatch: manager reports ${repo.skillsDir}`);
      }
    }
    if (agentKey) {
      const agents = await this.#json(discovery.cliPath, "agents list");
      if (!Array.isArray(agents) || !agents.some((agent) => agent.key === agentKey)) {
        reject("invalid_manager_agent", `unknown manager agent key: ${agentKey}`);
      }
    }
    const snapshot = await this.#buildSnapshot({
      cliPath: discovery.cliPath,
      skillsDir: repo.skillsDir,
      baseDir: repo.baseDir,
      version: discovery.version,
      selection: normalizedSelection,
    });
    const config = {
      schemaVersion: CONFIG_VERSION,
      connectedAt: nowIso(),
      lastSyncedAt: snapshot.syncedAt,
      cliPath: discovery.cliPath,
      managerBaseDir: repo.baseDir,
      managerSkillsDir: repo.skillsDir,
      managerVersion: discovery.version,
      agentKey: agentKey || null,
      selection: normalizedSelection,
      source: typeof source === "string" && source.trim() ? source.trim() : "manual",
    };
    this.#writeRecord(this.snapshotPath, snapshot);
    this.#writeRecord(this.configPath, config);
    return { connected: true, config: this.#publicConfig(config), snapshot: this.#snapshotSummary(snapshot) };
  }

  /**
   * Connection + snapshot + source health + live drift. `live` additionally asks the
   * manager for the current catalog (read-only) and diffs it against the snapshot.
   */
  async status({ live = false } = {}) {
    const result = { connected: false, config: null, snapshot: null, manager: null, source: null, drift: null, error: null };
    let config = null;
    let snapshot = null;
    try {
      config = this.#readConfig();
      snapshot = this.#readSnapshot();
      result.source = this.#sourceState(snapshot);
      result.snapshot = this.#snapshotSummary(snapshot, result.source);
    } catch (error) {
      if (!(error instanceof ManagerConnectionError)) throw error;
      result.error = { code: error.code, message: error.message };
      return result;
    }
    if (!config) {
      if (live) result.manager = await this.#probe();
      return result;
    }
    result.connected = true;
    result.config = this.#publicConfig(config);
    if (!live) return result;
    try {
      result.manager = await this.#probe();
      if (result.manager.available && snapshot) {
        const liveAll = await this.#listSkills(result.manager.cliPath);
        const liveScoped = liveAll.filter((skill) => this.#matches(skill, config.selection));
        const known = new Map(snapshot.skills.map((skill) => [skill.id, skill]));
        const suppressed = new Set([...(snapshot.invalid || []), ...(snapshot.warnings || [])].map((entry) => entry.id));
        const liveIds = new Set(liveScoped.map((skill) => skill.id));
        const byId = (left, right) => left.id.localeCompare(right.id);
        const liveKnown = liveScoped.filter((skill) => known.has(skill.id)).sort(byId);
        const added = [...liveIds].filter((id) => !known.has(id) && !suppressed.has(id)).sort();
        const removed = [...known.keys()].filter((id) => !liveIds.has(id)).sort();
        const metadataChanged = snapshotDigest(liveKnown) !== snapshotDigest([...snapshot.skills].sort(byId));
        result.drift = {
          added,
          removed,
          changed: added.length > 0 || removed.length > 0 || metadataChanged,
          digest: snapshotDigest(liveKnown),
          catalogUnavailable: [...liveIds].filter((id) => suppressed.has(id)).sort(),
        };
      }
    } catch (error) {
      if (!(error instanceof ManagerConnectionError)) throw error;
      result.manager = { available: false, reason: error.message };
    }
    return result;
  }

  /**
   * Refresh the snapshot from the manager. On any failure the previous complete
   * snapshot is left untouched and reported as `keptSnapshot`.
   */
  async sync({ force = false } = {}) {
    const config = this.#readConfig();
    if (!config) reject("manager_not_connected", "no skills-manager connection; run connect first");
    const previous = this.#readSnapshot();
    const discovery = await discoverManager({ cliPath: config.cliPath || this.cliPath, env: this.env, platform: this.platform, timeoutMs: this.timeoutMs });
    if (!discovery.available || !discovery.versionSupported) {
      return {
        synced: false,
        error: { code: "manager_unavailable", message: discovery.reason || "skills-manager CLI is unavailable" },
        keptSnapshot: this.#snapshotSummary(previous),
        config: this.#publicConfig(config),
      };
    }
    try {
      const repo = await this.#repoStatus(discovery.cliPath);
      const snapshot = await this.#buildSnapshot({
        cliPath: discovery.cliPath,
        skillsDir: repo.skillsDir,
        baseDir: repo.baseDir,
        version: discovery.version,
        selection: config.selection,
      });
      const before = new Set((previous?.skills || []).map((skill) => skill.id));
      const after = new Set(snapshot.skills.map((skill) => skill.id));
      const unchanged = previous?.digest === snapshot.digest;
      if (unchanged && !force) {
        const refreshed = { ...config, lastSyncedAt: snapshot.syncedAt, managerVersion: discovery.version };
        this.#writeRecord(this.configPath, refreshed);
        return {
          synced: true,
          changed: false,
          added: [],
          removed: [],
          skipped: snapshot.invalid.map((entry) => ({ id: entry.id, code: entry.code })),
          warnings: snapshot.warnings.map((entry) => ({ id: entry.id, code: entry.code })),
          config: this.#publicConfig(refreshed),
          snapshot: this.#snapshotSummary(snapshot),
        };
      }
      this.#writeRecord(this.snapshotPath, snapshot);
      const refreshed = { ...config, lastSyncedAt: snapshot.syncedAt, managerVersion: discovery.version };
      this.#writeRecord(this.configPath, refreshed);
      return {
        synced: true,
        changed: true,
        added: [...after].filter((id) => !before.has(id)).sort(),
        removed: [...before].filter((id) => !after.has(id)).sort(),
        skipped: snapshot.invalid.map((entry) => ({ id: entry.id, code: entry.code })),
        warnings: snapshot.warnings.map((entry) => ({ id: entry.id, code: entry.code })),
        config: this.#publicConfig(refreshed),
        snapshot: this.#snapshotSummary(snapshot),
      };
    } catch (error) {
      if (!(error instanceof ManagerConnectionError)) throw error;
      return {
        synced: false,
        error: { code: error.code, message: error.message },
        keptSnapshot: this.#snapshotSummary(previous),
        config: this.#publicConfig(config),
      };
    }
  }

  /** Drops the PiHub-side connection. Frozen agent copies and the snapshot are kept. */
  disconnect({ purgeSnapshot = false } = {}) {
    const existed = Boolean(statWithoutFollowing(this.configPath));
    if (existed) rmSync(this.configPath, { force: true });
    if (purgeSnapshot) rmSync(this.snapshotPath, { force: true });
    return {
      connected: false,
      alreadyDisconnected: !existed,
      snapshotKept: !purgeSnapshot && Boolean(statWithoutFollowing(this.snapshotPath)),
      snapshot: purgeSnapshot ? null : this.#snapshotSummary(this.#readSnapshot()),
    };
  }

  listSkills(filters = {}) {
    const skills = this.#readSnapshot()?.skills || [];
    return filterSkills(skills, filters).map((skill) => ({
      ...skill,
      tags: [...skill.tags],
      presets: [...skill.presets],
      presetIds: [...skill.presetIds],
      deployedTo: [...skill.deployedTo],
    }));
  }

  /**
   * Manager source directories for the requested policy, validated against the
   * manager library. Reads the current snapshot, so a sync immediately changes what
   * NEW subagents may freeze while existing authorizations stay untouched.
   */
  resolveSources(policy) {
    const config = this.#readConfig();
    if (!config) reject("manager_not_connected", "no skills-manager connection; run connect first");
    const normalized = normalizePolicy(policy);
    const snapshot = this.#readSnapshot();
    if (!snapshot) reject("manager_snapshot_missing", "no manager catalog snapshot; run sync first");
    const rootStat = statWithoutFollowing(config.managerSkillsDir);
    if (!rootStat || !rootStat.isDirectory() || rootStat.isSymbolicLink()) {
      reject("manager_source_missing", `manager skills directory is missing: ${config.managerSkillsDir}`);
    }
    const literalRoot = resolve(config.managerSkillsDir);
    const canonicalRoot = realpathSync(config.managerSkillsDir);
    const skills = this.listSkills();
    for (const id of normalized.mode === "only" ? normalized.ids : []) {
      const broken = (snapshot.invalid || []).find((entry) => entry.id === id);
      if (broken) reject(broken.code, broken.reason);
    }
    const chosen = normalized.mode === "all"
      ? skills
      : normalized.ids.map((id) => {
        const skill = skills.find((item) => item.id === id);
        if (!skill) reject("skill_not_found", `unknown manager skill: ${id}`);
        return skill;
      });
    return chosen
      .map((skill) => ({
        id: skill.id,
        name: skill.name,
        sourcePath: this.#canonicalEntry(skill.entryPath || skill.path, literalRoot, canonicalRoot, `skill ${skill.id}`),
      }))
      .sort((a, b) => a.id.localeCompare(b.id));
  }

  /**
   * Freeze the manager catalog into the PiHub registry (content-addressed copies),
   * then return an exact-scope authorization. The returned grants keep the frozen
   * digests, so later manager changes never widen an existing subagent.
   */
  async resolveAuthorization(policy, { registry }) {
    if (!registry || typeof registry.importSkill !== "function") {
      reject("invalid_manager_config", "resolveAuthorization requires a SkillRegistry instance");
    }
    const sources = this.resolveSources(policy);
    const config = this.#readConfig();
    for (const source of sources) {
      registry.importSkill({
        id: source.id,
        sourcePath: source.sourcePath,
        sourceLabel: `skills-manager ${config.managerVersion} @ ${source.sourcePath}`,
      });
    }
    return registry.resolveAuthorization({ mode: "only", ids: sources.map((source) => source.id) });
  }

  /**
   * Exact command for registering PiHub's own target directory as a manager custom
   * agent. PiHub never runs it: the user decides when to touch manager state.
   */
  manualRegistrationCommand({ key = "pihub", displayName = "PiHub", path } = {}) {
    if (!AGENT_KEY.test(key)) reject("invalid_manager_agent", "key must be 1-64 letters, digits, dots, underscores, or hyphens");
    const target = path ? assertAbsolute(path, "unsafe_manager_path", "registration path") : this.targetSkillsDir;
    const configured = this.#readConfig();
    const cliPath = configured?.cliPath || this.cliPath
      || join(this.env.SKILLS_MANAGER_HOME || join(this.env.HOME || homedir(), MANAGER_DIR_NAME), "bin", CLI_NAME);
    const argv = [cliPath, "agents", "add-custom", key, "--path", target, "--name", displayName, "--json"];
    return {
      argv,
      shell: argv.map((part) => (part.includes(" ") ? JSON.stringify(part) : part)).join(" "),
      targetSkillsDir: target,
      executed: false,
      note: "Run this yourself if you want skills-manager to deploy library skills into PiHub. PiHub never writes manager state.",
    };
  }
}

export { CONFIG_VERSION, SNAPSHOT_VERSION };
