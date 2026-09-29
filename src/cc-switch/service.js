import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  chmodSync,
  closeSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { isAbsolute, join, relative, resolve, sep } from "node:path";

const APPS = Object.freeze(["claude", "codex", "pi"]);
const VERSION = "5.10.5";
const LOCK_TIMEOUT_MS = 10_000;
const COMMAND_TIMEOUT_MS = 30_000;

export class CcSwitchError extends Error {
  constructor(code, message, extra = {}) {
    super(message);
    this.name = "CcSwitchError";
    this.code = code;
    Object.assign(this, extra);
  }
}

function fail(code, message, extra) {
  throw new CcSwitchError(code, message, extra);
}

function inside(root, target) {
  const rel = relative(root, target);
  return rel !== "" && rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel);
}

function assertNoSymlinkComponents(path) {
  let current = resolve(path);
  const components = [];
  while (current !== resolve(current, "..")) {
    components.push(current);
    current = resolve(current, "..");
  }
  for (const component of components.reverse()) {
    try {
      if (lstatSync(component).isSymbolicLink()) fail("CC_SWITCH_PATH_UNSAFE", "CC Switch 私有路径包含符号链接");
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
  }
}

function assertNoSymlinksInTree(root) {
  if (!existsSync(root)) return;
  const pending = [root];
  let scanned = 0;
  while (pending.length) {
    const path = pending.pop();
    assertNoSymlinkComponents(path);
    if (++scanned > 20_000) fail("CC_SWITCH_PATH_UNSAFE", "CC Switch 私有目录项目过多，无法安全检查");
    if (!lstatSync(path).isDirectory()) continue;
    for (const entry of readdirSync(path, { withFileTypes: true })) {
      if (entry.isSymbolicLink()) fail("CC_SWITCH_PATH_UNSAFE", "CC Switch 私有目录包含符号链接");
      pending.push(join(path, entry.name));
    }
  }
}

function tightenPrivateTree(root) {
  assertNoSymlinksInTree(root);
  if (!existsSync(root)) return;
  const pending = [root];
  while (pending.length) {
    const path = pending.pop();
    const stat = lstatSync(path);
    chmodSync(path, stat.isDirectory() ? 0o700 : 0o600);
    if (stat.isDirectory()) {
      for (const entry of readdirSync(path)) pending.push(join(path, entry));
    }
  }
}

function validateApp(app) {
  if (!APPS.includes(app)) fail("CC_SWITCH_UNSUPPORTED", "CC Switch 仅支持 claude、codex、pi");
  return app;
}

function validateId(id) {
  if (typeof id !== "string" || !/^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(id)) {
    fail("CC_SWITCH_INVALID_INPUT", "Provider ID 格式无效");
  }
  return id;
}

function nonEmpty(value, label, maxLength = 256) {
  if (typeof value !== "string" || !value.trim() || value.length > maxLength || /[\u0000-\u001f]/.test(value)) {
    fail("CC_SWITCH_INVALID_INPUT", `${label} 格式无效`);
  }
  return value.trim();
}

function validateBaseUrl(raw) {
  const value = nonEmpty(raw, "API URL", 2048);
  let url;
  try { url = new URL(value); } catch { fail("CC_SWITCH_INVALID_INPUT", "API URL 无效"); }
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    fail("CC_SWITCH_INVALID_INPUT", "API URL 必须是不含凭据、查询参数的 HTTP(S) 地址");
  }
  return value.replace(/\/$/, "");
}

function publicBaseUrl(raw) {
  if (typeof raw !== "string") return null;
  try {
    const url = new URL(raw);
    if (!["http:", "https:"].includes(url.protocol)) return null;
    url.username = "";
    url.password = "";
    url.search = "";
    url.hash = "";
    return url.toString().replace(/\/$/, "");
  } catch {
    return null;
  }
}

function validateModels(raw, defaultModel) {
  if (!Array.isArray(raw) || raw.length === 0 || raw.length > 100) {
    fail("CC_SWITCH_INVALID_INPUT", "modelList 必须包含 1 到 100 个模型");
  }
  const models = raw.map((model) => nonEmpty(model, "模型 ID", 160));
  if (new Set(models).size !== models.length) fail("CC_SWITCH_INVALID_INPUT", "modelList 包含重复模型");
  const selected = defaultModel === undefined ? models[0] : nonEmpty(defaultModel, "默认模型", 160);
  if (!models.includes(selected)) fail("CC_SWITCH_INVALID_INPUT", "默认模型不在 modelList 中");
  return { modelList: models, defaultModel: selected };
}

