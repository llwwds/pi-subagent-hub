const token = document.querySelector('meta[name="pihub-token"]').content;
const state = { agents: [], selectedId: null, events: [], profile: null, refreshTimer: null };
const $ = (selector) => document.querySelector(selector);

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      authorization: `Bearer ${token}`,
      ...(options.body ? { "content-type": "application/json" } : {}),
      ...options.headers,
    },
  });
  const payload = await response.json();
  if (!response.ok || !payload.ok) throw new Error(payload.error?.message || `HTTP ${response.status}`);
  return payload.data;
}

function toast(message, error = false) {
  const node = $("#toast");
  node.textContent = message;
  node.className = `toast show${error ? " error" : ""}`;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { node.className = "toast"; }, 2800);
}

function short(value, length = 14) {
  if (!value) return "—";
  return value.length > length ? `${value.slice(0, length)}…` : value;
}

function statusLabel(status) {
  return String(status || "unknown").replaceAll("_", " ").toUpperCase();
}

function modelLabel(agent, detailed = false) {
  const provider = agent.provider && agent.provider !== "unknown" ? agent.provider : null;
  const model = agent.model && agent.model !== "unknown" ? agent.model : null;
  if (!provider && !model) return detailed ? "NOT CONFIGURED" : "NO MODEL";
  return [provider, model].filter(Boolean).join("/");
}

function renderMetrics() {
  const counts = Object.fromEntries(["running", "idle", "waiting_input", "crashed", "failed"].map((key) => [key, 0]));
  for (const agent of state.agents) if (agent.status in counts) counts[agent.status] += 1;
  $("#metric-total").textContent = state.agents.length;
  $("#metric-running").textContent = counts.running;
  $("#metric-idle").textContent = counts.idle;
  $("#metric-waiting").textContent = counts.waiting_input;
  $("#metric-faulted").textContent = counts.crashed + counts.failed;
  $("#profile-digest").textContent = state.profile?.digest || "—";
  $("#profile-digest").title = state.profile?.digest || "";
}

function renderAgentList() {
  const query = $("#filter").value.trim().toLowerCase();
  const agents = state.agents.filter((agent) => !query || `${agent.name} ${agent.id}`.toLowerCase().includes(query));
  $("#agent-count").textContent = `${state.agents.length} AGENT${state.agents.length === 1 ? "" : "S"}`;
  const list = $("#agent-list");
  list.replaceChildren();
  for (const agent of agents) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `agent-row${agent.id === state.selectedId ? " selected" : ""}`;
    button.innerHTML = `
      <i class="status-light ${agent.status}"></i>
      <span class="agent-primary"><strong></strong><code></code></span>
      <span class="agent-meta"><span>${statusLabel(agent.status)}</span><code></code></span>`;
    button.querySelector("strong").textContent = agent.name;
    button.querySelector(".agent-primary code").textContent = short(agent.id, 19);
    button.querySelector(".agent-meta code").textContent = modelLabel(agent);
    button.addEventListener("click", () => selectAgent(agent.id));
    list.append(button);
  }
}

function eventSummary(event) {
  const payload = event.payload || {};
  if (event.event_type === "message_update") {
    const update = payload.assistantMessageEvent || {};
    return update.delta || update.content || update.type || "stream update";
  }
  if (event.event_type.startsWith("tool_execution")) return `${payload.toolName || "tool"}${payload.isError ? " · ERROR" : ""}`;
  if (event.event_type === "agent_created") return `${payload.workspaceMode || "shared"} · ${payload.cwd || ""}`;
  if (event.event_type === "agent_ready") return `PID ${payload.pid || "—"} · session ${short(payload.sessionId, 18)}`;
  if (payload.message) return payload.message;
  if (payload.kind) return payload.kind;
  return "";
}

function renderEvents() {
  const raw = $("#verbose-events").checked;
  const container = $("#events");
  container.replaceChildren();
  $("#event-count").textContent = `${state.events.length} EVENTS`;
  for (const event of state.events.slice(-250).reverse()) {
    const row = document.createElement("article");
    row.className = "event-row";
    const time = document.createElement("span");
    time.className = "event-time";
    time.textContent = new Date(event.created_at).toLocaleTimeString([], { hour12: false });
    const type = document.createElement("span");
    type.className = "event-type";
    type.textContent = event.event_type;
    type.title = event.event_type;
    const summary = document.createElement("span");
    summary.className = "event-summary";
    summary.textContent = eventSummary(event);
    if (raw) {
      const pre = document.createElement("pre");
      pre.textContent = JSON.stringify(event.payload, null, 2);
      summary.append(pre);
    }
    row.append(time, type, summary);
    container.append(row);
  }
}

