const token = document.querySelector('meta[name="pihub-token"]').content;
const $ = (selector) => document.querySelector(selector);
const state = {
  agents: [], carriers: [], skills: [], selectedId: null, events: [],
  recentEvents: new Map(), eventCursor: null, detailCursor: null,
  activeCount: 0, managedCount: 0, view: "agents", refreshBusy: false, metadataBusy: false,
  providers: { switch: { available: false, reason: "正在读取 CC Switch 状态…" }, apps: { pi: [], codex: [], claude: [] } },
  providersError: null, providersBusy: false,
};

const STATUS = {
  provisioning: { label: "准备中", tone: "pending" },
  starting: { label: "启动中", tone: "pending" },
  running: { label: "执行中", tone: "running" },
  idle: { label: "待命", tone: "idle" },
  waiting_input: { label: "等待输入", tone: "waiting_input" },
  stopping: { label: "停止中", tone: "pending" },
  stopped: { label: "已停止", tone: "stopped" },
  crashed: { label: "崩溃", tone: "failed" },
  failed: { label: "失败", tone: "failed" },
};
const LIFECYCLE = new Map([
  ["agent_created", "已创建"], ["agent_ready", "已就绪"],
  ["agent_start", "开始执行"], ["agent_started", "开始执行"],
  ["agent_settled", "进入待命"], ["status_changed", "状态变化"],
  ["extension_ui_request", "等待输入"], ["stop_requested", "请求停止"],
  ["restart_requested", "请求重启"], ["process_exit", "进程退出"],
  ["agent_stopped", "已停止"], ["agent_start_failed", "启动失败"],
  ["process_error", "进程异常"], ["agent_failed", "运行失败"],
]);
const REQUIRED_CARRIERS = [
  { id: "pi", label: "Pi Agent CLI" },
  { id: "codex", label: "Codex CLI" },
  { id: "claude", label: "Claude Code CLI" },
];
const API_FORMATS = {
  pi: [["openai-completions", "OpenAI Chat Completions"], ["openai-responses", "OpenAI Responses"], ["anthropic-messages", "Anthropic Messages"]],
  codex: [["responses", "OpenAI Responses"]],
  claude: [["anthropic", "Anthropic Messages"]],
};
const CLAUDE_AUTH_MODES = [["api-key", "Anthropic API Key · x-api-key"], ["bearer", "Bearer Token · Authorization"]];

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      authorization: `Bearer ${token}`,
      ...(options.body ? { "content-type": "application/json" } : {}),
      ...options.headers,
    },
  });
  let payload;
  try { payload = await response.json(); }
  catch { throw new Error(`HTTP ${response.status}: 服务响应无法解析`); }
  if (!response.ok || !payload.ok) throw new Error(payload.error?.message || `HTTP ${response.status}`);
  return payload.data;
}

function toast(message, error = false) {
  const node = $("#toast");
  node.textContent = message;
  node.className = `toast show${error ? " error" : ""}`;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { node.className = "toast"; }, 3200);
}

function short(value, length = 24) {
  const text = String(value ?? "");
  return text.length > length ? `${text.slice(0, length)}…` : text || "—";
}
function timeLabel(value) {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value);
  return new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(date);
}
function statusMeta(value) { return STATUS[value] || { label: String(value || "未知"), tone: "unknown" }; }
function carrierId(agent) { return String(agent.carrier || agent.carrier_id || "pi"); }
function carrierLabel(id) {
  return state.carriers.find((carrier) => carrier.id === id)?.displayName
    || state.carriers.find((carrier) => carrier.id === id)?.label
    || REQUIRED_CARRIERS.find((carrier) => carrier.id === id)?.label
    || id;
}
function modelLabel(agent) {
  const provider = agent.provider && agent.provider !== "unknown" ? agent.provider : "";
  const model = agent.model && agent.model !== "unknown" ? agent.model : "";
  return [provider, model].filter(Boolean).join("/") || "载体默认";
}
function providerModels(provider) {
  return (Array.isArray(provider?.modelList) ? provider.modelList : [])
    .map((item) => typeof item === "string" ? item : item?.id || item?.name || "")
    .filter(Boolean);
}
function providerReady(provider) { return provider?.configured === true && provider?.ready === true; }
function safeBaseUrl(value) {
  if (!value) return "未配置";
  try {
    const url = new URL(String(value));
    return `${url.origin}${url.pathname}`;
  } catch { return "地址不可用"; }
}
function providerAppList(app) {
  const list = state.providers.apps?.[app];
  return Array.isArray(list) ? list : [];
}
function sourceLabel(source) {
  if (typeof source === "string") return source;
  if (source && typeof source === "object") return source.path || source.uri || source.name || "来源未标注";
  return "来源未标注";
}
function skillName(id) { return state.skills.find((skill) => skill.id === id)?.name || id; }
function skillPolicy(agent) {
  const policy = agent.skillPolicy || agent.skill_policy;
  if (policy && typeof policy === "object") return policy;
  return null;
}
function skillIds(agent) {
  const ids = agent.skillIds || agent.skill_ids || skillPolicy(agent)?.ids;
  return Array.isArray(ids) ? ids : [];
}
function lastEvent(agent) { return state.recentEvents.get(agent.id); }

function setConnection(online) {
  const node = $("#connection");
  node.className = `connection ${online ? "online" : "offline"}`;
  node.innerHTML = `<i></i>${online ? "本地在线" : "连接中断"}`;
}