function configFor(input) {
  const { app, name, baseUrl, apiKey, modelList, defaultModel, apiFormat, authMode } = input;
  if (app === "claude") {
    if (apiFormat && apiFormat !== "anthropic") fail("CC_SWITCH_UNSUPPORTED", "Claude 目前只支持原生 Anthropic API 格式");
    const mode = authMode ?? "api-key";
    if (!["api-key", "bearer"].includes(mode)) fail("CC_SWITCH_UNSUPPORTED", "Claude 认证方式必须为 api-key 或 bearer");
    return { env: {
      ANTHROPIC_BASE_URL: baseUrl,
      [mode === "bearer" ? "ANTHROPIC_AUTH_TOKEN" : "ANTHROPIC_API_KEY"]: apiKey,
      ANTHROPIC_MODEL: defaultModel,
    } };
  }
  if (app === "codex") {
    if (apiFormat && !["responses", "openai_responses"].includes(apiFormat)) {
      fail("CC_SWITCH_UNSUPPORTED", "Codex 目前只支持 OpenAI Responses API 格式");
    }
    const toml = [
      'model_provider = "custom"',
      `model = ${JSON.stringify(defaultModel)}`,
      "",
      "[model_providers.custom]",
      `name = ${JSON.stringify(name)}`,
      `base_url = ${JSON.stringify(baseUrl)}`,
      'wire_api = "responses"',
      "requires_openai_auth = true",
      "",
    ].join("\n");
    return { auth: { OPENAI_API_KEY: apiKey }, config: toml };
  }
  const format = apiFormat ?? "openai-completions";
  if (!["openai-completions", "openai-responses", "anthropic-messages"].includes(format)) {
    fail("CC_SWITCH_UNSUPPORTED", "Pi 当前不支持该 API 格式");
  }
  return { name, baseUrl, api: format, apiKey, models: modelList.map((id) => ({ id })) };
}

function parseJson(text) {
  try { return JSON.parse(text); } catch { fail("CC_SWITCH_STATE_INVALID", "CC Switch 私有状态格式无效"); }
}

function readIndex(path) {
  if (!existsSync(path)) return { version: 1, providers: { claude: {}, codex: {}, pi: {} } };
  const index = parseJson(readFileSync(path, "utf8"));
  if (index?.version !== 1 || typeof index.providers !== "object" || index.providers === null) {
    fail("CC_SWITCH_STATE_INVALID", "CC Switch 模型索引格式无效");
  }
  return index;
}

function atomicWrite(path, value) {
  const directory = resolve(path, "..");
  const temp = join(directory, `.tmp-${process.pid}-${randomUUID()}`);
  try {
    writeFileSync(temp, `${JSON.stringify(value)}\n`, { mode: 0o600, flag: "wx" });
    chmodSync(temp, 0o600);
    renameSync(temp, path);
  } finally {
    if (existsSync(temp)) rmSync(temp, { force: true });
  }
}

function sleep(ms) { return new Promise((done) => setTimeout(done, ms)); }

export class CcSwitchService {
  constructor(paths, options = {}) {
    if (!paths || !isAbsolute(paths.home)) fail("CC_SWITCH_PATH_UNSAFE", "PIHUB_HOME 必须是绝对路径");
    this.paths = paths;
    this.root = join(paths.home, "cc-switch");
    this.claudeDir = join(paths.ccSwitchHome, ".claude");
    this.codexDir = join(paths.ccSwitchHome, ".codex");
    this.tmpDir = join(this.root, "tmp");
    this.dbPath = join(paths.ccSwitchConfigDir, "cc-switch.db");
    this.indexPath = join(this.root, "models-index.json");
    this.lockPath = join(this.root, "operation.lock");
    this.settingsPath = join(paths.ccSwitchConfigDir, "settings.json");
    this.commandTimeoutMs = options.commandTimeoutMs ?? COMMAND_TIMEOUT_MS;
    const expectedBinary = join(paths.home, "toolchains", "cc-switch-cli", "cc-switch");
    if (resolve(paths.ccSwitchCliPath) !== expectedBinary) fail("CC_SWITCH_PATH_UNSAFE", "CC Switch 二进制必须位于应用私有工具链");
    for (const path of [paths.ccSwitchConfigDir, paths.ccSwitchHome, paths.ccSwitchPiDir]) {
      if (!inside(paths.home, path) || !inside(this.root, path)) fail("CC_SWITCH_PATH_UNSAFE", "CC Switch 配置目录越过应用私有目录");
    }
  }

