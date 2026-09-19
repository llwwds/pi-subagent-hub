import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { StrictJsonlParser } from "../src/jsonl.js";
import { resolveHubPaths } from "../src/paths.js";

const paths = resolveHubPaths();
const temp = mkdtempSync(join(tmpdir(), "pihub-pi-smoke-"));
const agentDir = join(temp, "agent");
const sessionDir = join(temp, "sessions");
mkdirSync(agentDir, { recursive: true, mode: 0o700 });
mkdirSync(sessionDir, { recursive: true, mode: 0o700 });
writeFileSync(join(agentDir, "settings.json"), '{"enableInstallTelemetry":false,"defaultProjectTrust":"never"}\n', { mode: 0o600 });

const child = spawn(process.execPath, [
  paths.piCliPath,
  "--mode", "rpc",
  "--session-dir", sessionDir,
  "--session-id", randomUUID(),
  "--name", "pihub-smoke",
  "--no-skills",
  "--no-extensions",
  "--tools", "read",
  "--no-approve",
], {
  cwd: temp,
  env: {
    ...process.env,
    PI_CODING_AGENT_DIR: agentDir,
    PI_CODING_AGENT_SESSION_DIR: sessionDir,
    PI_OFFLINE: "1",
    PI_TELEMETRY: "0",
    PI_SKIP_VERSION_CHECK: "1",
  },
  stdio: ["pipe", "pipe", "pipe"],
});

let stderr = "";
child.stderr.on("data", (chunk) => { stderr += chunk.toString("utf8"); });
const result = await new Promise((resolvePromise, reject) => {
  const timeout = setTimeout(() => reject(new Error(`Pi RPC smoke timed out: ${stderr}`)), 20_000);
  const parser = new StrictJsonlParser({
    onValue(value) {
      if (value.type === "response" && value.id === "smoke") {
        clearTimeout(timeout);
        resolvePromise(value);
      }
    },
    onError(error, line) { reject(new Error(`invalid Pi JSONL: ${error.message}: ${line}`)); },
  });
  child.stdout.on("data", (chunk) => parser.push(chunk));
  child.on("exit", (code, signal) => {
    if (code !== 0 && code !== null) reject(new Error(`Pi exited early code=${code} signal=${signal}: ${stderr}`));
  });
  child.stdin.write(`${JSON.stringify({ id: "smoke", type: "get_state" })}\n`);
});

child.kill("SIGTERM");
await new Promise((resolvePromise) => child.once("exit", resolvePromise));
rmSync(temp, { recursive: true, force: true });
process.stdout.write(`${JSON.stringify({ ok: result.success === true, command: result.command, piCli: paths.piCliPath })}\n`);