function renderMetrics() {
  const counts = { running: 0, waiting_input: 0, crashed: 0, failed: 0 };
  for (const agent of state.agents) if (agent.status in counts) counts[agent.status] += 1;
  $("#hero-active").textContent = state.activeCount;
  $("#metric-total").textContent = state.managedCount;
  $("#metric-running").textContent = counts.running;
  $("#metric-waiting").textContent = counts.waiting_input;
  $("#metric-faulted").textContent = counts.crashed + counts.failed;
  $("#metric-carriers").textContent = state.carriers.filter((item) => item.available).length;
  $("#metric-skills").textContent = state.skills.length;
  $("#nav-skill-count").textContent = state.skills.length;
  $("#agent-count").textContent = `${state.agents.length} 个实例`;
  $("#skill-count").textContent = `${state.skills.length} 份快照`;
  $("#last-sync").textContent = `同步于 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`;
  renderActivityTrack();
}

function renderActivityTrack() {
  const track = $("#activity-track");
  track.replaceChildren();
  if (!state.agents.length) {
    const empty = document.createElement("span");
    empty.className = "activity-empty";
    empty.textContent = "暂无实例";
    track.append(empty);
    return;
  }
  for (const agent of state.agents.slice(0, 48)) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `activity-node ${statusMeta(agent.status).tone}`;
    button.title = `${agent.name} · ${statusMeta(agent.status).label}`;
    button.setAttribute("aria-label", button.title);
    button.addEventListener("click", () => { setView("agents"); selectAgent(agent.id); });
    track.append(button);
  }
  if (state.agents.length > 48) {
    const more = document.createElement("span");
    more.className = "activity-more";
    more.textContent = `+${state.agents.length - 48}`;
    track.append(more);
  }
}

function renderCarrierFilter() {
  const select = $("#carrier-filter");
  const current = select.value;
  const ids = [...new Set([...state.carriers.map((item) => item.id), ...state.agents.map(carrierId)])];
  select.replaceChildren(new Option("全部载体", ""));
  for (const id of ids) select.add(new Option(carrierLabel(id), id));
  select.value = ids.includes(current) ? current : "";
}

function agentCard(agent) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = `agent-card${agent.id === state.selectedId ? " selected" : ""}`;
  if (agent.id === state.selectedId) button.setAttribute("aria-current", "true");
  const status = statusMeta(agent.status);
  const top = document.createElement("div"); top.className = "card-top";
  const carrier = document.createElement("span"); carrier.className = "card-carrier"; carrier.textContent = carrierLabel(carrierId(agent));
  const pill = document.createElement("span"); pill.className = `status-pill ${status.tone}`;
  const dot = document.createElement("i"); dot.className = `dot ${status.tone}`; dot.setAttribute("aria-hidden", "true");
  pill.append(dot, document.createTextNode(status.label));
  top.append(carrier, pill);
  const name = document.createElement("strong"); name.className = "card-name"; name.textContent = agent.name;
  const id = document.createElement("code"); id.className = "card-id"; id.textContent = short(agent.id, 36); id.title = agent.id;
  const meta = document.createElement("div"); meta.className = "card-meta";
  const model = document.createElement("span"); model.textContent = short(modelLabel(agent), 30); model.title = modelLabel(agent);
  const policy = document.createElement("span");
  const mode = skillPolicy(agent)?.mode;
  policy.textContent = mode === "only" ? `限定 ${skillIds(agent).length} 个 Skill` : mode === "all" ? "全部 Skill" : "旧版 Profile";
  meta.append(model, policy);
  const foot = document.createElement("div"); foot.className = "card-foot";
  const event = lastEvent(agent);
  const recent = document.createElement("span");
  recent.textContent = agent.last_error ? `错误 · ${short(agent.last_error, 62)}` : event ? `${event.event_type} · ${short(eventSummary(event), 52)}` : "暂无最近事件";
  recent.title = agent.last_error || (event ? eventSummary(event) : "");
  const time = document.createElement("time"); time.textContent = timeLabel(agent.created_at);
  foot.append(recent, time);
  button.append(top, name, id, meta, foot);
  button.addEventListener("click", () => selectAgent(agent.id));
  return button;
}

function renderAgentList() {
  const query = $("#filter").value.trim().toLocaleLowerCase();
  const carrier = $("#carrier-filter").value;
  const status = $("#status-filter").value;
  const agents = state.agents.filter((agent) =>
    (!query || `${agent.name} ${agent.id}`.toLocaleLowerCase().includes(query))
    && (!carrier || carrierId(agent) === carrier)
    && (!status || agent.status === status));
  const list = $("#agent-list");
  list.replaceChildren();
  if (!agents.length) {
    const empty = document.createElement("div");
    empty.className = "list-empty";
    empty.textContent = state.agents.length ? "没有符合筛选条件的 Agent。" : "尚无 Agent。使用右上角按钮创建第一个实例。";
    list.append(empty);
  } else for (const agent of agents) list.append(agentCard(agent));
}

