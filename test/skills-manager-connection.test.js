import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SkillRegistry } from "../src/skills/registry.js";
import {
  ManagerConnectionError,
  READ_ONLY_COMMANDS,
  SkillManagerConnection,
  discoverManager,
} from "../src/skills/manager-connection.js";

// Fixture CLI: mimics the real `skills-manager-cli` JSON contract. It reads a
// catalog/state file from FIXTURE_MANAGER, logs every invocation, and never
// touches any real manager, database, or global skill directory.
const FIXTURE_CLI = [
  "#!/usr/bin/env node",
  'const fs = require("node:fs");',
  'const path = require("node:path");',
  "const root = process.env.FIXTURE_MANAGER;",
  "const args = process.argv.slice(2);",
  'const command = args.filter((arg) => arg !== "--json").join(" ");',
  'fs.appendFileSync(path.join(root, "calls.log"), JSON.stringify(args) + "\\n");',
  'const read = (name) => JSON.parse(fs.readFileSync(path.join(root, name), "utf8"));',
  'const stateFile = path.join(root, "state.json");',
  "const state = fs.existsSync(stateFile) ? read(\"state.json\") : {};",
  'const fail = (message) => { process.stderr.write(message + "\\n"); process.exit(1); };',
  "if (args.includes(\"--version\")) {",
  '  process.stdout.write((state.version || "skills-manager-cli 1.41.0") + "\\n");',
  "  process.exit(0);",
  "}",
  'if (command === "repo status") {',
  '  if (state.fail === "repo") fail("manager database is locked");',
  "  const catalog = read(\"catalog.json\");",
  "  process.stdout.write(JSON.stringify({",
  '    base_dir: path.join(root, "manager"),',
  '    skills_dir: state.skillsDir || path.join(root, "manager", "skills"),',
  '    db_path: path.join(root, "manager", "skills-manager.db"),',
  '    metadata_dir: path.join(root, "manager", "skills", ".skills-manager"),',
  "    skill_count: catalog.skills.length,",
  "    preset_count: catalog.presets.length,",
  '    active_preset_id: (catalog.presets.find((preset) => preset.active) || {}).id || null,',
  "  }));",
  "  process.exit(0);",
  "}",
  'if (command === "presets list") {',
  '  if (state.fail === "presets") fail("preset index unavailable");',
  '  process.stdout.write(JSON.stringify(read("catalog.json").presets));',
  "  process.exit(0);",
  "}",
  'if (command === "skills list") {',
  '  if (state.fail === "skills") fail("catalog index unavailable");',
  '  process.stdout.write(JSON.stringify(read("catalog.json").skills));',
  "  process.exit(0);",
  "}",
  'if (command === "agents list") {',
  '  process.stdout.write(JSON.stringify(read("catalog.json").agents));',
  "  process.exit(0);",
  "}",
  'if (command.startsWith("skills show ")) {',
  '  const reference = command.slice("skills show ".length);',
  '  if ((state.showFail || []).includes(reference)) fail(JSON.stringify({ ok: false, error: "no SKILL.md found for " + reference }));',
  '  const catalog = read("catalog.json");',
  '  const skill = catalog.skills.find((entry) => entry.name === reference || entry.id === reference);',
  '  if (!skill) fail(JSON.stringify({ ok: false, error: "skill not found: " + reference }));',
  '  const file = (catalog.skill_files || {})[skill.name] || path.join(skill.path, "SKILL.md");',
  '  process.stdout.write(JSON.stringify({ name: skill.name, path: skill.path, skill_file: file, files: ["SKILL.md"], markdown: "" }));',
  "  process.exit(0);",
  "}",
  'fail("unsupported fixture command: " + command);',
  "",
].join("\n");

function writeSkill(dir, name, body) {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "SKILL.md"), `---\nname: ${name}\ndescription: ${name} description\n---\n${body}\n`);
}

function catalogEntry(root, name, extra = {}, skillsDir) {
  return {
    id: `id-${name}`,
    name,
    description: `${name} from manager`,
    path: join(skillsDir || join(root, "manager", "skills"), name),
    enabled: true,
    tags: ["common"],
    source_type: "local",
    source_ref: join(root, "source", name),
    preset_ids: ["p1"],
    presets: ["每日常用"],
    deployed_to: ["claude_code"],
    ...extra,
  };
}

