import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const sourceLock = JSON.parse(readFileSync(join(root, "vendor", "pi.lock.json"), "utf8"));
const runtimeLock = JSON.parse(readFileSync(join(root, "vendor", "pi-runtime.lock.json"), "utf8"));
const sourceDir = join(root, "vendor", "pi");
const runtimeDir = join(root, "vendor", "pi-runtime");

if (!existsSync(join(sourceDir, ".git"))) {
  mkdirSync(join(root, "vendor"), { recursive: true });
  execFileSync("git", ["clone", "--branch", sourceLock.tag, "--single-branch", sourceLock.upstream, sourceDir], { stdio: "inherit" });
}
const head = execFileSync("git", ["-C", sourceDir, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
const tag = execFileSync("git", ["-C", sourceDir, "describe", "--tags", "--exact-match", "HEAD"], { encoding: "utf8" }).trim();
const status = execFileSync("git", ["-C", sourceDir, "status", "--short"], { encoding: "utf8" }).trim();
if (head !== sourceLock.commit || tag !== sourceLock.tag || status) {
  throw new Error(`vendor/pi does not match lock (head=${head}, tag=${tag}, dirty=${Boolean(status)})`);
}

mkdirSync(runtimeDir, { recursive: true });
copyFileSync(join(root, "vendor", "pi-runtime.package.json"), join(runtimeDir, "package.json"));
copyFileSync(join(root, "vendor", "pi-runtime.package-lock.json"), join(runtimeDir, "package-lock.json"));
execFileSync("npm", ["ci", "--no-audit", "--no-fund", `--registry=${runtimeLock.registry}`], {
  cwd: runtimeDir,
  stdio: "inherit",
});

const installedLock = JSON.parse(readFileSync(join(runtimeDir, "package-lock.json"), "utf8"));
for (const [name, expected] of Object.entries(runtimeLock.packages)) {
  const actual = JSON.parse(readFileSync(join(runtimeDir, "node_modules", ...name.split("/"), "package.json"), "utf8")).version;
  if (actual !== expected.version) throw new Error(`${name} version mismatch: expected ${expected.version}, got ${actual}`);
  const locked = installedLock.packages?.[`node_modules/${name}`];
  if (locked?.integrity !== expected.integrity) {
    throw new Error(`${name} integrity mismatch: expected ${expected.integrity}, got ${locked?.integrity || "missing"}`);
  }
}
process.stdout.write(`Pi ${sourceLock.tag} ready: ${head}\n`);