function renderSkillGallery() {
  const list = $("#skills-list");
  list.replaceChildren();
  if (!state.skills.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    const title = document.createElement("h3"); title.textContent = "广场尚无 Skill 快照";
    const body = document.createElement("p"); body.textContent = "确认首批清单并导入后，Skill 的来源、版本和摘要会显示在这里。";
    empty.append(title, body);
    list.append(empty);
    return;
  }
  for (const skill of state.skills) {
    const card = document.createElement("article"); card.className = "skill-card";
    const head = document.createElement("div"); head.className = "skill-head";
    const glyph = document.createElement("span"); glyph.className = "skill-glyph"; glyph.textContent = "✳"; glyph.setAttribute("aria-hidden", "true");
    const version = document.createElement("code"); version.textContent = `${skill.versions?.length || 1} 快照版本`;
    head.append(glyph, version);
    const name = document.createElement("h3"); name.textContent = skill.name || skill.id;
    const id = document.createElement("code"); id.className = "skill-id"; id.textContent = skill.id;
    const facts = document.createElement("dl");
    for (const [label, value] of [["来源", sourceLabel(skill.source)], ["SHA-256", skill.currentSha256 || skill.digest || "—"], ["导入", timeLabel(skill.importedAt || skill.imported_at)]]) {
      const dt = document.createElement("dt"); dt.textContent = label;
      const dd = document.createElement("dd"); dd.textContent = value; dd.title = value;
      facts.append(dt, dd);
    }
    card.append(head, name, id, facts);
    list.append(card);
  }
}

function renderProviders() {
  const switchState = state.providers.switch || {};
  const available = Boolean(switchState.available) && !state.providersError;
  const status = $("#provider-switch-status");
  status.className = `provider-status ${available ? "ready" : "unavailable"}`;
  status.textContent = available
    ? `CC Switch CLI 分支已就绪${switchState.version ? ` · ${switchState.version}` : ""} · 应用私有配置`
    : `CC Switch 暂不可用 · ${state.providersError || switchState.reason || "请检查应用内的 CC Switch 安装与配置"}`;
  $("#new-provider").disabled = !available;

  const container = $("#provider-apps");
  container.replaceChildren();
  for (const app of REQUIRED_CARRIERS) {
    const list = providerAppList(app.id);
    const section = document.createElement("section"); section.className = "provider-app";
    const head = document.createElement("div"); head.className = "provider-app-head";
    const identity = document.createElement("div");
    const eyebrow = document.createElement("span"); eyebrow.className = "overline"; eyebrow.textContent = app.id.toUpperCase();
    const title = document.createElement("h3"); title.textContent = app.label;
    identity.append(eyebrow, title);
    const count = document.createElement("span"); count.className = "view-counter"; count.textContent = `${list.length} 个供应商`;
    head.append(identity, count); section.append(head);
    const note = document.createElement("p"); note.className = "provider-app-note";
    note.textContent = app.id === "pi"
      ? "Pi 的模型由创建 Agent 时的参数指定；这里没有全局默认供应商切换。"
      : "切换此载体的当前默认配置。创建 Agent 时也可单独指定供应商和模型。";
    section.append(note);
    if (!list.length) {
      const empty = document.createElement("p"); empty.className = "provider-empty";
      empty.textContent = available ? "尚未配置供应商。" : "当前无法读取供应商列表。";
      section.append(empty);
    } else {
      const grid = document.createElement("div"); grid.className = "provider-grid";
      for (const provider of list) {
        const card = document.createElement("article"); card.className = "provider-card";
        const top = document.createElement("div"); top.className = "provider-card-top";
        const nameBlock = document.createElement("div"); nameBlock.className = "provider-card-name";
        const name = document.createElement("h4"); name.textContent = provider.name || provider.id;
        const id = document.createElement("code"); id.textContent = provider.id || "—";
        nameBlock.append(name, id);
        const ready = providerReady(provider);
        const badge = document.createElement("span"); badge.className = `provider-badge${ready && provider.isCurrent ? " current" : ""}${!ready ? " unconfigured" : ""}`;
        badge.textContent = !ready ? "未配置" : app.id === "pi" ? "按实例选择" : provider.isCurrent ? "当前默认" : "可切换";
        top.append(nameBlock, badge); card.append(top);
        const facts = document.createElement("dl"); facts.className = "provider-facts";
        const factRows = [["API URL", safeBaseUrl(provider.baseUrl)], ["协议", provider.apiFormat || "未标注"]];
        if (app.id === "claude") {
          factRows.push(["认证方式", CLAUDE_AUTH_MODES.find(([id]) => id === provider.authMode)?.[1] || "未标注"]);
        }
        factRows.push([app.id === "pi" ? "预设模型" : "默认模型", provider.defaultModel || "未设置"]);
        for (const [label, value] of factRows) {
          const dt = document.createElement("dt"); dt.textContent = label;
          const dd = document.createElement("dd"); dd.textContent = value; dd.title = value;
          facts.append(dt, dd);
        }
        card.append(facts);
        const models = document.createElement("div"); models.className = "provider-models";
        const label = document.createElement("span"); label.textContent = `模型列表 · ${providerModels(provider).length}`;
        models.append(label);
        const chips = document.createElement("div"); chips.className = "provider-model-chips";
        for (const model of providerModels(provider)) {
          const chip = document.createElement("code"); chip.textContent = model; chip.title = model; chips.append(chip);
        }
        if (!chips.childElementCount) {
          const missing = document.createElement("small"); missing.textContent = "未配置模型"; chips.append(missing);
        }
        models.append(chips); card.append(models);
        if (!ready) {
          const guidance = document.createElement("p"); guidance.className = "provider-unconfigured-note";
          guidance.textContent = "官方预设尚缺 API 地址、Key 或模型，不能用于创建 Agent 或切换。";
          card.append(guidance);
        }
        if (app.id !== "pi") {
          const action = document.createElement("button"); action.type = "button";
          action.className = "button button-secondary provider-switch-button";
          action.textContent = !ready ? "配置后可用" : provider.isCurrent ? "正在使用" : "设为默认 ↗";
          action.disabled = !ready || provider.isCurrent || !available;
          if (ready && !provider.isCurrent) action.addEventListener("click", () => switchProvider(app.id, provider.id, action));
          card.append(action);
        }
        grid.append(card);
      }
      section.append(grid);
    }
    container.append(section);
  }
}