function setup(base = tmpdir()) {
  const root = mkdtempSync(join(base, "pihub-smm-"));
  const manager = join(root, "manager");
  mkdirSync(join(manager, "skills"), { recursive: true });
  const home = join(root, "home");
  mkdirSync(home, { recursive: true });
  const bin = join(root, "bin");
  mkdirSync(bin);
  const cli = join(bin, "skills-manager-cli");
  writeFileSync(cli, FIXTURE_CLI, { mode: 0o755 });
  chmodSync(cli, 0o755);
  writeSkill(join(manager, "skills", "alpha"), "alpha", "Alpha body.");
  writeSkill(join(manager, "skills", "beta"), "beta", "Beta body.");
  mkdirSync(join(root, "outside", "evil"), { recursive: true });
  writeSkill(join(root, "outside", "evil"), "evil", "Outside body.");

  const context = {
    root,
    home,
    manager,
    cli,
    outside: join(root, "outside"),
    calls: join(root, "calls.log"),
    catalogPath: join(root, "catalog.json"),
    statePath: join(root, "state.json"),
  };
  writeCatalog(context, ["alpha", "beta"]);
  const connection = new SkillManagerConnection({
    home,
    cliPath: cli,
    env: { ...process.env, HOME: home, PATH: `${bin}${process.platform === "win32" ? ";" : ":"}${process.env.PATH || ""}`, FIXTURE_MANAGER: root },
    platform: "darwin",
    timeoutMs: 10_000,
  });
  const registry = new SkillRegistry({ home });
  return { ...context, connection, registry };
}

function writeCatalog(context, names, { extra = {}, agents, skillFiles, skillsDir } = {}) {
  const skills = names.map((name) => catalogEntry(context.root, name, extra[name] || {}, skillsDir));
  const catalog = {
    skills,
    presets: [
      { id: "p1", name: "每日常用", description: null, icon: "briefcase", sort_order: 0, skill_count: skills.length, active: true },
      { id: "p2", name: "Lark cli skills", description: null, icon: "notebook-pen", sort_order: 9, skill_count: 0, active: false },
    ],
    agents: agents || [
      { key: "claude_code", display_name: "Claude Code", installed: true, skills_dir: "/tmp/claude-skills", enabled: true, is_custom: false, has_path_override: false, project_relative_skills_dir: ".claude/skills", has_project_path_override: false, category: "coding" },
    ],
  };
  if (skillFiles) catalog.skill_files = skillFiles;
  writeFileSync(context.catalogPath, `${JSON.stringify(catalog, null, 2)}\n`);
  return catalog;
}

function setState(context, state) {
  writeFileSync(context.statePath, `${JSON.stringify(state, null, 2)}\n`);
}

function calls(context) {
  if (!existsSync(context.calls)) return [];
  return readFileSync(context.calls, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

function cleanup(root) {
  function thaw(dir) {
    chmodSync(dir, 0o700);
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (entry.isDirectory()) thaw(join(dir, entry.name));
    }
  }
  thaw(root);
  rmSync(root, { recursive: true, force: true });
}

function listTree(dir, prefix = "") {
  const result = [];
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
    result.push(relativePath);
    if (entry.isDirectory()) result.push(...listTree(join(dir, entry.name), relativePath));
  }
  return result;
}

async function expectCode(promise, code) {
  await assert.rejects(promise, (error) => {
    assert.ok(error instanceof ManagerConnectionError, `expected ManagerConnectionError, got ${error?.name}`);
    assert.equal(error.code, code);
    return true;
  });
}

