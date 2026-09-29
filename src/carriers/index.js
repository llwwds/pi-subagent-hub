import { CarrierRegistry } from "./registry.js";
import { PiCarrierAdapter } from "./pi.js";
import { CodexExecCarrierAdapter } from "./codex.js";
import { ClaudeCodeCarrierAdapter } from "./claude.js";

export { CarrierRegistry, PiCarrierAdapter, CodexExecCarrierAdapter, ClaudeCodeCarrierAdapter };

export function createDefaultCarrierRegistry({ codexCliPath, claudeCliPath } = {}) {
  return new CarrierRegistry([
    new PiCarrierAdapter(),
    new CodexExecCarrierAdapter({ executable: codexCliPath || "codex" }),
    new ClaudeCodeCarrierAdapter({ executable: claudeCliPath || "claude" }),
  ]);
}