function renderCreateModels({ selectDefault = false } = {}) {
  const carrier = $("#create-carrier").value;
  const providerId = $("#create-provider").value;
  const provider = providerAppList(carrier).find((item) => item.id === providerId && providerReady(item));
  const select = $("#create-model");
  const previous = select.value;
  const models = providerModels(provider);
  select.replaceChildren(new Option(carrier === "pi" ? "选择模型" : provider?.defaultModel ? `供应商默认 · ${provider.defaultModel}` : "使用供应商默认模型", ""));
  for (const model of models) select.add(new Option(model, model));
  select.value = !selectDefault && models.includes(previous) ? previous
    : carrier === "pi" && models.includes(provider?.defaultModel) ? provider.defaultModel : "";
  select.disabled = !models.length;
  const note = $("#create-provider-note");
  if (state.providersError || !state.providers.switch?.available) {
    note.textContent = `供应商列表不可用：${state.providersError || state.providers.switch?.reason || "CC Switch 尚未就绪"}。请恢复应用私有的 CC Switch 配置后再创建 Agent。`;
  } else if (!providerAppList(carrier).some(providerReady)) {
    note.textContent = carrier === "pi"
      ? "尚无已配置的 Pi 供应商。请先在供应商页添加；模型 ID 可在创建时覆盖。"
      : "此载体尚无已配置的供应商。请先到供应商页添加；官方预设的未配置条目不可选。";
  } else if (carrier === "pi") {
    note.textContent = "Pi 按此实例选择供应商和模型，没有全局默认切换。手工模型 ID 会覆盖下拉选择。";
  } else {
    note.textContent = "下拉项来自此载体的 CC Switch 配置；手工模型 ID 会覆盖下拉选择。";
  }
}

function renderCreateProviders({ selectDefault = false } = {}) {
  const carrier = $("#create-carrier").value;
  const select = $("#create-provider");
  const previous = select.value;
  const providers = providerAppList(carrier).filter(providerReady);
  select.replaceChildren(new Option("使用载体已有配置", ""));
  for (const provider of providers) select.add(new Option(`${provider.name || provider.id}${provider.isCurrent && carrier !== "pi" ? " · 当前默认" : ""}`, provider.id));
  const preferred = providers.find((item) => item.isCurrent)?.id || providers[0]?.id || "";
  select.value = !selectDefault && providers.some((item) => item.id === previous) ? previous : preferred;
  select.disabled = !providers.length;
  renderCreateModels({ selectDefault: selectDefault || select.value !== previous });
}

function updateThinkingControl() {
  const codex = $("#create-carrier").value === "codex";
  const control = $("#create-thinking");
  if (codex) control.value = "";
  control.disabled = codex;
  $("#create-thinking-note").textContent = codex
    ? "CC Switch shared-sessions 当前不支持逐 Agent 覆盖，使用供应商配置。"
    : "留空时使用载体默认值。";
}

async function loadProviders({ quiet = true } = {}) {
  if (state.providersBusy) return;
  state.providersBusy = true;
  try {
    const data = await api("/v2/providers");
    state.providers = {
      switch: data?.switch || { available: false, reason: "CC Switch 状态未知" },
      apps: data?.apps || { pi: [], codex: [], claude: [] },
    };
    state.providersError = null;
  } catch (error) {
    state.providers = { switch: { available: false, reason: "供应商 API 不可用" }, apps: { pi: [], codex: [], claude: [] } };
    state.providersError = error.message;
    if (!quiet) toast(`供应商列表读取失败：${error.message}`, true);
  } finally {
    state.providersBusy = false;
    renderProviders(); renderCreateProviders();
  }
}

async function switchProvider(app, id, button) {
  button.disabled = true;
  try {
    await api(`/v2/providers/${encodeURIComponent(app)}/${encodeURIComponent(id)}/switch`, { method: "POST", body: "{}" });
    await loadProviders();
    toast(`${carrierLabel(app)} 默认供应商已切换`);
  } catch (error) {
    button.disabled = false;
    toast(`切换失败：${error.message}`, true);
  }
}