  #checkPaths() {
    assertNoSymlinkComponents(this.paths.home);
    assertNoSymlinkComponents(this.paths.ccSwitchCliPath);
    for (const path of [this.root, this.paths.ccSwitchConfigDir, this.paths.ccSwitchHome, this.paths.ccSwitchPiDir]) {
      assertNoSymlinkComponents(path);
    }
    assertNoSymlinksInTree(this.root);
  }

  #prepare() {
    this.#checkPaths();
    for (const path of [this.root, this.paths.ccSwitchConfigDir, this.paths.ccSwitchHome, this.paths.ccSwitchPiDir, this.claudeDir, this.codexDir, this.tmpDir]) {
      mkdirSync(path, { recursive: true, mode: 0o700 });
    }
    this.#checkPaths();
    if (existsSync(this.settingsPath)) {
      const settings = parseJson(readFileSync(this.settingsPath, "utf8"));
      for (const key of ["claudeConfigDir", "codexConfigDir", "piConfigDir"]) {
        if (settings[key] !== undefined && settings[key] !== null && settings[key] !== "") {
          fail("CC_SWITCH_PATH_UNSAFE", `CC Switch ${key} 覆盖会破坏每个 agent 的私有目录`);
        }
      }
    }
    this.#checkPaths();
    tightenPrivateTree(this.root);
  }

  #env() {
    return {
      HOME: this.paths.ccSwitchHome,
      PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
      LANG: "C.UTF-8",
      TMPDIR: this.tmpDir,
      XDG_CONFIG_HOME: join(this.paths.ccSwitchHome, ".config"),
      XDG_DATA_HOME: join(this.paths.ccSwitchHome, ".local", "share"),
      CC_SWITCH_CONFIG_DIR: this.paths.ccSwitchConfigDir,
      CC_SWITCH_TEST_HOME: this.paths.ccSwitchHome,
      CLAUDE_CONFIG_DIR: this.claudeDir,
      CODEX_HOME: this.codexDir,
      PI_CODING_AGENT_DIR: this.paths.ccSwitchPiDir,
    };
  }

  async #run(args, { readOutput = false } = {}) {
    this.#prepare();
    if (!existsSync(this.paths.ccSwitchCliPath) || !statSync(this.paths.ccSwitchCliPath).isFile()) {
      fail("CC_SWITCH_MISSING", "应用私有 CC Switch CLI 尚未安装");
    }
    return new Promise((done, reject) => {
      let output = "";
      let settled = false;
      const child = spawn(this.paths.ccSwitchCliPath, args, {
        cwd: this.paths.ccSwitchHome,
        env: this.#env(),
        stdio: ["ignore", "pipe", "pipe"],
      });
      const timer = setTimeout(() => child.kill("SIGKILL"), this.commandTimeoutMs);
      const finish = (error, value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        try { tightenPrivateTree(this.root); }
        catch { error = new CcSwitchError("CC_SWITCH_PATH_UNSAFE", "CC Switch 私有文件权限或路径检查失败"); }
        if (error) reject(error);
        else done(value);
      };
      child.stdout.on("data", (bytes) => { if (readOutput && output.length < 1024) output += bytes.toString("utf8"); });
      child.stderr.on("data", () => {});
      child.on("error", () => finish(new CcSwitchError("CC_SWITCH_EXEC_FAILED", "无法执行应用私有 CC Switch CLI")));
      child.on("close", (code, signal) => {
        if (signal === "SIGKILL") finish(new CcSwitchError("CC_SWITCH_TIMEOUT", "CC Switch CLI 执行超时"));
        else if (code !== 0) finish(new CcSwitchError("CC_SWITCH_COMMAND_FAILED", "CC Switch CLI 操作失败；请检查私有配置"));
        else finish(null, output.trim());
      });
    });
  }

  async #version() {
    const output = await this.#run(["--version"], { readOutput: true });
    const match = /^cc-switch\s+(\d+\.\d+\.\d+)$/m.exec(output);
    if (!match || match[1] !== VERSION) fail("CC_SWITCH_VERSION_UNSUPPORTED", `需要应用私有 CC Switch CLI ${VERSION}`);
    return match[1];
  }

  async diagnose() {
    try {
      const version = await this.#version();
      return { available: true, version, binaryPath: this.paths.ccSwitchCliPath, configDir: this.paths.ccSwitchConfigDir };
    } catch (error) {
      if (!(error instanceof CcSwitchError)) throw error;
      return { available: false, version: null, binaryPath: this.paths.ccSwitchCliPath, configDir: this.paths.ccSwitchConfigDir, errorCode: error.code, reason: error.message };
    }
  }

  async #withLock(callback) {
    this.#prepare();
    const deadline = Date.now() + LOCK_TIMEOUT_MS;
    let fd;
    while (fd === undefined) {
      try {
        fd = openSync(this.lockPath, "wx", 0o600);
        writeFileSync(fd, `${process.pid}\n`);
      } catch (error) {
        if (error?.code !== "EEXIST") throw error;
        if (Date.now() >= deadline) fail("CC_SWITCH_BUSY", "CC Switch 正在执行另一项配置操作");
        await sleep(75);
      }
    }
    try { return await callback(); }
    finally {
      closeSync(fd);
      unlinkSync(this.lockPath);
    }
  }

  #readRows(app) {
    this.#checkPaths();
    if (!existsSync(this.dbPath)) return [];
    let db;
    try {
      db = new DatabaseSync(this.dbPath, { readOnly: true });
      return db.prepare("SELECT id, name, settings_config, is_current FROM providers WHERE app_type = ? ORDER BY sort_index, created_at, id").all(app);
    } catch {
      fail("CC_SWITCH_STATE_INVALID", "无法读取 CC Switch 私有 Provider 状态");
    } finally {
      db?.close();
    }
  }

  #piDefault() {
    const path = join(this.paths.ccSwitchPiDir, "settings.json");
    if (!existsSync(path)) return {};
    this.#checkPaths();
    const settings = parseJson(readFileSync(path, "utf8"));
    return {
      provider: typeof settings.defaultProvider === "string" ? settings.defaultProvider : null,
      model: typeof settings.defaultModel === "string" ? settings.defaultModel : null,
    };
  }

  async listProviders(app) {
    validateApp(app);
    await this.#version();
    const index = readIndex(this.indexPath);
    const piDefault = app === "pi" ? this.#piDefault() : {};
    return this.#readRows(app).map((row) => {
      const config = parseJson(row.settings_config);
      const saved = index.providers?.[app]?.[row.id] ?? {};
      let baseUrl = null;
      let models = [];
      let defaultModel = null;
      let hasCredential = false;
      if (app === "claude") {
        baseUrl = config.env?.ANTHROPIC_BASE_URL ?? null;
        defaultModel = config.env?.ANTHROPIC_MODEL ?? null;
        hasCredential = Boolean(config.env?.ANTHROPIC_AUTH_TOKEN || config.env?.ANTHROPIC_API_KEY);
      } else if (app === "codex") {
        const toml = typeof config.config === "string" ? config.config : "";
        const urlMatch = /^base_url\s*=\s*("(?:\\.|[^"])*")/m.exec(toml);
        const modelMatch = /^model\s*=\s*("(?:\\.|[^"])*")/m.exec(toml);
        if (urlMatch) try { baseUrl = JSON.parse(urlMatch[1]); } catch { /* unknown third-party TOML */ }
        if (modelMatch) try { defaultModel = JSON.parse(modelMatch[1]); } catch { /* unknown third-party TOML */ }
        hasCredential = Boolean(config.auth?.OPENAI_API_KEY || config.env?.OPENAI_API_KEY);
      } else {
        baseUrl = config.baseUrl ?? null;
        models = Array.isArray(config.models) ? config.models.map((item) => item?.id).filter((id) => typeof id === "string") : [];
        defaultModel = piDefault.provider === row.id ? piDefault.model : models[0] ?? null;
        hasCredential = Boolean(config.apiKey);
      }
      if (Array.isArray(saved.modelList)) models = saved.modelList.filter((model) => typeof model === "string");
      if (typeof saved.defaultModel === "string") defaultModel = saved.defaultModel;
      if (defaultModel && !models.includes(defaultModel)) models.unshift(defaultModel);
      const safeBaseUrl = publicBaseUrl(baseUrl);
      const configured = Boolean(safeBaseUrl && hasCredential && defaultModel && models.length);
      const apiFormat = typeof saved.apiFormat === "string" ? saved.apiFormat : app === "pi" && typeof config.api === "string" ? config.api : null;
      const authMode = app === "claude"
        ? (saved.authMode === "api-key" || saved.authMode === "bearer" ? saved.authMode
          : config.env?.ANTHROPIC_AUTH_TOKEN ? "bearer" : "api-key")
        : null;
      return {
        id: row.id,
        name: row.name,
        baseUrl: safeBaseUrl,
        modelList: models,
        defaultModel: typeof defaultModel === "string" ? defaultModel : null,
        apiFormat,
        authMode,
        configured,
        ready: configured,
        isCurrent: app === "pi" ? null : Boolean(row.is_current),
        isDefault: app === "pi" ? piDefault.provider === row.id : Boolean(row.is_current),
      };
    });
  }

  async listAllProviders() {
    const providers = {};
    for (const app of APPS) providers[app] = await this.listProviders(app);
    return providers;
  }

  async getProvider(app, id) {
    validateApp(app);
    validateId(id);
    return (await this.listProviders(app)).find((provider) => provider.id === id) ?? null;
  }

  async assertProviderExists(app, id) {
    const provider = await this.getProvider(app, id);
    if (!provider) fail("CC_SWITCH_PROVIDER_NOT_FOUND", "指定 Provider 不存在");
    return provider;
  }

  async addProvider(raw) {
    const app = validateApp(raw?.app);
    const id = validateId(raw?.id);
    const name = nonEmpty(raw?.name, "Provider 名称", 120);
    const baseUrl = validateBaseUrl(raw?.baseUrl);
    const apiKey = nonEmpty(raw?.apiKey, "API Key", 8192);
    const { modelList, defaultModel } = validateModels(raw?.modelList, raw?.defaultModel);
    const config = configFor({ app, name, baseUrl, apiKey, modelList, defaultModel, apiFormat: raw?.apiFormat, authMode: raw?.authMode });
    return this.#withLock(async () => {
      await this.#version();
      if (this.#readRows(app).some((row) => row.id === id)) fail("CC_SWITCH_PROVIDER_EXISTS", "Provider ID 已存在");
      const directory = mkdtempSync(join(this.tmpDir, "add-"));
      chmodSync(directory, 0o700);
      const configPath = join(directory, "settings.json");
      try {
        writeFileSync(configPath, `${JSON.stringify(config)}\n`, { mode: 0o600, flag: "wx" });
        chmodSync(configPath, 0o600);
        const args = ["--app", app, "provider", "add", "--id", id, "--name", name, "--config-file", configPath];
        if (raw?.apiFormat && app !== "pi") args.push("--api-format", raw.apiFormat);
        await this.#run(args);
      } finally {
        rmSync(directory, { recursive: true, force: true });
      }
      const row = this.#readRows(app).find((provider) => provider.id === id);
      if (!row) fail("CC_SWITCH_STATE_INVALID", "CC Switch 未保存 Provider");
      const index = readIndex(this.indexPath);
      index.providers[app] ??= {};
      index.providers[app][id] = {
        modelList, defaultModel,
        apiFormat: raw?.apiFormat ?? (app === "claude" ? "anthropic" : app === "codex" ? "responses" : "openai-completions"),
        ...(app === "claude" ? { authMode: raw?.authMode ?? "api-key" } : {}),
      };
      try { atomicWrite(this.indexPath, index); }
      catch { fail("CC_SWITCH_PARTIAL_ADD", "Provider 已加入，但模型索引写入失败", { providerAdded: true }); }
      return this.assertProviderExists(app, id);
    });
  }

  async switchProvider(app, id) {
    validateApp(app);
    validateId(id);
    if (app === "pi") fail("CC_SWITCH_UNSUPPORTED", "Pi 的当前/默认模型由 Pi settings.json 管理，CC Switch 的 switch 只启用模型条目");
    return this.#withLock(async () => {
      await this.#version();
      const selected = await this.assertProviderExists(app, id);
      if (!selected.configured || !selected.ready) {
        fail("CC_SWITCH_PROVIDER_NOT_READY", "该 Provider 缺少 API 地址、凭据或模型配置，无法切换");
      }
      await this.#run(["--app", app, "provider", "switch", id]);
      const provider = await this.assertProviderExists(app, id);
      if (!provider.isCurrent) fail("CC_SWITCH_STATE_INVALID", "CC Switch 切换后状态未更新");
      return provider;
    });
  }
}
