import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SkillRegistry } from "../src/skills/registry.js";

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "pihub-skills-"));
  const source = join(root, "source");
  mkdirSync(source);
  writeFileSync(join(source, "SKILL.md"), "---\nname: sample\n---\nA sample skill.\n");
  mkdirSync(join(source, "scripts"));
  writeFileSync(join(source, "scripts", "run.js"), "export default 1;\n");
  const registry = new SkillRegistry({ home: join(root, "home") });
  return { root, source, registry };
}

function cleanup(path) {
  function thaw(dir) {
    chmodSync(dir, 0o700);
    for (const name of readdirSync(dir, { withFileTypes: true })) {
      if (name.isDirectory()) thaw(join(dir, name.name));
    }
  }
  thaw(path);
  rmSync(path, { recursive: true, force: true });
}

test("imports a private content-addressed copy with provenance, and pins all/only grants", () => {
  const { root, source, registry } = fixture();
  try {
    const first = registry.importSkill({ id: "sample", sourcePath: source, sourceLabel: "user-approved source" });
    assert.match(first.sha256, /^[a-f0-9]{64}$/);
    assert.equal(first.source, "user-approved source");
    assert.equal(first.path, join(root, "home", "skills", "objects", first.sha256));
    assert.equal(readFileSync(join(first.path, "SKILL.md"), "utf8"), readFileSync(join(source, "SKILL.md"), "utf8"));
    assert.equal(registry.listSkills()[0].id, "sample");

    const originalAll = registry.resolveAuthorization();
    const originalOnly = registry.resolveAuthorization({ mode: "only", ids: ["sample"] });
    assert.equal(originalAll.mode, "all");
    assert.deepEqual(originalAll.grants.map((grant) => grant.id), ["sample"]);
    assert.equal(originalOnly.grants[0].sha256, first.sha256);
    assert.equal(registry.verifyAuthorization(originalOnly), true);

    writeFileSync(join(source, "SKILL.md"), "---\nname: sample\n---\nUpdated source.\n");
    assert.match(readFileSync(join(first.path, "SKILL.md"), "utf8"), /A sample skill/);
    const second = registry.importSkill({ id: "sample", sourcePath: source, sourceLabel: null });
    assert.notEqual(second.sha256, first.sha256);
    assert.equal(registry.listSkills()[0].versions.length, 2);
    assert.equal(registry.resolveAuthorization({ mode: "all" }).grants[0].sha256, second.sha256);
    assert.equal(originalAll.grants[0].sha256, first.sha256);
    assert.equal(registry.verifyAuthorization(originalAll), true);
  } finally {
    cleanup(root);
  }
});

test("all only includes skills present when authorization was resolved", () => {
  const { root, source, registry } = fixture();
  try {
    const empty = registry.resolveAuthorization();
    assert.deepEqual(empty.grants, []);
    registry.importSkill({ id: "sample", sourcePath: source });
    assert.deepEqual(empty.grants, []);
    assert.deepEqual(registry.resolveAuthorization().grants.map((grant) => grant.id), ["sample"]);
    assert.deepEqual(registry.resolveAuthorization({ mode: "only", ids: [] }).grants, []);
    assert.throws(() => registry.resolveAuthorization({ mode: "only", ids: ["missing"] }), { code: "skill_not_found" });
    assert.throws(() => registry.resolveAuthorization({ mode: "only", ids: ["sample", "sample"] }), { code: "invalid_skill_policy" });
    assert.throws(() => registry.resolveAuthorization({ mode: "all", ids: [] }), { code: "invalid_skill_policy" });
  } finally {
    cleanup(root);
  }
});

test("rejects traversal, escaped links, runtime state, and credential material", () => {
  const { root, source, registry } = fixture();
  try {
    assert.throws(() => registry.importSkill({ id: "../escape", sourcePath: source }), { code: "invalid_skill_id" });
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: `${source}/../source` }), { code: "unsafe_skill_source" });
    const outside = join(root, "private.txt");
    writeFileSync(outside, "outside");
    symlinkSync(outside, join(source, "scripts", "escape"));
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "unsafe_skill_source" });
    rmSync(join(source, "scripts", "escape"));

    writeFileSync(join(source, "auth.json"), "{}\n");
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "sensitive_skill_content" });
    rmSync(join(source, "auth.json"));
    writeFileSync(join(source, "scripts", "secret.txt"), "-----BEGIN PRIVATE KEY-----\n");
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "sensitive_skill_content" });
    assert.deepEqual(registry.listSkills(), []);
  } finally {
    cleanup(root);
  }
});