function renderCreateCarriers() {
  const select = $("#create-carrier");
  const current = select.value;
  const carriers = [...REQUIRED_CARRIERS];
  for (const carrier of state.carriers) if (!carriers.some((item) => item.id === carrier.id)) carriers.push({ id: carrier.id, label: carrier.displayName || carrier.label });
  select.replaceChildren();
  const availability = $("#carrier-availability");
  availability.replaceChildren();
  for (const item of carriers) {
    const carrier = state.carriers.find((row) => row.id === item.id);
    const label = carrier?.displayName || carrier?.label || item.label;
    const option = new Option(carrier?.available ? label : `${label} · 不可用`, item.id);
    option.disabled = !carrier?.available;
    option.title = carrier?.reason || (!carrier ? "载体尚未注册" : "");
    select.add(option);
    const row = document.createElement("div"); row.className = "carrier-availability-row";
    const title = document.createElement("strong"); title.textContent = label;
    const detail = document.createElement("span");
    detail.className = carrier?.available ? "available" : "unavailable";
    detail.textContent = carrier?.available ? "可用" : `不可用 · ${carrier?.reason || "载体尚未注册"}`;
    row.append(title, detail); availability.append(row);
  }
  const preferred = carriers.find((item) => item.id === current && state.carriers.find((row) => row.id === current)?.available)?.id
    || carriers.find((item) => state.carriers.find((row) => row.id === item.id)?.available)?.id;
  select.value = preferred || "";
  $("#create-submit").disabled = !preferred;
  renderCarrierNote({ selectDefault: select.value !== current });
}
function renderCarrierNote({ selectDefault = false } = {}) {
  const id = $("#create-carrier").value;
  const node = $("#create-carrier-note");
  if (!node) return;
  node.textContent = id ? (state.carriers.find((row) => row.id === id)?.reason || "") : "当前没有可用载体。";
  renderCreateProviders({ selectDefault });
  updateThinkingControl();
  updateSkillSelector();
}
function renderCreateSkills() {
  const container = $("#create-skill-options");
  const selected = new Set([...container.querySelectorAll('input[type="checkbox"]:checked')].map((input) => input.value));
  container.replaceChildren();
  if (!state.skills.length) {
    const note = document.createElement("p"); note.className = "skill-selector-empty"; note.textContent = "尚无已导入 Skill。选择“仅指定”将创建无 Skill 的 Agent。";
    container.append(note);
    return;
  }
  for (const skill of state.skills) {
    const row = document.createElement("label"); row.className = "skill-check";
    const input = document.createElement("input"); input.type = "checkbox"; input.name = "skill-id"; input.value = skill.id; input.checked = selected.has(skill.id);
    const text = document.createElement("span");
    const name = document.createElement("strong"); name.textContent = skill.name || skill.id;
    const details = document.createElement("small"); details.textContent = `${skill.id} · ${short(skill.currentSha256 || skill.digest, 18)}`;
    text.append(name, details); row.append(input, text); container.append(row);
  }
}
function updateSkillSelector() {
  const only = document.querySelector('input[name="skill-mode"]:checked')?.value === "only";
  $("#create-skill-selector").classList.toggle("hidden", !only);
  const carrier = state.carriers.find((item) => item.id === $("#create-carrier").value);
  const selectionAvailable = Boolean(carrier?.capabilities?.skillLoadingSelection);
  const note = $("#policy-boundary-note");
  note.classList.toggle("hidden", !only);
  note.textContent = only
    ? selectionAvailable
      ? "仅限制平台配置的 Skill 自动发现和加载；不隔离文件、凭据、网络或载体内置能力。"
      : "该载体无法选择自动加载的 Skill 集，平台会拒绝创建。"
    : "";
  const hasProvider = !state.providersError && state.providers.switch?.available
    && providerAppList(carrier?.id).some(providerReady);
  $("#create-submit").disabled = !carrier?.available || !hasProvider || (only && !selectionAvailable);
}

function eventSummary(event) {
  const payload = event.payload || {};
  const type = event.event_type || event.eventType || "event";
  if (type === "message_end" || (payload.message && typeof payload.message === "object")) {
    const message = payload.message || {};
    const content = message.content;
    if (typeof content === "string") return short(content, 180);
    if (Array.isArray(content)) {
      const text = content.map((part) => typeof part === "string" ? part : part?.text || "").filter(Boolean).join(" ");
      if (text) return short(text, 180);
    }
  }
  if (type === "message_update") {
    const update = payload.assistantMessageEvent || {};
    return String(update.delta || update.content || update.type || "消息更新");
  }
  if (type.startsWith("tool_execution")) return `${payload.toolName || "工具"}${payload.isError ? " · 错误" : ""}`;
  if (type === "agent_created") return `${payload.workspaceMode || "shared"} · ${payload.cwd || ""}`;
  if (type === "agent_ready") return `PID ${payload.pid || "—"} · session ${short(payload.sessionId, 18)}`;
  if (payload.message) return String(payload.message);
  if (payload.status) return `状态：${statusMeta(payload.status).label}`;
  if (payload.kind) return String(payload.kind);
  return "";
}

function renderTimeline(agent) {
  const container = $("#timeline"); container.replaceChildren();
  const entries = state.events.filter((event) => LIFECYCLE.has(event.event_type || event.eventType)).slice(-8).reverse();
  if (!entries.length) {
    const empty = document.createElement("p"); empty.className = "timeline-empty";
    empty.textContent = `当前状态：${statusMeta(agent.status).label}。暂无生命周期事件。`;
    container.append(empty);
  }
  for (const event of entries) {
    const type = event.event_type || event.eventType;
    const row = document.createElement("div"); row.className = "timeline-row";
    const marker = document.createElement("i"); marker.className = "timeline-marker"; marker.setAttribute("aria-hidden", "true");
    const body = document.createElement("div");
    const title = document.createElement("strong"); title.textContent = LIFECYCLE.get(type);
    const summary = document.createElement("p"); summary.textContent = eventSummary(event);
    body.append(title, summary);
    const time = document.createElement("time"); time.textContent = timeLabel(event.created_at || event.createdAt);
    row.append(marker, body, time); container.append(row);
  }
  $("#timeline-count").textContent = `${entries.length} 项`;
}

function renderEvents() {
  const container = $("#events"); container.replaceChildren();
  const events = state.events.slice(-60).reverse();
  if (!events.length) {
    const empty = document.createElement("p"); empty.className = "event-empty"; empty.textContent = "暂无事件。"; container.append(empty);
  }
  for (const event of events) {
    const row = document.createElement("article"); row.className = "event-row";
    const time = document.createElement("time"); time.textContent = timeLabel(event.created_at || event.createdAt);
    const type = document.createElement("span"); type.className = "event-type"; type.textContent = event.event_type || event.eventType || "event";
    const summary = document.createElement("span"); summary.className = "event-summary"; summary.textContent = eventSummary(event);
    if ($("#verbose-events").checked) {
      const pre = document.createElement("pre"); pre.textContent = JSON.stringify(event.payload, null, 2); summary.append(pre);
    }
    row.append(time, type, summary); container.append(row);
  }
}

