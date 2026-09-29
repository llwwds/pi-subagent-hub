import { accessSync, constants } from "node:fs";
import { delimiter, isAbsolute, join } from "node:path";

function isExecutable(path) {
  try {
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

export function findExecutable(nameOrPath, env = process.env) {
  if (isAbsolute(nameOrPath)) return isExecutable(nameOrPath) ? nameOrPath : null;
  if (nameOrPath.includes("/")) return null;
  for (const directory of String(env.PATH || "").split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, nameOrPath);
    if (isExecutable(candidate)) return candidate;
  }
  return null;
}
