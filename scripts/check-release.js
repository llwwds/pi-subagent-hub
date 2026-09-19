import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { APP_VERSION } from "../src/constants.js";

const root = resolve(import.meta.dirname, "..");
const packageJson = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
const expectedAppVersion = `${packageJson.version}.0`;
if (APP_VERSION !== expectedAppVersion) {
  throw new Error(`version mismatch: app=${APP_VERSION}, package=${packageJson.version}`);
}

for (const path of [
  "LICENSE",
  "THIRD_PARTY_NOTICES.md",
  "vendor/pi.lock.json",
  "vendor/pi-runtime.lock.json",
  "vendor/pi-runtime.package-lock.json",
  `docs/releases/v${APP_VERSION}.md`,
]) {
  if (!existsSync(join(root, path))) throw new Error(`missing release file: ${path}`);
}

const readme = readFileSync(join(root, "README.md"), "utf8");
if (!readme.includes(`当前稳定版：\`${APP_VERSION}\``)) {
  throw new Error(`README does not declare version ${APP_VERSION}`);
}

process.stdout.write(`release metadata ok: ${APP_VERSION}\n`);
