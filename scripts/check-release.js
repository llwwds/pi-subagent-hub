import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { APP_VERSION, RELEASE_CHANNEL } from "../src/constants.js";

const root = resolve(import.meta.dirname, "..");
// This is the explicit legacy 2.1 check. The repository/package version is
// 3.0.0 and the default `pihub` entry is Rust; the Node source stays pinned to
// its accepted 2.1 app version.
const LEGACY_APP_VERSION = "2.1.0.0";
if (APP_VERSION !== LEGACY_APP_VERSION) {
  throw new Error(`legacy version mismatch: app=${APP_VERSION}, expected=${LEGACY_APP_VERSION}`);
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
  if (!readme.includes(APP_VERSION)) {
    throw new Error(`README does not declare legacy development version ${APP_VERSION}`);
  }
} else {
  throw new Error(`unknown release channel: ${RELEASE_CHANNEL}`);
}

process.stdout.write(`release metadata ok: ${APP_VERSION} (${RELEASE_CHANNEL})\n`);
