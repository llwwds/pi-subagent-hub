import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { resolveProfile } from "../src/profile.js";

test("profile digest is deterministic and changes with resource content", () => {
  const root = mkdtempSync(join(tmpdir(), "pihub-profile-"));
  const skill = join(root, "skill");
  mkdirSync(skill);
  writeFileSync(join(skill, "SKILL.md"), "one\n");
  const first = resolveProfile({ schemaVersion: 1, name: "x", tools: ["write", "read", "read"], skills: [skill], extensions: [] });
  const same = resolveProfile({ schemaVersion: 1, name: "x", tools: ["read", "write"], skills: [skill], extensions: [] });
  assert.equal(first.digest, same.digest);
  assert.deepEqual(first.tools, ["read", "write"]);
  writeFileSync(join(skill, "SKILL.md"), "two\n");
  const changed = resolveProfile({ schemaVersion: 1, name: "x", tools: ["read", "write"], skills: [skill], extensions: [] });
  assert.notEqual(first.digest, changed.digest);
  rmSync(root, { recursive: true, force: true });
});
