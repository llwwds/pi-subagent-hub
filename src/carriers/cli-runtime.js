import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import {
  appendFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readlinkSync,
  symlinkSync,
  createWriteStream,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { basename, delimiter, dirname, join, resolve } from "node:path";
import { StrictJsonlParser } from "../jsonl.js";
import { atomicWriteJson, newId, nowIso, readJson, sleep } from "../utils.js";
import { findExecutable } from "./executable.js";
import { preparePrivateHubCli } from "./hub-cli.js";

const SKILL_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,100}$/;
const CODEX_THINKING = new Set(["minimal", "low", "medium", "high", "xhigh", "max"]);
const CLAUDE_THINKING = new Set(["low", "medium", "high", "xhigh", "max"]);
// dontAsk denies any tool that would prompt. Approve the normal coding tools
// explicitly so a headless Claude turn can edit and run checks.
const CLAUDE_ALLOWED_TOOLS = "Read,Edit,Write,Glob,Grep,Bash,NotebookEdit,WebFetch,WebSearch,Skill";

function makePrivateDirectory(path) {
  mkdirSync(path, { recursive: true, mode: 0o700 });
}

function signalTurn(child, signal, group) {
  if (!child?.pid) return;
  try {
    if (group && process.platform !== "win32") process.kill(-child.pid, signal);
    else child.kill(signal);
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

function skillRoot(skill) {
  if (!skill || typeof skill.path !== "string") throw new Error("skill must have a path");
  const path = resolve(skill.path);
  const root = basename(path) === "SKILL.md" ? dirname(path) : path;
  if (!existsSync(join(root, "SKILL.md"))) throw new Error(`skill has no SKILL.md: ${root}`);
  return root;
}

function mountSkills(skills, destination) {
  makePrivateDirectory(destination);
  const expected = new Map();
  for (const skill of skills || []) {
    const path = skillRoot(skill);
    const name = String(skill.id || basename(path));
    if (!SKILL_NAME.test(name)) throw new Error(`invalid skill id for CLI carrier: ${name}`);
    if (expected.has(name)) throw new Error(`duplicate skill id for CLI carrier: ${name}`);
    expected.set(name, path);
  }
  for (const entry of readdirSync(destination)) {
    if (!expected.has(entry)) throw new Error(`unexpected skill already mounted: ${entry}`);
  }
  for (const [name, path] of expected) {
    const target = join(destination, name);
    if (existsSync(target)) {
      if (!lstatSync(target).isSymbolicLink() || readlinkSync(target) !== path) {
        throw new Error(`skill mount differs from fixed snapshot: ${name}`);
      }
    } else {
      symlinkSync(path, target, "dir");
    }
  }
  return destination;
}

function assistantMessage(text, carrier) {
  return {
    type: "message_end",
    carrier,
    at: nowIso(),
    message: { role: "assistant", content: [{ type: "text", text }] },
  };
}

function errorMessage(value) {
  if (typeof value === "string") return value;
  if (value?.message) return String(value.message);
  return JSON.stringify(value || "unknown error");
}

// A CLI turn is one process. The platform runtime remains alive between turns so
// the manager can resume the same private session without exposing vendor state.
export class CliExecRuntime extends EventEmitter {
  constructor({ carrier, executable, spec, env = process.env }) {
    super();
    if (carrier !== "codex" && carrier !== "claude") throw new Error(`unsupported CLI carrier: ${carrier}`);
    if (!spec?.cwd || !spec?.stateDir || !spec?.logsDir || !spec?.sessionId) {
      throw new Error(`${carrier} carrier requires cwd, stateDir, logsDir, and sessionId`);
    }
    this.carrier = carrier;
    this.executable = executable;
    this.spec = spec;
    this.baseEnv = env;
    this.started = false;
    this.stopping = false;
    this.child = null;
    this.turn = null;
    this.privateHome = join(spec.stateDir, `${carrier}-home`);
    this.sessionPath = join(spec.stateDir, `${carrier}-session.json`);
    this.protocolLogPath = join(spec.logsDir, `${carrier}-protocol.jsonl`);
    this.stderrLogPath = join(spec.logsDir, `${carrier}-stderr.log`);
    this.sessionId = null;
    this.skillsDir = null;
    this.skillMountRoot = null;
    this.turnUsesProcessGroup = Boolean(spec.ccSwitch);
  }

  get pid() { return this.child?.pid ?? null; }
  get alive() { return this.started; }

  #prepareHome() {
    makePrivateDirectory(this.privateHome);
    this.hubBin = preparePrivateHubCli(this.privateHome, this.spec.hubHome, this.spec.hubCliPath);
    makePrivateDirectory(this.spec.tmpDir || join(this.spec.stateDir, "tmp"));
    if (this.carrier === "codex") {
      const codexHome = join(this.privateHome, ".codex");
      makePrivateDirectory(codexHome);
      const workspaceNote = join(this.privateHome, "AGENTS.md");
      const note = `# Hub workspace\n\nThe user-selected project directory is ${JSON.stringify(this.spec.cwd)}. Read its instructions and do project work there.\n`;
      if (existsSync(workspaceNote) && readFileSync(workspaceNote, "utf8") !== note) {
        throw new Error("Codex private workspace note differs from the frozen project path");
      }
      if (!existsSync(workspaceNote)) writeFileSync(workspaceNote, note, { mode: 0o600, flag: "wx" });
      this.skillsDir = mountSkills(this.spec.skills, join(this.privateHome, ".agents", "skills"));
    } else {
      const claudeHome = join(this.privateHome, ".claude");
      makePrivateDirectory(claudeHome);
      // In bare mode Claude only discovers <added-dir>/.claude/skills/*.
      // Keep this add-dir separate from the private config and credential home.
      this.skillMountRoot = join(this.spec.stateDir, "claude-skill-mount");
      this.skillsDir = mountSkills(this.spec.skills, join(this.skillMountRoot, ".claude", "skills"));
    }
    if (this.spec.ccSwitch) {
      for (const path of [this.spec.ccSwitch.configDir, this.spec.ccSwitch.home, this.spec.ccSwitch.piDir,
        join(this.privateHome, ".codex"), join(this.privateHome, ".claude")]) makePrivateDirectory(path);
    }
  }

  #state() {
    return {
      sessionId: this.sessionId || this.spec.sessionId,
      sessionFile: this.sessionPath,
      model: { provider: this.spec.provider || (this.carrier === "codex" ? "openai" : "anthropic"), id: this.spec.model || "unknown" },
      thinkingLevel: this.spec.thinking || null,
      isStreaming: Boolean(this.child),
    };
  }

  async start() {
    if (this.started) throw new Error(`${this.carrier} runtime already started`);
    const executable = findExecutable(this.executable, this.baseEnv);
    if (!executable) throw new Error(`${this.carrier} CLI executable is unavailable: ${this.executable}`);
    this.executable = executable;
    if (this.spec.ccSwitch && !this.spec.provider) {
      throw new Error(`${this.carrier} requires a CC Switch provider selection`);
    }
    if (!this.spec.ccSwitch && this.spec.provider && this.spec.provider !== (this.carrier === "codex" ? "openai" : "anthropic")) {
      throw new Error(`${this.carrier} carrier does not support provider ${this.spec.provider}`);
    }
    const validThinking = this.carrier === "codex" ? CODEX_THINKING : CLAUDE_THINKING;
    if (this.spec.thinking && !validThinking.has(this.spec.thinking)) {
      throw new Error(`${this.carrier} carrier does not support thinking level ${this.spec.thinking}`);
    }
    if (this.spec.ccSwitch && this.carrier === "codex" && this.spec.thinking) {
      throw new Error("CC Switch Codex shared sessions configure reasoning effort in the provider profile; per-agent thinking is unavailable");
    }
    this.#prepareHome();
    this.sessionId = readJson(this.sessionPath)?.sessionId || null;
    this.started = true;
    this.stopping = false;
    return this.#state();
  }

  #args() {
    if (this.carrier === "codex") {
      const args = ["--sandbox", "workspace-write", "--ask-for-approval", "never", "--add-dir", this.spec.cwd, "exec"];
      if (this.sessionId) args.push("resume");
      args.push("--json");
      if (!this.spec.ccSwitch) args.push("--ignore-user-config");
      args.push("--skip-git-repo-check");
      if (this.spec.model) args.push("--model", this.spec.model);
      if (this.spec.thinking) args.push("-c", `model_reasoning_effort=\"${this.spec.thinking}\"`);
      if (this.sessionId) args.push(this.sessionId);
      args.push("-");
      return this.spec.ccSwitch
        ? ["start", "codex", this.spec.provider, "--shared-sessions", "--", ...args]
        : args;
    }
    const args = ["--bare", "--permission-mode", "dontAsk", "--allowedTools", CLAUDE_ALLOWED_TOOLS];
    if (this.spec.skills?.length) args.push("--add-dir", this.skillMountRoot);
    if (this.spec.model) args.push("--model", this.spec.model);
    if (this.spec.thinking) args.push("--effort", this.spec.thinking);
    if (this.sessionId) args.push("--resume", this.sessionId);
    else args.push("--session-id", this.spec.sessionId);
    args.push("--output-format", "stream-json", "--verbose", "-p");
    return this.spec.ccSwitch
      ? ["start", "claude", this.spec.provider, "--", ...args]
      : args;
  }

  #env() {
    const env = this.spec.ccSwitch ? {} : { ...this.baseEnv };
    if (this.spec.ccSwitch) {
      for (const key of ["LANG", "LC_ALL", "LC_CTYPE", "TERM", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"]) {
        if (typeof this.baseEnv[key] === "string") env[key] = this.baseEnv[key];
      }
      env.PATH = [...new Set([
        this.hubBin,
        dirname(this.spec.ccSwitch.codexCliPath),
        dirname(this.spec.ccSwitch.claudeCliPath),
        dirname(process.execPath),
        "/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin",
      ].filter(Boolean))].join(delimiter);
      env.CC_SWITCH_CONFIG_DIR = this.spec.ccSwitch.configDir;
      env.CC_SWITCH_TEST_HOME = this.spec.ccSwitch.home;
      env.PI_CODING_AGENT_DIR = this.spec.ccSwitch.piDir;
      env.XDG_CONFIG_HOME = join(this.privateHome, ".config");
      env.XDG_DATA_HOME = join(this.privateHome, ".local", "share");
      env.XDG_CACHE_HOME = join(this.privateHome, ".cache");
      env.XDG_STATE_HOME = join(this.privateHome, ".local", "state");
    }
    env.HOME = this.privateHome;
    if (this.spec.hubHome) {
      env.PIHUB_HOME = this.spec.hubHome;
      if (!this.spec.ccSwitch) env.PATH = [this.hubBin, dirname(process.execPath), env.PATH || "/usr/bin:/bin"].join(delimiter);
    }
    env.TMPDIR = this.spec.tmpDir || join(this.spec.stateDir, "tmp");
    env.PIHUB_AGENT_ID = this.spec.id;
    env.CODEX_HOME = join(this.privateHome, ".codex");
    env.CLAUDE_CONFIG_DIR = join(this.privateHome, ".claude");
    return env;
  }

  #log(direction, value) {
    appendFileSync(this.protocolLogPath, `${JSON.stringify({ at: nowIso(), direction, value })}\n`, { mode: 0o600 });
  }

  #handleOutput(value, turn) {
    this.#log("out", value);
    this.emit("event", { type: "carrier_event", carrier: this.carrier, value });
    if (this.carrier === "codex") {
      if (value.type === "thread.started" && value.thread_id) this.#setSession(value.thread_id);
      if (value.type === "item.completed" && value.item?.type === "agent_message") {
        const message = value.item.text || value.item.content?.map((part) => part.text || "").join("");
        if (message) this.emit("event", assistantMessage(message, this.carrier));
      }
      if (value.type === "turn.completed") turn.sawFinal = true;
      if (value.type === "turn.failed") turn.error = errorMessage(value.error);
    } else {
      if (value.type === "system" && value.subtype === "init" && value.session_id) this.#setSession(value.session_id);
      if (value.type === "result") {
        turn.sawFinal = true;
        if (value.session_id) this.#setSession(value.session_id);
        if (typeof value.result === "string" && value.result) this.emit("event", assistantMessage(value.result, this.carrier));
        if (value.is_error) turn.error = errorMessage(value.result || value.error);
      }
    }
  }

  #setSession(sessionId) {
    if (this.sessionId && this.sessionId !== sessionId) {
      throw new Error(`${this.carrier} session id changed during a turn`);
    }
    this.sessionId = sessionId;
    atomicWriteJson(this.sessionPath, { carrier: this.carrier, sessionId });
  }

  async #prompt(message) {
    if (typeof message !== "string" || !message) throw new Error("message must be non-empty");
    if (this.child) throw new Error(`${this.carrier} carrier is already processing a turn`);
    const responseId = newId();
    const args = this.#args();
    this.#log("in", { id: responseId, type: "prompt", message });
    const child = spawn(this.spec.ccSwitch?.executable || this.executable, args, {
      cwd: this.carrier === "codex" ? this.privateHome : this.spec.cwd,
      env: this.#env(),
      stdio: ["pipe", "pipe", "pipe"],
      detached: this.turnUsesProcessGroup,
    });
    this.child = child;
    const turn = { child, error: null, aborted: false, launchError: null, sawFinal: false };
    this.turn = turn;
    child.stdin.on("error", (error) => { turn.error ||= error.message; });
    const stderrStream = createWriteStream(this.stderrLogPath, { flags: "a", mode: 0o600 });
    child.stderr.pipe(stderrStream);
    const parser = new StrictJsonlParser({
      onValue: (value) => this.#handleOutput(value, turn),
      onError: (error, line) => {
        turn.error ||= `invalid ${this.carrier} JSONL: ${error.message}`;
        this.#log("error", { message: error.message, line });
        this.emit("protocol_error", { error, line });
      },
    });
    child.stdout.on("data", (chunk) => parser.push(chunk));
    child.stdout.on("end", () => parser.finish());
    child.on("error", (error) => { turn.launchError = error; });
    child.on("close", (code, signal) => {
      stderrStream.end();
      if (this.child === child) this.child = null;
      if (this.turn === turn) this.turn = null;
      if (this.stopping) {
        this.#emitExit(code, signal);
      } else if (turn.aborted) {
        this.emit("event", { type: "agent_settled", carrier: this.carrier, aborted: true, at: nowIso() });
      } else if (code === 0 && turn.sawFinal && !turn.error && !turn.launchError) {
        this.emit("event", { type: "agent_settled", carrier: this.carrier, at: nowIso() });
      } else {
        const error = turn.launchError || new Error(turn.error || `${this.carrier} turn failed or returned no final result (code=${code}, signal=${signal})`);
        this.emit("process_error", error);
        this.#emitExit(code, signal);
      }
    });
    return new Promise((resolvePromise, reject) => {
      child.once("error", reject);
      child.once("spawn", () => {
        child.removeListener("error", reject);
        child.stdin.end(message);
        this.emit("event", { type: "agent_start", carrier: this.carrier, at: nowIso() });
        resolvePromise({ id: responseId, type: "response", command: "prompt", success: true, data: { accepted: true } });
      });
    });
  }

  request(type, payload = {}) {
    if (!this.started) return Promise.reject(new Error(`${this.carrier} runtime is not running`));
    if (type === "get_state") return Promise.resolve({ id: newId(), type: "response", command: type, success: true, data: this.#state() });
    if (type === "prompt") return this.#prompt(payload.message);
    if (type === "clear_queue") return Promise.resolve({ id: newId(), type: "response", command: type, success: true, data: { cleared: 0 } });
    if (type === "abort") {
      if (this.turn) {
        this.turn.aborted = true;
        signalTurn(this.child, "SIGTERM", this.turnUsesProcessGroup);
      }
      return Promise.resolve({ id: newId(), type: "response", command: type, success: true, data: { aborted: Boolean(this.turn) } });
    }
    return Promise.reject(new Error(`${this.carrier} CLI carrier does not support ${type}; use prompt after the current turn settles`));
  }

  #emitExit(code = 0, signal = null) {
    if (!this.started) return;
    this.started = false;
    this.stopping = false;
    this.emit("exit", { code, signal, at: nowIso() });
  }

  async stop() {
    if (!this.started) return;
    this.stopping = true;
    if (!this.child) return this.#emitExit();
    signalTurn(this.child, "SIGTERM", this.turnUsesProcessGroup);
    for (let index = 0; index < 50 && this.child; index += 1) await sleep(100);
    if (this.child) signalTurn(this.child, "SIGKILL", this.turnUsesProcessGroup);
  }
}
