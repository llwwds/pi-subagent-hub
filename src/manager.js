import { EventEmitter } from "node:events";
import { existsSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { PiRpcProcess } from "./rpc-process.js";
import { ensurePiConfigFiles } from "./config-files.js";
import {
  assertAgentId,
  atomicWriteJson,
  ensurePrivateDir,
  newId,
  nowIso,
  resolveExistingDirectory,
} from "./utils.js";

const VALID_THINKING = new Set(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);

function linkSharedFile(source, target) {
  if (!existsSync(source) || existsSync(target)) return;
  symlinkSync(source, target);
}

export class AgentManager extends EventEmitter {
  constructor({ paths, store, profile, runtimeFactory = (options) => new PiRpcProcess(options) }) {
    super();
    this.paths = paths;
    this.store = store;
    this.profile = profile;
    this.runtimeFactory = runtimeFactory;
    this.runtimes = new Map();
  }

  recoverAfterDaemonRestart() {
    this.store.markInterruptedAgents();
  }

  #agentPaths(id) {
    const root = join(this.paths.agentsDir, id);
    return {
      root,
      agentDir: join(root, "pi-agent"),
      sessionDir: join(root, "sessions"),
      logsDir: join(root, "logs"),
      stateDir: join(root, "state"),
      tmpDir: join(root, "tmp"),
    };
  }

  #provision(id) {
    const paths = this.#agentPaths(id);
    for (const value of Object.values(paths)) ensurePrivateDir(value);
    ensurePiConfigFiles(this.paths);
    const settingsPath = join(paths.agentDir, "settings.json");
    if (!existsSync(settingsPath)) {
      atomicWriteJson(settingsPath, { enableInstallTelemetry: false, defaultProjectTrust: "never" });
    }
    linkSharedFile(this.paths.authPath, join(paths.agentDir, "auth.json"));
    linkSharedFile(this.paths.modelsPath, join(paths.agentDir, "models.json"));
    return paths;
  }

  #record(agentId, eventType, payload) {
    const event = {
      eventId: newId(),
      agentId,
      eventType,
      payload,
      createdAt: nowIso(),
    };
    const cursor = this.store.appendEvent(event);
    this.emit("event", { cursor, ...event });
    return cursor;
  }

  #wireRuntime(agentId, runtime) {
    runtime.on("event", (event) => {
      if (event?.type === "agent_start") this.store.updateAgent(agentId, { status: "running" });
      if (event?.type === "agent_settled") this.store.updateAgent(agentId, { status: "idle" });
      if (event?.type === "extension_ui_request") this.store.updateAgent(agentId, { status: "waiting_input" });
      this.#record(agentId, event?.type || "pi_event", event);
    });
    runtime.on("protocol_error", ({ error, line }) => {
      this.store.updateAgent(agentId, { last_error: `protocol error: ${error.message}` });
      this.#record(agentId, "protocol_error", { message: error.message, line });
    });
    runtime.on("process_error", (error) => {
      this.store.updateAgent(agentId, { status: "crashed", last_error: error.message, pid: null });
      this.#record(agentId, "process_error", { message: error.message });
    });
    runtime.on("exit", ({ code, signal, at }) => {
      const current = this.store.getAgent(agentId);
      const expected = current?.desired_state === "stopped" || current?.status === "stopping";
      this.store.updateAgent(agentId, {
        status: expected ? "stopped" : "crashed",
        pid: null,
        last_error: expected ? null : `Pi process exited (code=${code}, signal=${signal})`,
      });
      this.runtimes.delete(agentId);
      this.#record(agentId, "process_exit", { code, signal, at, expected });
    });
  }

  async createAgent(spec = {}) {
    const id = assertAgentId(spec.agentId || newId());
    if (this.store.getAgent(id)) throw new Error(`agent already exists: ${id}`);
    const cwd = resolveExistingDirectory(spec.cwd || process.cwd());
    const thinking = spec.thinking || null;
    if (thinking && !VALID_THINKING.has(thinking)) throw new Error(`unsupported thinking level: ${thinking}`);
    const name = String(spec.name || `agent-${id.slice(0, 8)}`);
    const workspaceMode = spec.workspaceMode || "shared";
    if (workspaceMode !== "shared") {
      throw new Error("workspaceMode must be shared in 1.0; isolated workspaces are not implemented yet");
    }
    const privatePaths = this.#provision(id);
    const sessionId = newId();
    const now = nowIso();
    this.store.createAgent({
      id,
      name,
      cwd,
      workspace_mode: workspaceMode,
      provider: spec.provider || null,
      model: spec.model || null,
      thinking,
      status: "provisioning",
      desired_state: "running",
      pid: null,
      session_id: sessionId,
      session_file: null,
      session_dir: privatePaths.sessionDir,
      agent_dir: privatePaths.agentDir,
      logs_dir: privatePaths.logsDir,
      profile_digest: this.profile.digest,
      tools_json: JSON.stringify(this.profile.tools),
      skills_json: JSON.stringify(this.profile.skills),
      extensions_json: JSON.stringify(this.profile.extensions),
      last_error: null,
      created_at: now,
      updated_at: now,
    });
    this.#record(id, "agent_created", {
      name,
      cwd,
      workspaceMode,
      profileDigest: this.profile.digest,
      sharedWorkspaceWarning: workspaceMode === "shared",
    });
    try {
      return await this.#startStoredAgent(id, { initialPrompt: spec.prompt || null });
    } catch (error) {
      this.store.updateAgent(id, { status: "failed", desired_state: "stopped", pid: null, last_error: error.message });
      this.#record(id, "agent_start_failed", { message: error.message });
      throw error;
    }
  }

  async #startStoredAgent(id, { initialPrompt = null } = {}) {
    const agent = this.store.getAgent(id);
    if (!agent) throw new Error(`unknown agent: ${id}`);
    if (this.runtimes.has(id)) throw new Error(`agent is already running: ${id}`);
    const privatePaths = this.#provision(id);
    const runtime = this.runtimeFactory({
      piCliPath: this.paths.piCliPath,
      spec: {
        id,
        name: agent.name,
        cwd: agent.cwd,
        provider: agent.provider,
        model: agent.model,
        thinking: agent.thinking,
        sessionId: agent.session_id,
        sessionDir: agent.session_dir,
        agentDir: agent.agent_dir,
        logsDir: agent.logs_dir,
        stateDir: privatePaths.stateDir,
        tmpDir: privatePaths.tmpDir,
        tools: agent.tools,
        skills: agent.skills,
        extensions: agent.extensions,
      },
    });
    this.runtimes.set(id, runtime);
    this.#wireRuntime(id, runtime);
    this.store.updateAgent(id, { status: "starting", desired_state: "running", last_error: null });
    const state = await runtime.start();
    const hasResolvedModel = state?.model
      && state.model.provider
      && state.model.id
      && state.model.provider !== "unknown"
      && state.model.id !== "unknown";
    const started = this.store.updateAgent(id, {
      status: state?.isStreaming ? "running" : "idle",
      desired_state: "running",
      pid: runtime.pid,
      session_file: state?.sessionFile || null,
      provider: hasResolvedModel ? state.model.provider : agent.provider,
      model: hasResolvedModel ? state.model.id : agent.model,
      thinking: hasResolvedModel ? (state.thinkingLevel || agent.thinking) : agent.thinking,
    });
    this.#record(id, "agent_ready", { pid: runtime.pid, sessionId: state?.sessionId || agent.session_id });
    if (initialPrompt) await this.send(id, "prompt", initialPrompt);
    return started;
  }

  getAgent(id) {
    return this.store.getAgent(id);
  }

  listAgents(options) {
    return this.store.listAgents(options);
  }

  listEvents(options) {
    return this.store.listEvents(options);
  }

  async send(id, kind, message) {
    const runtime = this.runtimes.get(id);
    if (!runtime?.alive) throw new Error(`agent is not running: ${id}`);
    const mapping = { prompt: "prompt", steer: "steer", follow_up: "follow_up" };
    const rpcType = mapping[kind];
    if (!rpcType) throw new Error(`unsupported message kind: ${kind}`);
    if (typeof message !== "string" || message.length === 0) throw new Error("message must be non-empty");
    const response = await runtime.request(rpcType, { message });
    this.#record(id, "command_accepted", { kind, responseId: response.id });
    return response;
  }

  async request(id, type, payload = {}) {
    const runtime = this.runtimes.get(id);
    if (!runtime?.alive) throw new Error(`agent is not running: ${id}`);
    const response = await runtime.request(type, payload);
    this.#record(id, "command_accepted", { kind: type, responseId: response.id });
    return response;
  }

  async stopAgent(id) {
    const agent = this.store.getAgent(id);
    if (!agent) throw new Error(`unknown agent: ${id}`);
    this.store.updateAgent(id, { status: "stopping", desired_state: "stopped" });
    this.#record(id, "stop_requested", {});
    const runtime = this.runtimes.get(id);
    if (runtime) await runtime.stop();
    if (!runtime || !runtime.alive) this.store.updateAgent(id, { status: "stopped", desired_state: "stopped", pid: null });
    return this.store.getAgent(id);
  }

  async restartAgent(id) {
    const agent = this.store.getAgent(id);
    if (!agent) throw new Error(`unknown agent: ${id}`);
    if (this.runtimes.has(id)) await this.stopAgent(id);
    this.#record(id, "restart_requested", {});
    return this.#startStoredAgent(id);
  }

  async stopAll() {
    const ids = [...this.runtimes.keys()];
    await Promise.allSettled(ids.map((id) => this.stopAgent(id)));
  }
}