test("connect records an explicit connection in the PiHub home and only queries the manager", async () => {
  const context = setup();
  try {
    const managerBefore = listTree(context.manager);
    const result = await context.connection.connect({ source: "integration-test" });
    assert.equal(result.connected, true);
    assert.equal(result.config.source, "integration-test");
    assert.equal(result.snapshot.skillCount, 2);
    assert.match(result.snapshot.digest, /^sha256:[a-f0-9]{64}$/);
    assert.equal(result.snapshot.mode, "cli");
    assert.deepEqual(listTree(context.manager), managerBefore, "manager library must not be modified");
    assert.equal(existsSync(join(context.manager, "skills-manager.db")), false, "PiHub must not create manager state");

    const config = JSON.parse(readFileSync(join(context.home, "skills", "manager-connection.json"), "utf8"));
    assert.equal(config.schemaVersion, 1);
    assert.equal(config.managerSkillsDir, join(context.manager, "skills"));
    assert.equal(config.managerVersion, "1.41.0");
    if (process.platform !== "win32") {
      assert.equal(statSync(join(context.home, "skills", "manager-connection.json")).mode & 0o777, 0o600);
      assert.equal(statSync(join(context.home, "skills", "manager-catalog.json")).mode & 0o777, 0o600);
    }

    const commands = calls(context).map((args) => args.filter((arg) => arg !== "--json").join(" "));
    assert.ok(commands.length > 0);
    for (const command of commands) {
      assert.ok(command === "--version" || READ_ONLY_COMMANDS.includes(command), `unexpected manager command: ${command}`);
    }
    assert.ok(!commands.some((command) => /add-custom|deploy|undeploy|sync|set-path/.test(command)));
  } finally {
    cleanup(context.root);
  }
});

test("status exposes the manager source, freshness, and missing sources", async () => {
  const context = setup();
  try {
    const before = await context.connection.status({ live: true });
    assert.equal(before.connected, false);
    assert.equal(before.manager.available, true);
    assert.equal(before.manager.version, "1.41.0");

    await context.connection.connect();
    const status = await context.connection.status({ live: true });
    assert.equal(status.connected, true);
    assert.equal(status.snapshot.stale, false);
    assert.equal(status.snapshot.managerSkillsDir, join(context.manager, "skills"));
    assert.equal(status.source.skillsDirOk, true);
    assert.deepEqual(status.source.missing, []);
    assert.deepEqual(status.drift.added, []);
    assert.deepEqual(status.drift.removed, []);
    assert.equal(status.drift.changed, false);

    writeSkill(join(context.manager, "skills", "gamma"), "gamma", "Gamma body.");
    writeCatalog(context, ["alpha", "beta", "gamma"]);
    const drifted = await context.connection.status({ live: true });
    assert.deepEqual(drifted.drift.added, ["gamma"]);
    assert.equal(drifted.drift.changed, true);

    rmSync(join(context.manager, "skills", "beta"), { recursive: true, force: true });
    const missing = await context.connection.status();
    assert.equal(missing.snapshot.stale, true);
    assert.deepEqual(missing.snapshot.missingSources, ["beta"]);
    assert.match(missing.snapshot.staleReason, /missing/);
  } finally {
    cleanup(context.root);
  }
});

test("listSkills filters by manager preset, tag, and query", async () => {
  const context = setup();
  try {
    writeCatalog(context, ["alpha", "beta"], { extra: { beta: { tags: ["lark"], preset_ids: ["p2"], presets: ["Lark cli skills"] } } });
    await context.connection.connect();
    assert.deepEqual(context.connection.listSkills({ preset: "每日常用" }).map((skill) => skill.id), ["alpha"]);
    assert.deepEqual(context.connection.listSkills({ preset: "p2" }).map((skill) => skill.id), ["beta"]);
    assert.deepEqual(context.connection.listSkills({ tag: "lark" }).map((skill) => skill.id), ["beta"]);
    assert.deepEqual(context.connection.listSkills({ query: "BETA" }).map((skill) => skill.id), ["beta"]);
    assert.deepEqual(context.connection.listSkills({ deployedTo: "claude_code" }).map((skill) => skill.id), ["alpha", "beta"]);
    assert.equal(context.connection.listSkills().length, 2);
  } finally {
    cleanup(context.root);
  }
});

