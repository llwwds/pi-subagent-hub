import { existsSync, mkdirSync } from "node:fs";
import { delimiter, dirname, join } from "node:path";
import { PiRpcProcess } from "../rpc-process.js";
import { preparePrivateHubCli } from "./hub-cli.js";

export class PiCarrierAdapter {
  id = "pi";
  displayName = "Pi Agent CLI";
  capabilities = Object.freeze({
    messages: ["prompt", "steer", "follow_up", "abort", "clear_queue", "get_state"],
    events: true,
    skillLoadingSelection: true,
    skillIsolation: false,
  });

  diagnose({ paths } = {}) {
    const path = paths?.piCliPath;
    return path && existsSync(path)
      ? { available: true }
      : { available: false, reason: "Pi CLI bundle is unavailable; build the pinned Pi runtime first" };
  }

  createRuntime({ paths, spec, env } = {}) {
    if (!paths?.piCliPath) throw new Error("Pi CLI path is required");
    const privateHome = join(spec.stateDir, "pi-home");
    mkdirSync(privateHome, { recursive: true, mode: 0o700 });
    const hubBin = preparePrivateHubCli(privateHome, paths?.home, paths?.appRoot && join(paths.appRoot, "bin", "pihub.js"));
    const cleanEnv = {};
    for (const key of ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"]) {
      if (typeof env?.[key] === "string") cleanEnv[key] = env[key];
    }
    cleanEnv.HOME = privateHome;
    if (hubBin) {
      cleanEnv.PIHUB_HOME = paths.home;
      cleanEnv.PATH = [hubBin, dirname(process.execPath), cleanEnv.PATH || "/usr/bin:/bin"].join(delimiter);
    }
    return new PiRpcProcess({ piCliPath: paths.piCliPath, spec, env: cleanEnv });
  }
}