function renderDetail() {
  const agent = state.agents.find((item) => item.id === state.selectedId);
  $("#empty-state").classList.toggle("hidden", Boolean(agent));
  $("#agent-detail").classList.toggle("hidden", !agent);
  if (!agent) return;
  $("#detail-name").textContent = agent.name;
  $("#detail-id").textContent = agent.id;
  $("#detail-status").textContent = statusLabel(agent.status);
  $("#detail-model").textContent = modelLabel(agent, true);
  $("#detail-thinking").textContent = agent.thinking || "DEFAULT";
  $("#detail-pid").textContent = agent.pid || "—";
  $("#detail-cwd").textContent = agent.cwd;
  $("#detail-cwd").title = agent.cwd;
  $("#detail-session").textContent = agent.session_id;
  renderEvents();
}

async function selectAgent(id) {
  state.selectedId = id;
  renderAgentList();
  try {
    state.events = await api(`/v1/agents/${encodeURIComponent(id)}/events?after=0&limit=1000`);
  } catch (error) {
    toast(error.message, true);
    state.events = [];
  }
  renderDetail();
}

async function refresh({ quiet = false } = {}) {
  try {
    const snapshot = await api("/v1/snapshot");
    state.agents = snapshot.agents;
    state.profile = snapshot.profile;
    $("#connection").className = "connection online";
    $("#connection").innerHTML = "<i></i> LOCAL / ONLINE";
    renderMetrics();
    renderAgentList();
    if (state.selectedId) {
      const stillExists = state.agents.some((agent) => agent.id === state.selectedId);
      if (!stillExists) state.selectedId = null;
      else state.events = await api(`/v1/agents/${encodeURIComponent(state.selectedId)}/events?after=0&limit=1000`);
      renderDetail();
    }
    if (!quiet) toast("CONTROL DECK SYNCHRONIZED");
  } catch (error) {
    $("#connection").className = "connection offline";
    $("#connection").innerHTML = "<i></i> OFFLINE";
    if (!quiet) toast(error.message, true);
  }
}

async function runAction(action) {
  if (!state.selectedId) return;
  try {
    await api(`/v1/agents/${encodeURIComponent(state.selectedId)}/${action}`, { method: "POST", body: "{}" });
    toast(`${action.toUpperCase()} ACCEPTED`);
    await refresh({ quiet: true });
  } catch (error) { toast(error.message, true); }
}

$("#refresh").addEventListener("click", () => refresh());
$("#filter").addEventListener("input", renderAgentList);
$("#verbose-events").addEventListener("change", renderEvents);
$("#new-agent").addEventListener("click", () => $("#create-dialog").showModal());
for (const button of document.querySelectorAll("[data-action]")) button.addEventListener("click", () => runAction(button.dataset.action));

$("#create-form").addEventListener("submit", async (event) => {
  const submitter = event.submitter;
  if (submitter?.value === "cancel") return;
  event.preventDefault();
  const form = event.currentTarget;
  const data = new FormData(form);
  const modelValue = String(data.get("model") || "");
  const slash = modelValue.indexOf("/");
  const body = {
    name: data.get("name"),
    cwd: data.get("cwd"),
    provider: slash > 0 ? modelValue.slice(0, slash) : null,
    model: slash > 0 ? modelValue.slice(slash + 1) : modelValue || null,
    thinking: data.get("thinking") || null,
    prompt: data.get("prompt") || null,
    workspaceMode: "shared",
  };
  const errorNode = $("#create-error");
  errorNode.textContent = "";
  try {
    const created = await api("/v1/agents", { method: "POST", body: JSON.stringify(body) });
    form.reset();
    $("#create-dialog").close();
    await refresh({ quiet: true });
    await selectAgent(created.id);
    toast("AGENT READY");
  } catch (error) { errorNode.textContent = error.message; }
});

$("#message-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const message = $("#message").value.trim();
  if (!message || !state.selectedId) return;
  const action = $("#message-kind").value;
  try {
    await api(`/v1/agents/${encodeURIComponent(state.selectedId)}/${action}`, {
      method: "POST",
      body: JSON.stringify({ message }),
    });
    $("#message").value = "";
    toast(`${action.toUpperCase()} ACCEPTED`);
    await refresh({ quiet: true });
  } catch (error) { toast(error.message, true); }
});

refresh({ quiet: true });
state.refreshTimer = setInterval(() => refresh({ quiet: true }), 1800);