test("live drift does not report an in-scope unavailable entry as an addition", async () => {
  const context = setup();
  try {
    const broken = join(context.manager, "skills", "broken");
    mkdirSync(broken, { recursive: true });
    writeCatalog(context, ["alpha", "broken"], { extra: { broken: { path: broken } } });
    setState(context, { showFail: ["broken"] });
    await context.connection.connect({ selection: { presetIds: ["p1"] } });
    const status = await context.connection.status({ live: true });
    assert.deepEqual(status.snapshot.invalidSources.map((entry) => entry.id), ["broken"]);
    assert.deepEqual(status.drift.added, []);
    assert.deepEqual(status.drift.catalogUnavailable, ["broken"]);
    assert.equal(status.drift.changed, false);

    writeSkill(join(context.manager, "skills", "gamma"), "gamma", "Gamma body.");
    writeCatalog(context, ["alpha", "broken", "gamma"], { extra: { broken: { path: broken } } });
    const drifted = await context.connection.status({ live: true });
    assert.deepEqual(drifted.drift.added, ["gamma"]);
    assert.equal(drifted.drift.changed, true);
  } finally {
    cleanup(context.root);
  }
});

test("connect honours an explicit preset selection and rejects unknown selections", async () => {
  const context = setup();
  try {
    writeCatalog(context, ["alpha", "beta"], { extra: { beta: { tags: ["lark"], preset_ids: ["p2"], presets: ["Lark cli skills"] } } });
    const result = await context.connection.connect({ selection: { presetIds: ["p2"] } });
    assert.equal(result.snapshot.skillCount, 1);
    assert.deepEqual(context.connection.listSkills().map((skill) => skill.id), ["beta"]);
    assert.deepEqual(result.config.selection, { presetIds: ["p2"], tags: [], ids: [] });

    await expectCode(context.connection.connect({ selection: { presetIds: ["nope"] } }), "invalid_manager_selection");
    await expectCode(context.connection.connect({ selection: { ids: ["missing-skill"] } }), "invalid_manager_selection");
    await expectCode(context.connection.connect({ selection: { bogus: ["x"] } }), "invalid_manager_selection");
    await expectCode(context.connection.connect({ agentKey: "no_such_agent" }), "invalid_manager_agent");
  } finally {
    cleanup(context.root);
  }
});

test("resolveSources validates containment and reports removed sources instead of silently succeeding", async () => {
  const context = setup();
  try {
    await context.connection.connect();
    const all = context.connection.resolveSources({ mode: "all" });
    assert.deepEqual(all.map((source) => source.id), ["alpha", "beta"]);
    // sourcePath is always the canonical (realpath) directory, so a system alias
    // such as macOS /tmp -> /private/tmp does not change what gets frozen.
    assert.equal(all[0].sourcePath, realpathSync(join(context.manager, "skills", "alpha")));
    assert.deepEqual(context.connection.resolveSources({ mode: "only", ids: ["beta"] }).map((source) => source.id), ["beta"]);
    await expectCode(Promise.resolve().then(() => context.connection.resolveSources({ mode: "only", ids: ["ghost"] })), "skill_not_found");

    rmSync(join(context.manager, "skills", "alpha"), { recursive: true, force: true });
    await expectCode(Promise.resolve().then(() => context.connection.resolveSources({ mode: "all" })), "manager_source_missing");
  } finally {
    cleanup(context.root);
  }
});

