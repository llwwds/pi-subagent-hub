import { execFileSync } from "node:child_process";
import { readdirSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const roots = ["bin", "src", "scripts", "test", "fixtures"].map((path) => join(root, path));
const files = [];

function walk(path) {
  for (const entry of readdirSync(path)) {
    const child = join(path, entry);
    if (statSync(child).isDirectory()) walk(child);
    else if (child.endsWith(".js")) files.push(child);
  }
}

for (const path of roots) walk(path);
for (const file of files) execFileSync(process.execPath, ["--check", file], { stdio: "pipe" });
process.stdout.write(`syntax ok: ${files.length} JavaScript files\n`);
