import { createHash } from "node:crypto";
import { existsSync, lstatSync, readFileSync, readdirSync, readlinkSync } from "node:fs";
import { basename, join, relative, resolve } from "node:path";
import { DEFAULT_TOOLS } from "./constants.js";
import { atomicWriteJson, canonicalJson, readJson } from "./utils.js";

function hashPath(path) {
  const root = resolve(path);
  if (!existsSync(root)) throw new Error(`profile resource does not exist: ${root}`);
  const hash = createHash("sha256");

  function walk(current) {
    const stat = lstatSync(current);
    const name = relative(root, current) || basename(root);
    if (stat.isSymbolicLink()) {
      hash.update(`L\0${name}\0${readlinkSync(current)}\0`);
      return;
    }
    if (stat.isDirectory()) {
      hash.update(`D\0${name}\0`);
      for (const entry of readdirSync(current).sort()) walk(join(current, entry));
      return;
    }
    if (stat.isFile()) {
      hash.update(`F\0${name}\0${stat.mode & 0o777}\0`);
      hash.update(readFileSync(current));
      hash.update("\0");
    }
  }

  walk(root);
  return { path: root, sha256: hash.digest("hex") };
}

export function ensureDefaultProfile(profilePath) {
  const existing = readJson(profilePath);
  if (existing) return existing;
  const profile = {
    schemaVersion: 1,
    name: "default",
    tools: DEFAULT_TOOLS,
    skills: [],
    extensions: [],
  };
  atomicWriteJson(profilePath, profile);
  return profile;
}

export function resolveProfile(profile) {
  if (!profile || profile.schemaVersion !== 1) throw new Error("unsupported profile schemaVersion");
  const tools = [...new Set((profile.tools || []).map(String))].sort();
  if (tools.length === 0) throw new Error("profile must enable at least one tool");
  const skills = (profile.skills || []).map(hashPath).sort((a, b) => a.path.localeCompare(b.path));
  const extensions = (profile.extensions || []).map(hashPath).sort((a, b) => a.path.localeCompare(b.path));
  const normalized = { schemaVersion: 1, name: String(profile.name || "default"), tools, skills, extensions };
  return {
    ...normalized,
    digest: `sha256:${createHash("sha256").update(canonicalJson(normalized)).digest("hex")}`,
  };
}