test("a synced add or remove changes what new subagents may freeze, never an existing authorization", async () => {
  const context = setup();
  try {
    await context.connection.connect();
    const first = await context.connection.resolveAuthorization({ mode: "all" }, { registry: context.registry });
    assert.equal(first.mode, "only");
    assert.deepEqual(first.grants.map((grant) => grant.id), ["alpha", "beta"]);
    for (const grant of first.grants) {
      assert.equal(readFileSync(join(grant.path, "SKILL.md"), "utf8").includes("body"), true);
      assert.equal(grant.path.startsWith(join(context.home, "skills", "objects")), true);
    }
    assert.equal(context.registry.verifyAuthorization(first), true);

    writeSkill(join(context.manager, "skills", "gamma"), "gamma", "Gamma body.");
    writeCatalog(context, ["alpha", "beta", "gamma"]);
    const added = await context.connection.sync();
    assert.equal(added.synced, true);
    assert.deepEqual(added.added, ["gamma"]);
    const second = await context.connection.resolveAuthorization({ mode: "all" }, { registry: context.registry });
    assert.deepEqual(second.grants.map((grant) => grant.id), ["alpha", "beta", "gamma"]);
    assert.equal(context.registry.verifyAuthorization(first), true, "existing agent keeps its frozen grants");

    writeFileSync(join(context.manager, "skills", "alpha", "SKILL.md"), "---\nname: alpha\n---\nAlpha v2 body.\n");
    writeCatalog(context, ["alpha", "gamma"]);
    rmSync(join(context.manager, "skills", "beta"), { recursive: true, force: true });
    const removed = await context.connection.sync();
    assert.deepEqual(removed.removed, ["beta"]);
    assert.equal(removed.changed, true);
    assert.equal(context.registry.verifyAuthorization(first), true);
    const third = await context.connection.resolveAuthorization({ mode: "all" }, { registry: context.registry });
    assert.deepEqual(third.grants.map((grant) => grant.id), ["alpha", "gamma"]);
    const alphaGrant = third.grants.find((grant) => grant.id === "alpha");
    assert.notEqual(alphaGrant.sha256, first.grants.find((grant) => grant.id === "alpha").sha256);
  } finally {
    cleanup(context.root);
  }
});

test("a failed sync keeps the last complete snapshot and marks the connection stale", async () => {
  const context = setup();
  try {
    await context.connection.connect();
    const snapshotPath = join(context.home, "skills", "manager-catalog.json");
    const before = readFileSync(snapshotPath, "utf8");

    setState(context, { fail: "skills" });
    const failed = await context.connection.sync({ force: true });
    assert.equal(failed.synced, false);
    assert.equal(failed.error.code, "manager_query_failed");
    assert.equal(failed.keptSnapshot.skillCount, 2);
    assert.equal(readFileSync(snapshotPath, "utf8"), before, "snapshot file must be untouched");

    setState(context, { fail: "repo" });
    const repoFailed = await context.connection.sync();
    assert.equal(repoFailed.synced, false);
    assert.equal(repoFailed.keptSnapshot.digest, failed.keptSnapshot.digest);

    setState(context, {});
    const recovered = await context.connection.sync();
    assert.equal(recovered.synced, true);
    assert.equal(recovered.snapshot.skillCount, 2);
  } finally {
    cleanup(context.root);
  }
});

test("disconnect drops the connection, keeps the snapshot, and is idempotent", async () => {
  const context = setup();
  try {
    assert.equal(context.connection.disconnect().alreadyDisconnected, true);
    await context.connection.connect();
    const authorization = await context.connection.resolveAuthorization({ mode: "only", ids: ["alpha"] }, { registry: context.registry });

    const first = context.connection.disconnect();
    assert.deepEqual([first.connected, first.snapshotKept, first.alreadyDisconnected], [false, true, false]);
    assert.equal(existsSync(join(context.home, "skills", "manager-connection.json")), false);
    assert.equal(existsSync(join(context.home, "skills", "manager-catalog.json")), true);
    assert.equal(context.registry.verifyAuthorization(authorization), true, "frozen copies survive disconnect");
    await expectCode(Promise.resolve().then(() => context.connection.resolveSources({ mode: "all" })), "manager_not_connected");
    assert.equal(context.connection.disconnect().alreadyDisconnected, true);
    assert.equal(context.connection.disconnect({ purgeSnapshot: true }).snapshotKept, false);
    assert.equal(existsSync(join(context.home, "skills", "manager-catalog.json")), false);
    assert.equal(context.registry.verifyAuthorization(authorization), true);
  } finally {
    cleanup(context.root);
  }
});

test("rejects catalog entries that escape the manager library or hide behind symlinks", async () => {
  const context = setup();
  try {
    writeCatalog(context, ["alpha"], { extra: { alpha: { path: join(context.outside, "evil") } } });
    await expectCode(context.connection.connect(), "unsafe_manager_skill_path");

    writeCatalog(context, ["alpha"]);
    rmSync(join(context.manager, "skills", "alpha"), { recursive: true, force: true });
    symlinkSync(join(context.outside, "evil"), join(context.manager, "skills", "alpha"));
    await expectCode(context.connection.connect(), "unsafe_manager_skill_path");

    writeCatalog(context, ["alpha"], { extra: { alpha: { path: join(context.manager, "skills", "gone") } } });
    await expectCode(context.connection.connect(), "manager_source_missing");
  } finally {
    cleanup(context.root);
  }
});