function renderDetail() {
  const agent = state.agents.find((item) => item.id === state.selectedId);
  $("#empty-state").classList.toggle("hidden", Boolean(agent));
  $("#agent-detail").classList.toggle("hidden", !agent);
  if (!agent) return;
  const status = statusMeta(agent.status);
  $("#detail-name").textContent = agent.name;
  $("#detail-id").textContent = agent.id;
  $("#detail-carrier-tag").textContent = carrierLabel(carrierId(agent));
  $("#detail-carrier").textContent = carrierLabel(carrierId(agent));
  $("#detail-status").textContent = status.label;
  $("#detail-status-indicator").className = `status-pill ${status.tone}`;
  $("#detail-status-indicator .dot").className = `dot ${status.tone}`;
  $("#detail-created").textContent = timeLabel(agent.created_at);
  $("#detail-updated").textContent = timeLabel(agent.updated_at);
  $("#detail-model").textContent = modelLabel(agent);
  $("#detail-thinking").textContent = agent.thinking || "默认";
  $("#detail-process").textContent = agent.pid ? `PID ${agent.pid}` : short(agent.session_id, 24);
  $("#detail-process").title = agent.session_id || "";
  $("#detail-cwd").textContent = agent.cwd || "—";
  $("#detail-cwd").title = agent.cwd || "";
  const policy = skillPolicy(agent);
  const ids = skillIds(agent);
  const permissionList = $("#detail-skill-list"); permissionList.replaceChildren();
  if (policy?.mode === "only") {
    $("#detail-skill-policy").textContent = ids.length ? `仅指定 ${ids.length} 个 Skill` : "无 Skill";
    for (const id of ids) { const chip = document.createElement("span"); chip.textContent = skillName(id); chip.title = id; permissionList.append(chip); }
  } else if (policy?.mode === "all") {
    $("#detail-skill-policy").textContent = `全部 ${ids.length} 个 Skill（创建时固定）`;
    for (const id of ids) { const chip = document.createElement("span"); chip.textContent = skillName(id); chip.title = id; permissionList.append(chip); }
  } else {
    $("#detail-skill-policy").textContent = "旧版 Profile 授权";
    const count = Array.isArray(agent.skills) ? agent.skills.length : 0;
    const chip = document.createElement("span"); chip.textContent = `${count} 个 Profile Skill`; permissionList.append(chip);
  }
  const error = $("#detail-error"); error.classList.toggle("hidden", !agent.last_error);
  error.querySelector("p").textContent = agent.last_error || "";
  const capabilities = state.carriers.find((item) => item.id === carrierId(agent))?.capabilities;
  const allowedMessages = capabilities?.messages || ["prompt", "steer", "follow_up", "abort"];
  const kindSelect = $("#message-kind");
  const previousKind = kindSelect.value;
  kindSelect.replaceChildren();
  for (const [value, capability, label] of [["prompt", "prompt", "消息"], ["steer", "steer", "调整当前任务"], ["follow-up", "follow_up", "后续任务"]]) {
    if (allowedMessages.includes(capability)) kindSelect.add(new Option(label, value));
  }
  if ([...kindSelect.options].some((option) => option.value === previousKind)) kindSelect.value = previousKind;
  for (const button of document.querySelectorAll("[data-action]")) {
    const action = button.dataset.action;
    button.disabled = action === "restart" ? ["provisioning", "starting", "stopping"].includes(agent.status)
      : action === "abort" ? !allowedMessages.includes("abort") || !["running", "waiting_input"].includes(agent.status)
        : !["running", "idle", "waiting_input", "starting"].includes(agent.status);
  }
  const busyCli = carrierId(agent) !== "pi" && agent.status === "running";
  $("#message-form").querySelector("button[type=submit]").disabled = busyCli || !["running", "idle", "waiting_input"].includes(agent.status);
  $("#message").placeholder = busyCli ? "当前回合执行中，请等待完成后发送下一条消息。" : "向当前 Agent 发送消息…";
  renderTimeline(agent); renderEvents();
}

async function fetchDetailEvents(id, snapshotCursor = state.eventCursor) {
  const after = Math.max(0, Number(snapshotCursor || 0) - 1200);
  try {
    const events = await api(`/v2/agents/${encodeURIComponent(id)}/events?after=${after}&limit=1200`);
    if (state.selectedId !== id) return;
    state.events = Array.isArray(events) ? events : [];
    state.detailCursor = snapshotCursor;
    renderDetail();
  } catch (error) {
    if (state.selectedId !== id) return;
    state.events = [];
    renderDetail();
    toast(`事件读取失败：${error.message}`, true);
  }
}
async function selectAgent(id) {
  if (state.selectedId !== id) {
    state.selectedId = id;
    state.events = [];
    state.detailCursor = null;
  }
  renderAgentList(); renderDetail();
  await fetchDetailEvents(id);
}

