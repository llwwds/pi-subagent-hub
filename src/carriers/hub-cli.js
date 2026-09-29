import { existsSync, lstatSync, mkdirSync, readlinkSync, statSync, symlinkSync } from "node:fs";
import { isAbsolute, join } from "node:path";

// A bundled Skill can invoke `pihub`. Route that command back to this platform
// instance instead of letting an agent discover another installed release.
export function preparePrivateHubCli(privateHome, hubHome, hubCliPath) {
  if (!hubHome && !hubCliPath) return null;
  if (!hubHome || !hubCliPath || !isAbsolute(hubHome) || !isAbsolute(hubCliPath)) {
    throw new Error("platform pihub route requires absolute home and CLI paths");
  }
  if (!existsSync(hubCliPath) || !statSync(hubCliPath).isFile()) {
    throw new Error("platform pihub CLI is unavailable");
  }
  const binDir = join(privateHome, ".bin");
  mkdirSync(binDir, { recursive: true, mode: 0o700 });
  const command = join(binDir, "pihub");
  let current = null;
  try { current = lstatSync(command); }
  catch (error) { if (error?.code !== "ENOENT") throw error; }
  if (current) {
    if (!current.isSymbolicLink() || readlinkSync(command) !== hubCliPath) {
      throw new Error("platform pihub command differs from the private route");
    }
  } else {
    symlinkSync(hubCliPath, command, "file");
  }
  return binDir;
}
