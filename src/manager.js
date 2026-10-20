import { EventEmitter } from "node:events";
import { existsSync, lstatSync, readlinkSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { PiRpcProcess } from "./rpc-process.js";
import { ensurePiConfigFiles } from "./config-files.js";
import { createHubLogWriter, isEphemeralStreamEvent, resolveHubLogDir } from "./logging.js";
import { DEFAULT_PROMPT_PRINCIPLE_IDS, listPromptPrinciples, promptSummary, readFrozenPromptConfig, resolvePromptConfig } from "./prompts.js";
import {
  assertAgentId,
  atomicWriteJson,
  ensurePrivateDir,
  newId,
  nowIso,
  readJson,
  resolveExistingDirectory,
} from "./utils.js";

const VALID_THINKING = new Set(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);

function promptInput(spec) {
  if (spec.promptConfig !== undefined && spec.promptConfig !== null) return spec.promptConfig;
  if (spec.systemPrompt !== undefined || spec.principles !== undefined) {
    return { customText: spec.systemPrompt, principleIds: spec.principles };
  }
  return undefined;
}

function storeHasColumn(store, column) {
  try {
    const rows = store.db?.prepare?.("PRAGMA table_info(agents)")?.all?.() || [];
    return rows.some((row) => row.name === column);
  } catch {
    return false;
  }
}

function linkSharedFile(source, target) {
  if (!existsSync(source) || existsSync(target)) return;
  symlinkSync(source, target);
}

function lstatSyncSafe(path) {
  try { return lstatSync(path); }
  catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

export class AgentManager extends EventEmitter {
  constructor({
    paths,
    store,
    profile,
    runtimeFactory = (options) => new PiRpcProcess(options),
    carrierRegistry = null,
    skillRegistry = null,
    ccSwitchService = null,
    skillManager = null,
    logger = null,
    logDebug = undefined,
  }) {
    super();
    this.paths = paths;
    this.store = store;
    this.profile = profile;
    this.runtimeFactory = runtimeFactory;
    this.carrierRegistry = carrierRegistry;
    this.skillRegistry = skillRegistry;
    this.ccSwitchService = ccSwitchService;
    this.skillManager = skillManager;
    this.skillManagerModulePromise = null;
    // One writer per Hub home; every Agent adapter is only a tagged view of it.
    // It is opened on first use and closed again when no Agent adapter remains,
    // so an idle Hub (and every test process) leaves no file handle open.
    this.injectedLogger = logger;
    this.logger = logger;
    this.logDebug = logDebug === undefined ? process.env.PIHUB_LOG_DEBUG === "1" : Boolean(logDebug);
    this.agentLogs = new Map();
    this.runtimes = new Map();
    // The store column is the queryable projection; the per-Agent private file
    // keeps the frozen prompt stable until the column lands in every store.
    this.promptConfigColumn = storeHasColumn(store, "prompt_config_json");
  }

  recoverAfterDaemonRestart() {
    this.store.markInterruptedAgents();
  }

  #agentPaths(id, carrier = "pi") {
    const root = join(this.paths.agentsDir, id);
    return {
      root,
      agentDir: join(root, carrier === "pi" ? "pi-agent" : `${carrier}-agent`),
      sessionDir: join(root, "sessions"),
      logsDir: join(root, "logs"),
      stateDir: join(root, "state"),
      tmpDir: join(root, "tmp"),
    };
  }

  #provision(id, carrier = "pi", platform = false) {
    const paths = this.#agentPaths(id, carrier);
    for (const value of Object.values(paths)) ensurePrivateDir(value);
    if (carrier === "pi") {
      const settingsPath = join(paths.agentDir, "settings.json");
      if (!existsSync(settingsPath)) {
        atomicWriteJson(settingsPath, { enableInstallTelemetry: false, defaultProjectTrust: "never" });
      }
      if (platform && this.ccSwitchService) {
        const modelsPath = this.paths.ccSwitchPiDir && join(this.paths.ccSwitchPiDir, "models.json");
        if (!modelsPath || !existsSync(modelsPath)) throw new Error("CC Switch Pi models.json is unavailable; add a Pi provider first");
        const target = join(paths.agentDir, "models.json");
        if (lstatSyncSafe(target)) {
          if (!lstatSync(target).isSymbolicLink() || readlinkSync(target) !== modelsPath) {
            throw new Error("Pi models link differs from the platform CC Switch configuration");
          }
        } else symlinkSync(modelsPath, target);
        const authPath = join(paths.agentDir, "auth.json");
        if (!existsSync(authPath)) atomicWriteJson(authPath, {});
      } else {
        ensurePiConfigFiles(this.paths);
        linkSharedFile(this.paths.authPath, join(paths.agentDir, "auth.json"));
        linkSharedFile(this.paths.modelsPath, join(paths.agentDir, "models.json"));
      }
    }
    return paths;
  }

  #freezePrompt(stateDir, frozen) {
    atomicWriteJson(join(stateDir, "prompt-config.json"), frozen);
  }

  #frozenPrompt(agent) {
    if (!agent) return null;
    const stored = readFrozenPromptConfig(agent.prompt_config ?? agent.prompt_config_json);
    if (stored) return stored;
    const { stateDir } = this.#agentPaths(agent.id, agent.carrier || "pi");
    return readFrozenPromptConfig(readJson(join(stateDir, "prompt-config.json")));
  }

  getPromptCatalog() {
    return { principles: listPromptPrinciples(), defaultPrincipleIds: [...DEFAULT_PROMPT_PRINCIPLE_IDS] };
  }

  promptConfigOf(agent, { includeText = false } = {}) {
    return promptSummary(this.#frozenPrompt(agent), { includeText });
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

  #agentLog(agentId) {
    return this.agentLogs.get(agentId) || null;
  }

  #wireRuntime(agentId, runtime) {
    const log = this.#agentLog(agentId);
    runtime.on("event", (event) => {
      if (event?.type === "agent_start") this.store.updateAgent(agentId, { status: "running", pid: runtime.pid });
      if (event?.type === "agent_settled") this.store.updateAgent(agentId, { status: "idle", pid: runtime.pid });
      if (event?.type === "extension_ui_request") this.store.updateAgent(agentId, { status: "waiting_input" });
      // Streaming deltas are progress, not history: neither SQLite nor the Hub log.
      if (isEphemeralStreamEvent(event)) return;
      this.#record(agentId, event?.type || "pi_event", event);
    });
    runtime.on("protocol_error", ({ error, line }) => {
      this.store.updateAgent(agentId, { last_error: `protocol error: ${error.message}` });
      log?.note("protocol_error", { message: error.message, line: typeof line === "string" ? line.slice(0, 512) : line }, "error");
      this.#record(agentId, "protocol_error", { message: error.message, line });
    });
    runtime.on("process_error", (error) => {
      this.store.updateAgent(agentId, { status: "crashed", last_error: error.message, pid: null });
      log?.note("process_error", { message: error.message }, "error");
      this.#record(agentId, "process_error", { message: error.message });
    });
    runtime.on("exit", ({ code, signal, at }) => {
      const current = this.store.getAgent(agentId);
      const expected = current?.desired_state === "stopped" || current?.status === "stopping";
      this.store.updateAgent(agentId, {
        status: expected ? "stopped" : "crashed",
        pid: null,
        last_error: expected ? null : `${current?.carrier === "pi" ? "Pi" : current?.carrier || "Agent"} process exited (code=${code}, signal=${signal})`,
      });
      this.runtimes.delete(agentId);
      log?.note("process_exit", { code, signal, expected });
      log?.end();
      this.agentLogs.delete(agentId);
      this.#record(agentId, "process_exit", { code, signal, at, expected });
      this.#releaseLoggerIfIdle().catch(() => {});
    });
  }

  /** Flush and close the Hub log stream. Call once during daemon shutdown. */
  async closeLogs() {
    await Promise.allSettled([...this.agentLogs.values()].map((log) => log.end()));
    this.agentLogs.clear();
    const logger = this.logger;
    this.logger = null;
    if (logger) await logger.close();
  }

  logStats() {
    if (!this.logger) return null;
    return this.logger.stats();
  }

  #ensureLogger() {
    if (this.logger) return this.logger;
    this.logger = createHubLogWriter({ dir: resolveHubLogDir(this.paths), debug: this.logDebug });
    return this.logger;
  }

  // The rotating stream keeps the process alive while open, so close it once the
  // last Agent adapter is gone instead of holding it for the whole process.
  async #releaseLoggerIfIdle() {
    if (this.injectedLogger || this.agentLogs.size > 0 || !this.logger) return;
    const logger = this.logger;
    this.logger = null;
    try { await logger.close(); } catch {}
  }

  async createAgent(spec = {}) {
    return this.#createAgent(spec, { platform: false });
  }

  async createPlatformAgent(spec = {}) {
    return this.#createAgent(spec, { platform: true });
  }

  async #resolvePlatformProvider(carrier, spec) {
    if (!this.ccSwitchService) return { provider: spec.provider || null, model: spec.model || null };
    const providers = (await this.ccSwitchService.listProviders(carrier)).filter((item) => item.configured !== false);
    const selectedId = spec.provider || providers.find((item) => item.isCurrent)?.id
      || (providers.length === 1 ? providers[0].id : null);
    if (!selectedId) throw new Error(`no CC Switch provider selected for ${carrier}; add one or choose a provider`);
    const provider = providers.find((item) => item.id === selectedId);
    if (!provider) throw new Error(`CC Switch provider ${selectedId} is unavailable for ${carrier}`);
    return { provider: selectedId, model: spec.model || provider.defaultModel || null };
  }

  async #createAgent(spec, { platform }) {
    const id = assertAgentId(spec.agentId || newId());
    if (this.store.getAgent(id)) throw new Error(`agent already exists: ${id}`);
    const cwd = resolveExistingDirectory(spec.cwd || process.cwd());
    const carrier = platform ? String(spec.carrier || "pi") : "pi";
    let authorization = null;
    if (platform) {
      if (!this.carrierRegistry || !this.skillRegistry) throw new Error("platform registries are unavailable");
      const diagnostic = await this.carrierRegistry.diagnose(carrier, { paths: this.paths, env: process.env });
      if (!diagnostic.available) throw new Error(`carrier ${carrier} is unavailable: ${diagnostic.reason || "unknown reason"}`);
      authorization = await this.#resolveSkillAuthorization(spec.skillPolicy);
    }
    const route = platform ? await this.#resolvePlatformProvider(carrier, spec)
      : { provider: spec.provider || null, model: spec.model || null };
    const thinking = spec.thinking || null;
    if (thinking && !VALID_THINKING.has(thinking)) throw new Error(`unsupported thinking level: ${thinking}`);
    // Resolve before any provisioning so an unsupported or invalid prompt
    // leaves no half-created agent directory behind.
    const promptConfig = resolvePromptConfig(promptInput(spec), { carrier });
    const name = String(spec.name || `agent-${id.slice(0, 8)}`);
    const workspaceMode = spec.workspaceMode || "shared";
    if (workspaceMode !== "shared") {
      throw new Error("workspaceMode must be shared in 1.0; isolated workspaces are not implemented yet");
    }
    const privatePaths = this.#provision(id, carrier, platform);
    // Frozen once, then re-read verbatim on every restart: later global changes
    // cannot alter a running Agent's system prompt.
    this.#freezePrompt(privatePaths.stateDir, promptConfig);
    const sessionId = newId();
    const now = nowIso();
    const record = {
      id,
      name,
      cwd,
      workspace_mode: workspaceMode,
      provider: route.provider,
      model: route.model,
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
      skills_json: JSON.stringify(authorization ? authorization.grants.map((grant) => ({ id: grant.id, path: grant.path, sha256: grant.sha256 })) : this.profile.skills),
      extensions_json: JSON.stringify(platform ? [] : this.profile.extensions),
      carrier,
      skill_authorization_json: authorization ? JSON.stringify(authorization) : null,
      last_error: null,
      created_at: now,
      updated_at: now,
    };
    if (this.promptConfigColumn) record.prompt_config_json = JSON.stringify(promptConfig);
    this.store.createAgent(record);
    this.#record(id, "agent_created", {
      name,
      cwd,
      workspaceMode,
      profileDigest: this.profile.digest,
      sharedWorkspaceWarning: workspaceMode === "shared",
      prompt: promptSummary(promptConfig),
      ...(platform ? { carrier, skillDigest: authorization.digest } : {}),
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
    if (agent.skill_authorization) {
      this.skillRegistry?.verifyAuthorization(agent.skill_authorization);
    }
    const privatePaths = this.#provision(id, agent.carrier, Boolean(agent.skill_authorization));
    const runtimeSpec = {
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
      authorization: agent.skill_authorization,
      promptConfig: this.#frozenPrompt(agent),
    };
    const logger = this.#ensureLogger();
    const agentLog = logger.createAgentLogWriter({ agentId: id, carrier: agent.carrier || "pi", source: "hub-manager" });
    this.agentLogs.set(id, agentLog);
    agentLog.note("runtime_start", { carrier: agent.carrier || "pi", sessionId: agent.session_id });
    const runtime = agent.skill_authorization
      ? this.carrierRegistry.createRuntime(agent.carrier, { paths: this.paths, spec: runtimeSpec, env: process.env, logger })
      : this.runtimeFactory({ piCliPath: this.paths.piCliPath, spec: runtimeSpec, logger });
    this.runtimes.set(id, runtime);
    this.#wireRuntime(id, runtime);
    this.store.updateAgent(id, { status: "starting", desired_state: "running", last_error: null });
    let state;
    try {
      state = await runtime.start();
    } catch (error) {
      try { await runtime.stop(); } catch {}
      if (this.runtimes.get(id) === runtime) this.runtimes.delete(id);
      agentLog.note("runtime_start_failed", { message: error.message }, "error");
      await agentLog.end();
      this.agentLogs.delete(id);
      await this.#releaseLoggerIfIdle();
      throw error;
    }
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
    agentLog.note("agent_ready", { pid: runtime.pid });
    if (initialPrompt) await this.send(id, "prompt", initialPrompt);
    return initialPrompt ? this.store.getAgent(id) : started;
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

  activeCount() {
    return [...this.runtimes.values()].filter((runtime) => runtime.alive && runtime.pid !== null).length;
  }

  managedCount() {
    return [...this.runtimes.values()].filter((runtime) => runtime.alive).length;
  }

  async listCarriers() {
    if (!this.carrierRegistry) return [];
    const diagnostics = await Promise.all(this.carrierRegistry.list().map((carrier) =>
      this.carrierRegistry.diagnose(carrier.id, { paths: this.paths, env: process.env })));
    return diagnostics.map((carrier) => ({ ...carrier, label: carrier.displayName }));
  }

  async listProviders() {
    if (!this.ccSwitchService) return { switch: { available: false, reason: "CC Switch service is unavailable", version: null }, apps: { pi: [], codex: [], claude: [] } };
    const [diagnostic, apps] = await Promise.all([
      this.ccSwitchService.diagnose(),
      this.ccSwitchService.listAllProviders(),
    ]);
    return { switch: diagnostic, apps };
  }

  async addProvider(input) {
    if (!this.ccSwitchService) throw new Error("CC Switch service is unavailable");
    return this.ccSwitchService.addProvider(input);
  }

  async switchProvider(app, id) {
    if (!this.ccSwitchService) throw new Error("CC Switch service is unavailable");
    return this.ccSwitchService.switchProvider(app, id);
  }

  listSkills() {
    return (this.skillRegistry?.listSkills() || []).map((skill) => ({
      id: skill.id,
      name: skill.id,
      version: skill.currentSha256.slice(0, 12),
      digest: `sha256:${skill.currentSha256}`,
      currentSha256: skill.currentSha256,
      source: skill.source,
      importedAt: skill.importedAt,
      versions: skill.versions,
    }));
  }

  importSkill(spec) {
    if (!this.skillRegistry) throw new Error("Skill registry is unavailable");
    return this.skillRegistry.importSkill(spec);
  }

  async #resolveSkillManager() {
    if (this.skillManager) return this.skillManager;
    if (!this.skillManagerModulePromise) {
      this.skillManagerModulePromise = import("./skills/manager-connection.js")
        .then((module) => new module.SkillManagerConnection({ home: this.paths.home }))
        .catch((error) => {
          this.skillManagerModulePromise = null;
          throw new Error(`Skill Manager connection is unavailable: ${error.message}`);
        });
    }
    return this.skillManagerModulePromise;
  }

  async #optionalSkillManager() {
    try {
      return await this.#resolveSkillManager();
    } catch {
      return null;
    }
  }

  // Connected Skill Manager is opt-in. Without an explicit connection the
  // registry catalog keeps its existing default authorization semantics.
  async #resolveSkillAuthorization(policy) {
    const connection = await this.#optionalSkillManager();
    if (!connection || !(await connection.status()).connected) return this.skillRegistry.resolveAuthorization(policy);
    const sources = await connection.resolveSources(policy);
    for (const source of sources) {
      this.skillRegistry.importSkill({
        id: source.id,
        sourcePath: source.sourcePath,
        sourceLabel: `skills-manager:${source.name || source.id}`,
      });
    }
    // Freeze exactly this snapshot list so later manager changes cannot expand it.
    return this.skillRegistry.resolveAuthorization({ mode: "only", ids: sources.map((source) => source.id) });
  }

  async skillManagerStatus({ live = false } = {}) {
    const connection = await this.#resolveSkillManager();
    const status = await connection.status({ live });
    return { ...status, skills: status.connected ? connection.listSkills() : [] };
  }

  async connectSkillManager(input = {}) {
    const connection = await this.#resolveSkillManager();
    const result = await connection.connect(input);
    this.#record(null, "skill_manager_connected", {
      skillCount: result?.snapshot?.skillCount ?? null,
      source: result?.config?.source || null,
    });
    return { ...(await this.skillManagerStatus({ live: true })), result };
  }

  async syncSkillManager(input = {}) {
    const connection = await this.#resolveSkillManager();
    const result = await connection.sync(input);
    this.#record(null, "skill_manager_synced", { synced: Boolean(result?.synced), skillCount: result?.snapshot?.skillCount ?? null });
    return { ...(await this.skillManagerStatus({ live: true })), result };
  }

  async disconnectSkillManager(input = {}) {
    const connection = await this.#resolveSkillManager();
    const result = await connection.disconnect(input);
    this.#record(null, "skill_manager_disconnected", { snapshotKept: Boolean(result?.snapshotKept) });
    return { ...(await this.skillManagerStatus()), result };
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