test("copies Skill content while excluding runtime dependencies and dotfile credentials", () => {
  const { root, source, registry } = fixture();
  try {
    mkdirSync(join(source, "node_modules"));
    writeFileSync(join(source, "node_modules", "dependency.js"), "export default 1;\n");
    mkdirSync(join(source, "tests"));
    writeFileSync(join(source, "tests", "fake-key.txt"), "-----BEGIN PRIVATE KEY-----\n");
    mkdirSync(join(source, ".signature"));
    writeFileSync(join(source, ".signature", "manifest.json"), "{}\n");
    writeFileSync(join(source, ".todoist-token"), "private-token-placeholder\n");
    writeFileSync(join(source, ".DS_Store"), "desktop metadata\n");
    const imported = registry.importSkill({ id: "sample", sourcePath: source });
    assert.equal(existsSync(join(imported.path, "node_modules")), false);
    assert.equal(existsSync(join(imported.path, "tests")), false);
    assert.equal(existsSync(join(imported.path, ".todoist-token")), false);
    assert.equal(existsSync(join(imported.path, ".DS_Store")), false);
    assert.equal(existsSync(join(imported.path, ".signature", "manifest.json")), true);
  } finally {
    cleanup(root);
  }
});

test("rejects nested Skill manifests and requires the root SKILL.md spelling", () => {
  const { root, source, registry } = fixture();
  try {
    const child = join(source, "scripts", "child");
    mkdirSync(child);
    writeFileSync(join(child, "SKILL.md"), "---\nname: hidden\n---\n");
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "invalid_skill_package" });
    rmSync(join(child, "SKILL.md"));

    writeFileSync(join(child, "sKiLl.Md"), "---\nname: hidden\n---\n");
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "invalid_skill_package" });
    rmSync(join(child, "sKiLl.Md"));

    symlinkSync(join(source, "SKILL.md"), join(child, "SKILL.md"));
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "unsafe_skill_source" });
    rmSync(join(child, "SKILL.md"));

    rmSync(join(source, "SKILL.md"));
    writeFileSync(join(source, "skill.md"), "---\nname: sample\n---\n");
    assert.throws(() => registry.importSkill({ id: "sample", sourcePath: source }), { code: "invalid_skill_package" });
    assert.deepEqual(registry.listSkills(), []);
  } finally {
    cleanup(root);
  }
});

test("rejects a source that overlaps skill storage before staging a copy", () => {
  const { root, registry } = fixture();
  try {
    const home = join(root, "home");
    mkdirSync(home);
    writeFileSync(join(home, "SKILL.md"), "---\nname: recursive\n---\n");
    assert.throws(() => registry.importSkill({ id: "recursive", sourcePath: home }), { code: "unsafe_skill_source" });
    const nested = join(home, "skills", "nested");
    mkdirSync(nested, { recursive: true });
    writeFileSync(join(nested, "SKILL.md"), "---\nname: nested\n---\n");
    assert.throws(() => registry.importSkill({ id: "nested", sourcePath: nested }), { code: "unsafe_skill_source" });
    assert.equal(existsSync(join(home, "skills", "objects")), false);
  } finally {
    cleanup(root);
  }
});

test("detects a changed frozen snapshot before a launch can reuse it", () => {
  const { root, source, registry } = fixture();
  try {
    registry.importSkill({ id: "sample", sourcePath: source });
    const authorization = registry.resolveAuthorization({ mode: "only", ids: ["sample"] });
    chmodSync(authorization.grants[0].path, 0o700);
    chmodSync(join(authorization.grants[0].path, "SKILL.md"), 0o600);
    writeFileSync(join(authorization.grants[0].path, "SKILL.md"), "tampered\n");
    assert.throws(() => registry.verifyAuthorization(authorization), { code: "skill_snapshot_changed" });
  } finally {
    cleanup(root);
  }
});