test("an importable catalog entry can coexist with unusable ones, which stay visible", async () => {
  const context = setup();
  try {
    writeSkill(join(context.manager, "skills", "broken"), "broken", "Broken body.");
    rmSync(join(context.manager, "skills", "broken", "SKILL.md"), { force: true });
    writeCatalog(context, ["alpha", "broken"]);
    const connected = await context.connection.connect();
    assert.equal(connected.snapshot.skillCount, 1);
    assert.equal(connected.snapshot.skippedCount, 1);
    assert.deepEqual(connected.snapshot.invalidSources.map((entry) => entry.id), ["broken"]);
    assert.equal(connected.snapshot.stale, true);
    assert.deepEqual(context.connection.listSkills().map((skill) => skill.id), ["alpha"]);
    await expectCode(Promise.resolve().then(() => context.connection.resolveSources({ mode: "only", ids: ["broken"] })), "invalid_manager_skill");
    await expectCode(context.connection.connect({ selection: { ids: ["broken"] } }), "invalid_manager_skill");
  } finally {
    cleanup(context.root);
  }
});

test("refuses symlinked connection files and reports corrupt state without throwing", async () => {
  const context = setup();
  try {
    mkdirSync(join(context.home, "skills"), { recursive: true });
    const sentinel = join(context.root, "sentinel.json");
    writeFileSync(sentinel, "{}\n");
    symlinkSync(sentinel, join(context.home, "skills", "manager-connection.json"));
    await expectCode(context.connection.connect(), "unsafe_manager_path");
    assert.equal(readFileSync(sentinel, "utf8"), "{}\n", "symlink target must not be overwritten");

    rmSync(join(context.home, "skills", "manager-connection.json"));
    writeFileSync(join(context.home, "skills", "manager-connection.json"), "not json\n");
    const status = await context.connection.status();
    assert.equal(status.connected, false);
    assert.equal(status.error.code, "invalid_manager_config");
  } finally {
    cleanup(context.root);
  }
});

test("manual registration stays a user action and targets only the PiHub connection directory", async () => {
  const context = setup();
  try {
    await context.connection.connect();
    const registration = context.connection.manualRegistrationCommand({ key: "pihub", displayName: "PiHub" });
    assert.equal(registration.executed, false);
    assert.equal(registration.targetSkillsDir, join(context.home, "skills", "manager-target"));
    assert.deepEqual(registration.argv.slice(1, 5), ["agents", "add-custom", "pihub", "--path"]);
    assert.equal(registration.argv[5], registration.targetSkillsDir);
    const commands = calls(context).map((args) => args.join(" "));
    assert.ok(!commands.some((command) => command.includes("add-custom")), "PiHub must never register itself");
    await expectCode(Promise.resolve().then(() => context.connection.manualRegistrationCommand({ key: "../evil" })), "invalid_manager_agent");
  } finally {
    cleanup(context.root);
  }
});

test("discoverManager resolves an explicit CLI and rejects an unusable one", async () => {
  const context = setup();
  try {
    const fixtureEnv = { ...process.env, HOME: context.home, FIXTURE_MANAGER: context.root };
    const found = await discoverManager({ cliPath: context.cli, env: fixtureEnv, platform: "darwin" });
    assert.equal(found.available, true);
    assert.equal(found.version, "1.41.0");
    assert.equal(found.versionSupported, true);

    setState(context, { version: "skills-manager-cli 1.2.0" });
    const old = await discoverManager({ cliPath: context.cli, env: fixtureEnv, platform: "darwin" });
    assert.equal(old.available, true);
    assert.equal(old.versionSupported, false);
    await expectCode(context.connection.connect(), "manager_unavailable");

    const missing = await discoverManager({ cliPath: join(context.root, "nope"), env: { ...fixtureEnv, PATH: "" }, platform: "linux" });
    assert.equal(missing.available, false);
  } finally {
    cleanup(context.root);
  }
});