async function loadMetadata({ quiet = true } = {}) {
  if (state.metadataBusy) return;
  state.metadataBusy = true;
  const [carriersResult, skillsResult] = await Promise.allSettled([api("/v2/carriers"), api("/v2/skills")]);
  if (carriersResult.status === "fulfilled") state.carriers = Array.isArray(carriersResult.value) ? carriersResult.value : [];
  if (skillsResult.status === "fulfilled") state.skills = Array.isArray(skillsResult.value) ? skillsResult.value : [];
  state.metadataBusy = false;
  renderCarrierFilter(); renderCreateCarriers(); renderCreateSkills(); renderSkillGallery(); renderMetrics(); renderAgentList(); renderDetail();
  if (!quiet && carriersResult.status === "rejected") toast(`载体列表读取失败：${carriersResult.reason.message}`, true);
  else if (!quiet && skillsResult.status === "rejected") toast(`Skill 广场读取失败：${skillsResult.reason.message}`, true);
}

async function refresh({ quiet = false } = {}) {
  if (state.refreshBusy) return;
  state.refreshBusy = true;
  try {
    const snapshot = await api("/v2/snapshot");
    state.agents = Array.isArray(snapshot.agents) ? snapshot.agents : [];
    state.activeCount = Number.isFinite(snapshot.activeCount) ? snapshot.activeCount
      : state.agents.filter((agent) => ["running", "idle", "waiting_input", "starting", "provisioning"].includes(agent.status)).length;
    state.managedCount = Number.isFinite(snapshot.managedCount) ? snapshot.managedCount
      : state.agents.filter((agent) => ["running", "idle", "waiting_input", "starting", "provisioning"].includes(agent.status)).length;
    setConnection(true);
    const cursor = Number(snapshot.eventCursor || 0);
    if (cursor !== state.eventCursor) {
      const after = state.eventCursor === null ? Math.max(0, cursor - 300) : state.eventCursor;
      const events = await api(`/v2/events?after=${after}&limit=500`);
      for (const event of events) if (event.agent_id) state.recentEvents.set(event.agent_id, event);
      state.eventCursor = events.length === 500 ? Number(events.at(-1).cursor) : cursor;
      if (state.selectedId) await fetchDetailEvents(state.selectedId, cursor);
    }
    if (state.selectedId && !state.agents.some((agent) => agent.id === state.selectedId)) {
      state.selectedId = null; state.events = []; state.detailCursor = null;
    }
    renderCarrierFilter(); renderMetrics(); renderAgentList(); renderDetail();
    if (!quiet) toast("已同步最新状态");
  } catch (error) {
    setConnection(false);
    if (!quiet) toast(error.message, true);
  } finally { state.refreshBusy = false; }
}

function setView(view) {
  state.view = view;
  $("#agents-view").classList.toggle("hidden", view !== "agents");
  $("#skills-view").classList.toggle("hidden", view !== "skills");
  $("#providers-view").classList.toggle("hidden", view !== "providers");
  for (const button of document.querySelectorAll("[data-view]")) {
    const selected = button.dataset.view === view;
    button.classList.toggle("active", selected);
    if (selected) button.setAttribute("aria-current", "page"); else button.removeAttribute("aria-current");
  }
}
async function runAction(action) {
  if (!state.selectedId) return;
  try {
    await api(`/v2/agents/${encodeURIComponent(state.selectedId)}/${action}`, { method: "POST", body: "{}" });
    toast(`${{ restart: "重启", abort: "中止", stop: "停止" }[action]}指令已接收`);
    await refresh({ quiet: true });
  } catch (error) { toast(error.message, true); }
}

for (const button of document.querySelectorAll("[data-view]")) button.addEventListener("click", () => setView(button.dataset.view));
$("#refresh").addEventListener("click", async () => { await Promise.all([loadMetadata({ quiet: false }), loadProviders({ quiet: false }), refresh()]); });
for (const selector of ["#filter", "#carrier-filter", "#status-filter"]) $(selector).addEventListener(selector === "#filter" ? "input" : "change", renderAgentList);
$("#verbose-events").addEventListener("change", renderEvents);
$("#create-carrier").addEventListener("change", () => renderCarrierNote({ selectDefault: true }));
$("#create-provider").addEventListener("change", () => renderCreateModels({ selectDefault: true }));
for (const input of document.querySelectorAll('input[name="skill-mode"]')) input.addEventListener("change", updateSkillSelector);
$("#new-agent").addEventListener("click", async () => { await Promise.all([loadMetadata(), loadProviders()]); $("#create-error").textContent = ""; $("#create-dialog").showModal(); });
for (const button of document.querySelectorAll("[data-action]")) button.addEventListener("click", () => runAction(button.dataset.action));

$("#create-form").addEventListener("submit", async (event) => {
  if (event.submitter?.value === "cancel") return;
  event.preventDefault();
  const form = event.currentTarget;
  const data = new FormData(form);
  const carrier = String(data.get("carrier") || "");
  const carrierInfo = state.carriers.find((item) => item.id === carrier);
  const errorNode = $("#create-error"); errorNode.textContent = "";
  if (!carrierInfo?.available) { errorNode.textContent = carrierInfo?.reason || "所选载体不可用。"; return; }
  const selectedProvider = String(data.get("provider") || "").trim();
  const manualModel = String(data.get("model-manual") || "").trim();
  const selectedModel = String(data.get("model-choice") || "").trim();
  const modelValue = manualModel || selectedModel;
  const slash = selectedProvider ? -1 : modelValue.indexOf("/");
  const mode = String(data.get("skill-mode") || "all");
  if (mode === "only" && !carrierInfo.capabilities?.skillLoadingSelection) {
    errorNode.textContent = "当前载体无法选择自动加载的 Skill 集。";
    return;
  }
  const body = {
    name: String(data.get("name") || "").trim(),
    cwd: String(data.get("cwd") || "").trim(),
    provider: selectedProvider || (slash > 0 ? modelValue.slice(0, slash) : null),
    model: slash > 0 ? modelValue.slice(slash + 1) : modelValue || null,
    thinking: data.get("thinking") || null,
    prompt: String(data.get("prompt") || "").trim() || null,
    workspaceMode: "shared",
    carrier,
    skillPolicy: mode === "only" ? { mode: "only", ids: data.getAll("skill-id").map(String) } : { mode: "all" },
  };
  $("#create-submit").disabled = true;
  try {
    const created = await api("/v2/agents", { method: "POST", body: JSON.stringify(body) });
    form.reset(); renderCreateProviders({ selectDefault: true }); updateSkillSelector();
    $("#create-dialog").close();
    await refresh({ quiet: true });
    await selectAgent(created.id);
    setView("agents");
    toast("Agent 已创建");
  } catch (error) { errorNode.textContent = error.message; }
  finally { updateSkillSelector(); }
});

