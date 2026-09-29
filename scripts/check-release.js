import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { APP_VERSION, RELEASE_CHANNEL } from "../src/constants.js";

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
]) {
  if (!existsSync(join(root, path))) throw new Error(`missing release file: ${path}`);
}

const readme = readFileSync(join(root, "README.md"), "utf8");
if (RELEASE_CHANNEL === "stable") {
  if (!existsSync(join(root, `docs/releases/v${APP_VERSION}.md`))) {
    throw new Error(`missing release file: docs/releases/v${APP_VERSION}.md`);
  }
  if (!readme.includes(`当前稳定版：\`${APP_VERSION}\``)) {
    throw new Error(`README does not declare stable version ${APP_VERSION}`);
  }
} else if (RELEASE_CHANNEL === "development") {
  if (!readme.includes(`\`${APP_VERSION}\` 平台功能正在源码仓库开发`)) {
    throw new Error(`README does not declare development version ${APP_VERSION}`);
  }
} else {
  throw new Error(`unknown release channel: ${RELEASE_CHANNEL}`);
}

process.stdout.write(`release metadata ok: ${APP_VERSION} (${RELEASE_CHANNEL})\n`);