test("accepts a system alias for the manager library without weakening symlink checks", async () => {
  const context = setup();
  try {
    // Same shape as macOS /tmp -> /private/tmp: an ancestor directory is a symlink,
    // while the library root reported by the manager is a real directory.
    const realBase = join(context.root, "real-base");
    mkdirSync(join(realBase, "manager", "skills"), { recursive: true });
    writeSkill(join(realBase, "manager", "skills", "alpha"), "alpha", "Alpha body.");
    writeSkill(join(realBase, "manager", "skills", "beta"), "beta", "Beta body.");
    const aliasBase = join(context.root, "alias-base");
    symlinkSync(realBase, aliasBase);
    const aliasedSkills = join(aliasBase, "manager", "skills");
    const realSkills = join(realBase, "manager", "skills");
    setState(context, { skillsDir: aliasedSkills });
    writeCatalog(context, ["alpha", "beta"], { skillsDir: aliasedSkills });

    const connected = await context.connection.connect();
    assert.equal(connected.snapshot.skillCount, 2);
    assert.equal(connected.config.managerSkillsDir, aliasedSkills, "the literal root is recorded as reported");

    const sources = context.connection.resolveSources({ mode: "all" });
    assert.deepEqual(sources.map((source) => source.id), ["alpha", "beta"]);
    assert.deepEqual(sources.map((source) => source.sourcePath), [
      realpathSync(join(realSkills, "alpha")),
      realpathSync(join(realSkills, "beta")),
    ], "frozen sources are canonical, not alias-relative");

    const authorization = await context.connection.resolveAuthorization({ mode: "all" }, { registry: context.registry });
    assert.equal(context.registry.verifyAuthorization(authorization), true);

    // A symlink *inside* the library is still refused.
    rmSync(join(realSkills, "alpha"), { recursive: true, force: true });
    symlinkSync(join(context.outside, "evil"), join(realSkills, "alpha"));
    await expectCode(Promise.resolve().then(() => context.connection.resolveSources({ mode: "only", ids: ["alpha"] })), "unsafe_manager_skill_path");
  } finally {
    cleanup(context.root);
  }
});

test("the real /tmp system alias resolves to the same canonical library", async (t) => {
  if (!existsSync("/private/tmp") || realpathSync("/tmp") !== "/private/tmp") {
    t.skip("/tmp is not a symlinked alias on this platform");
    return;
  }
  const context = setup("/tmp");
  try {
    const connected = await context.connection.connect({ selection: { ids: ["alpha"] } });
    assert.equal(connected.snapshot.skillCount, 1);
    const sources = context.connection.resolveSources({ mode: "all" });
    assert.equal(sources.length, 1);
    assert.equal(sources[0].sourcePath, realpathSync(join(context.manager, "skills", "alpha")));
    assert.ok(sources[0].sourcePath.startsWith("/private/tmp/"), `expected canonical /private/tmp path, got ${sources[0].sourcePath}`);
    const authorization = await context.connection.resolveAuthorization({ mode: "all" }, { registry: context.registry });
    assert.equal(context.registry.verifyAuthorization(authorization), true);
  } finally {
    cleanup(context.root);
  }
});

test("resolves a manager-reported nested SKILL.md entry and freezes it", async () => {
  const context = setup();
  try {
    const bundle = join(context.manager, "skills", "nested-bundle");
    writeSkill(join(bundle, "standards"), "nested-bundle", "Nested standards body.");
    writeCatalog(context, ["alpha", "nested-bundle"], {
      extra: { "nested-bundle": { path: bundle } },
      skillFiles: { "nested-bundle": join(bundle, "standards", "SKILL.md") },
    });
    const connected = await context.connection.connect({ selection: { ids: ["nested-bundle"] } });
    assert.equal(connected.snapshot.skillCount, 1);
    assert.equal(connected.snapshot.stale, false);
    assert.deepEqual(connected.snapshot.invalidSources, []);
    const sources = context.connection.resolveSources({ mode: "all" });
    assert.equal(sources[0].sourcePath, realpathSync(join(bundle, "standards")));
    const authorization = await context.connection.resolveAuthorization({ mode: "all" }, { registry: context.registry });
    assert.deepEqual(authorization.grants.map((grant) => grant.id), ["nested-bundle"]);
    assert.match(readFileSync(join(authorization.grants[0].path, "SKILL.md"), "utf8"), /Nested standards body/);
  } finally {
    cleanup(context.root);
  }
});