function closeProviderDialog() { $("#provider-dialog").close(); }
function renderApiFormats() {
  const app = $("#provider-form [name=app]").value;
  const select = $("#provider-api-format");
  select.replaceChildren();
  for (const [value, label] of API_FORMATS[app] || []) select.add(new Option(label, value));
  const claude = app === "claude";
  $("#claude-auth-field").classList.toggle("hidden", !claude);
  const authSelect = $("#claude-auth-mode");
  authSelect.disabled = !claude;
  authSelect.replaceChildren();
  if (claude) for (const [value, label] of CLAUDE_AUTH_MODES) authSelect.add(new Option(label, value));
  $("#provider-key-note").textContent = claude
    ? "api-key 使用 x-api-key；bearer 使用 Authorization。仅在本次保存时提交，页面不回显凭据。"
    : "仅用于本次保存请求，页面不会读取或显示已有凭据。";
}
$("#new-provider").addEventListener("click", () => { $("#provider-error").textContent = ""; $("#provider-dialog").showModal(); });
$("#provider-close").addEventListener("click", closeProviderDialog);
$("#provider-cancel").addEventListener("click", closeProviderDialog);
$("#provider-dialog").addEventListener("close", () => { $("#provider-form").reset(); $("#provider-error").textContent = ""; $("#provider-model-options").replaceChildren(); renderApiFormats(); });
$("#provider-form [name=app]").addEventListener("change", renderApiFormats);
renderApiFormats();
$("#provider-form [name=modelList]").addEventListener("input", (event) => {
  const models = [...new Set(event.target.value.split(/[\n,]/).map((item) => item.trim()).filter(Boolean))];
  const choices = $("#provider-model-options"); choices.replaceChildren();
  for (const model of models) choices.append(new Option(model, model));
});
$("#provider-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.currentTarget;
  const data = new FormData(form);
  const apiKey = String(data.get("apiKey") || "");
  const modelList = [...new Set(String(data.get("modelList") || "").split(/[\n,]/).map((item) => item.trim()).filter(Boolean))];
  const defaultModel = String(data.get("defaultModel") || "").trim() || modelList[0];
  const errorNode = $("#provider-error"); errorNode.textContent = "";
  if (modelList.length < 1 || modelList.length > 100) {
    errorNode.textContent = "模型列表需要包含 1 到 100 个模型 ID。";
    return;
  }
  if (defaultModel && !modelList.includes(defaultModel)) {
    errorNode.textContent = "默认模型必须出现在模型列表中。";
    return;
  }
  let baseUrl;
  try { baseUrl = new URL(String(data.get("baseUrl") || "").trim()); }
  catch { errorNode.textContent = "API URL 无效。"; return; }
  if (!["http:", "https:"].includes(baseUrl.protocol) || baseUrl.username || baseUrl.password || baseUrl.search || baseUrl.hash) {
    errorNode.textContent = "API URL 必须是未包含凭据和查询参数的 HTTP(S) 地址。";
    return;
  }
  const app = String(data.get("app") || "");
  const body = {
    app,
    id: String(data.get("id") || "").trim(),
    name: String(data.get("name") || "").trim(),
    baseUrl: String(data.get("baseUrl") || "").trim(),
    apiFormat: String(data.get("apiFormat") || "").trim(),
    apiKey,
    modelList,
    defaultModel,
    ...(app === "claude" ? { authMode: String(data.get("authMode") || "api-key") } : {}),
  };
  $("#provider-submit").disabled = true;
  try {
    await api("/v2/providers", { method: "POST", body: JSON.stringify(body) });
    closeProviderDialog();
    await loadProviders();
    toast("供应商已保存");
  } catch {
    errorNode.textContent = "保存失败。请检查供应商信息和 CC Switch 状态；API Key 不会显示在错误提示中。";
  } finally { $("#provider-submit").disabled = false; }
});

$("#message-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const message = $("#message").value.trim();
  if (!message || !state.selectedId) return;
  const action = $("#message-kind").value;
  try {
    await api(`/v2/agents/${encodeURIComponent(state.selectedId)}/${action}`, { method: "POST", body: JSON.stringify({ message }) });
    $("#message").value = "";
    toast("指令已接收");
    await refresh({ quiet: true });
  } catch (error) { toast(error.message, true); }
});

await Promise.all([loadMetadata(), loadProviders(), refresh({ quiet: true })]);
setInterval(() => refresh({ quiet: true }), 2400);
setInterval(() => { loadMetadata(); loadProviders(); }, 30000);
