import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { appendFileSync, createWriteStream } from "node:fs";
import { join } from "node:path";
import { StrictJsonlParser } from "./jsonl.js";
import { newId, nowIso, sleep } from "./utils.js";

export class PiRpcProcess extends EventEmitter {
  constructor({ piCliPath, spec, env = process.env }) {
    super();
    this.piCliPath = piCliPath;
    this.spec = spec;
    this.baseEnv = env;
    this.child = null;
    this.pending = new Map();
    this.protocolLogPath = join(spec.logsDir, "protocol.jsonl");
    this.stderrLogPath = join(spec.logsDir, "stderr.log");
  }

  get pid() {
    return this.child?.pid ?? null;
  }

  get alive() {
    return Boolean(this.child && this.child.exitCode === null && this.child.signalCode === null);
  }

  #args() {
    const args = [
      this.piCliPath,
      "--mode", "rpc",
      "--session-dir", this.spec.sessionDir,
      "--session-id", this.spec.sessionId,
      "--name", this.spec.name,
      "--no-skills",
      "--no-extensions",
      "--tools", this.spec.tools.join(","),
      "--no-approve",
    ];
    for (const skill of this.spec.skills) args.push("--skill", skill.path);
    for (const extension of this.spec.extensions) args.push("--extension", extension.path);
    if (this.spec.provider) args.push("--provider", this.spec.provider);
    if (this.spec.model) args.push("--model", this.spec.model);
    if (this.spec.thinking) args.push("--thinking", this.spec.thinking);
    return args;
  }

  async start() {
    if (this.child) throw new Error("Pi process already started");
    const child = spawn(process.execPath, this.#args(), {
      cwd: this.spec.cwd,
      env: {
        ...this.baseEnv,
        PI_CODING_AGENT_DIR: this.spec.agentDir,
        PI_CODING_AGENT_SESSION_DIR: this.spec.sessionDir,
        PIHUB_AGENT_ID: this.spec.id,
        PIHUB_AGENT_STATE_DIR: this.spec.stateDir,
        TMPDIR: this.spec.tmpDir,
        PI_TELEMETRY: "0",
        PI_SKIP_VERSION_CHECK: "1",
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child = child;
    const stderrStream = createWriteStream(this.stderrLogPath, { flags: "a", mode: 0o600 });
    child.stderr.pipe(stderrStream);

    const parser = new StrictJsonlParser({
      onValue: (value) => this.#handleValue(value),
      onError: (error, line) => this.#handleProtocolError(error, line),
    });
    child.stdout.on("data", (chunk) => parser.push(chunk));
    child.stdout.on("end", () => parser.finish());
    child.on("error", (error) => this.emit("process_error", error));
    child.on("exit", (code, signal) => {
      stderrStream.end();
      for (const { reject, timeout } of this.pending.values()) {
        clearTimeout(timeout);
        reject(new Error(`Pi process exited before response (code=${code}, signal=${signal})`));
      }
      this.pending.clear();
      this.emit("exit", { code, signal, at: nowIso() });
    });

    const state = await this.request("get_state", {}, 15_000);
    return state.data;
  }

  #handleValue(value) {
    appendFileSync(this.protocolLogPath, `${JSON.stringify({ at: nowIso(), direction: "out", value })}\n`, { mode: 0o600 });
    if (value?.type === "response" && value.id && this.pending.has(value.id)) {
      const pending = this.pending.get(value.id);
      this.pending.delete(value.id);
      clearTimeout(pending.timeout);
      if (value.success) pending.resolve(value);
      else pending.reject(new Error(value.error || `Pi RPC command failed: ${value.command}`));
      return;
    }
    this.emit("event", value);
  }

  #handleProtocolError(error, line) {
    appendFileSync(this.protocolLogPath, `${JSON.stringify({ at: nowIso(), direction: "error", error: error.message, line })}\n`, { mode: 0o600 });
    this.emit("protocol_error", { error, line });
  }

  request(type, payload = {}, timeoutMs = 30_000) {
    if (!this.alive || !this.child.stdin.writable) return Promise.reject(new Error("Pi process is not running"));
    const id = newId();
    const command = { id, type, ...payload };
    appendFileSync(this.protocolLogPath, `${JSON.stringify({ at: nowIso(), direction: "in", value: command })}\n`, { mode: 0o600 });
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Pi RPC timeout for ${type}`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timeout });
      this.child.stdin.write(`${JSON.stringify(command)}\n`, (error) => {
        if (!error) return;
        clearTimeout(timeout);
        this.pending.delete(id);
        reject(error);
      });
    });
  }

  async stop() {
    if (!this.alive) return;
    try { await this.request("clear_queue", {}, 2_000); } catch {}
    try { await this.request("abort", {}, 5_000); } catch {}
    if (!this.alive) return;
    this.child.kill("SIGTERM");
    for (let index = 0; index < 50 && this.alive; index += 1) await sleep(100);
    if (this.alive) this.child.kill("SIGKILL");
  }
}