test("an unusable entry outside the selected scope is a catalog warning, not a stale connection", async () => {
  const context = setup();
  try {
    const broken = join(context.manager, "skills", "broken");
    mkdirSync(broken, { recursive: true });
    writeCatalog(context, ["alpha", "broken"], { extra: { broken: { path: broken } } });
    setState(context, { showFail: ["broken"] });

    const connected = await context.connection.connect({ selection: { ids: ["alpha"] } });
    assert.equal(connected.snapshot.skillCount, 1);
    assert.equal(connected.snapshot.stale, false);
    assert.equal(connected.snapshot.freshness.stale, false);
    assert.deepEqual(connected.snapshot.invalidSources, []);
    assert.deepEqual(connected.snapshot.warnings.map((entry) => entry.id), ["broken"]);
    assert.equal(connected.snapshot.availability.warningCount, 1);

    await expectCode(context.connection.connect({ selection: { ids: ["broken"] } }), "invalid_manager_skill");
  } finally {
    cleanup(context.root);
  }
});

test("explicit ids fail fast on a broken entry, while preset scope surfaces it per item", async () => {
  const context = setup();
  try {
    const bundle = join(context.manager, "skills", "nested-bundle");
    writeSkill(join(bundle, "standards"), "nested-bundle", "Nested body.");
    const broken = join(context.manager, "skills", "broken");
    mkdirSync(broken, { recursive: true });
    writeCatalog(context, ["nested-bundle", "broken"], {
      extra: { "nested-bundle": { path: bundle }, broken: { path: broken } },
      skillFiles: { "nested-bundle": join(bundle, "standards", "SKILL.md") },
    });
    setState(context, { showFail: ["broken"] });

    await expectCode(context.connection.connect({ selection: { ids: ["nested-bundle", "broken"] } }), "invalid_manager_skill");

    const scoped = await context.connection.connect({ selection: { presetIds: ["p1"] } });
    assert.deepEqual(scoped.snapshot.selectedIds, ["nested-bundle"]);
    assert.deepEqual(scoped.snapshot.invalidSources.map((entry) => entry.id), ["broken"]);
    assert.equal(scoped.snapshot.stale, true);
    assert.match(scoped.snapshot.staleReason, /not importable/);
    await expectCode(Promise.resolve().then(() => context.connection.resolveSources({ mode: "only", ids: ["broken"] })), "invalid_manager_skill");
  } finally {
    cleanup(context.root);
  }
});

test("rejects skill_file entries that escape the manager library or cross symlinks", async () => {
  const context = setup();
  try {
    writeCatalog(context, ["alpha"], {
      extra: { alpha: { path: join(context.manager, "skills", "alpha-bundle") } },
      skillFiles: { alpha: join(context.outside, "evil", "SKILL.md") },
    });
    await expectCode(context.connection.connect(), "unsafe_manager_skill_path");

    mkdirSync(join(context.manager, "skills", "alpha-bundle", "standards"), { recursive: true });
    symlinkSync(join(context.outside, "evil"), join(context.manager, "skills", "alpha-bundle", "linked"));
    writeCatalog(context, ["alpha"], {
      extra: { alpha: { path: join(context.manager, "skills", "alpha-bundle") } },
      skillFiles: { alpha: join(context.manager, "skills", "alpha-bundle", "linked", "SKILL.md") },
    });
    await expectCode(context.connection.connect(), "unsafe_manager_skill_path");
  } finally {
    cleanup(context.root);
  }
});

test("real skills-manager CLI is discoverable on this host (read-only version probe)", async (t) => {
  const found = await discoverManager({ env: process.env, platform: "darwin" });
  if (!found.available) {
    t.skip(`real skills-manager CLI not installed here: ${found.reason}`);
    return;
  }
  assert.match(found.version, /^\d+\.\d+\.\d+$/);
  assert.equal(found.cliPath.endsWith("skills-manager-cli"), true);
});
