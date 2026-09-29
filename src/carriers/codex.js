import { join } from "node:path";
import { CliExecRuntime } from "./cli-runtime.js";
import { findExecutable } from "./executable.js";

export class CodexExecCarrierAdapter {
  constructor({ executable = "codex" } = {}) { this.executable = executable; }

  id = "codex";
  displayName = "Codex CLI";
  capabilities = Object.freeze({
    messages: ["prompt", "abort", "clear_queue", "get_state"],
    events: true,
    skillLoadingSelection: true,
    skillIsolation: false,
  });

  diagnose({ paths, env = process.env } = {}) {
    const executable = paths?.codexCliPath || this.executable;
    const found = findExecutable(executable, env);
    if (!found) return { available: false, reason: `Codex CLI executable is unavailable: ${executable}` };
    if (paths?.ccSwitchCliPath && !findExecutable(paths.ccSwitchCliPath, env)) {
      return { available: false, reason: "app-private CC Switch CLI is unavailable" };
    }
    return { available: true };
  }

  createRuntime({ paths, spec, env = process.env } = {}) {
    return new CliExecRuntime({
      carrier: this.id,
      executable: paths?.codexCliPath || this.executable,
      spec: {
        ...spec,
        hubHome: paths?.home || spec?.hubHome,
        hubCliPath: paths?.appRoot ? join(paths.appRoot, "bin", "pihub.js") : spec?.hubCliPath,
        ccSwitch: paths?.ccSwitchCliPath ? {
          executable: paths.ccSwitchCliPath,
          configDir: paths.ccSwitchConfigDir,
          home: paths.ccSwitchHome,
          piDir: paths.ccSwitchPiDir,
          codexCliPath: paths.codexCliPath,
          claudeCliPath: paths.claudeCliPath,
        } : spec?.ccSwitch || null,
      },
      env,
    });
  }
}
