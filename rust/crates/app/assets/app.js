// Progressive enhancement for the server-rendered panel. The server remains the
// source of Agent state, labels, permissions and form actions.
(() => {
  const REFRESH_MS = 2400;
  const root = document.documentElement;
  const preferenceValues = {
    skin: ["cloud", "flow"],
    atmosphere: ["subtle", "balanced", "immersive"],
  };
  const defaults = { skin: "cloud", atmosphere: "balanced" };
  const drafts = new Map();
  let scope = "active";
  try {
    const savedScope = localStorage.getItem("pihub.scope");
    if (savedScope === "active" || savedScope === "all") scope = savedScope;
  } catch { /* storage may be unavailable; use the in-page default */ }
  let requestNumber = 0;
  let polling = false;
  let navigating = false;
  let renderedSearch = window.location.search;

  const readPreference = (name) => {
    try {
      const value = localStorage.getItem(`pihub.${name}`);
      if (preferenceValues[name].includes(value)) return value;
    } catch { /* storage may be unavailable; use the in-page default */ }
    return defaults[name];
  };

  const setPreference = (name, value, persist = true) => {
    if (!preferenceValues[name]?.includes(value)) return;
    root.dataset[name] = value;
    if (persist) {
      try { localStorage.setItem(`pihub.${name}`, value); } catch { /* optional */ }
    }
    for (const button of document.querySelectorAll(`[data-preference="${name}"]`)) {
      button.setAttribute("aria-pressed", String(button.dataset.value === value));
    }
    if (name === "skin") arrangeBoard();
  };

  const currentAgent = () => document.querySelector("#detail #detail-id")?.textContent.trim() || "";
  const saveDraft = () => {
    const id = currentAgent();
    const input = document.querySelector("#message-form #message");
    if (!id || !input) return;
    if (input.value) drafts.set(id, input.value);
    else drafts.delete(id);
  };
  const restoreDraft = () => {
    const id = currentAgent();
    const input = document.querySelector("#message-form #message");
    if (id && input && drafts.has(id)) input.value = drafts.get(id);
  };

  const agentUrl = (id) => {
    const url = new URL(window.location.href);
    url.searchParams.set("view", "agents");
    if (id) url.searchParams.set("agent", id);
    else url.searchParams.delete("agent");
    for (const transient of ["create", "create-provider", "toast", "error", "verbose"]) {
      url.searchParams.delete(transient);
    }
    url.hash = "";
    return url;
  };

  function arrangeBoard() {
    const canvas = document.getElementById("fleet-canvas");
    const cloud = document.getElementById("agent-list");
    const flow = document.getElementById("flow-lanes");
    if (!canvas || !cloud || !flow) return;
    const isFlow = root.dataset.skin === "flow";
    const lanes = [...flow.querySelectorAll(".flow-lane")];
    const byTone = new Map(lanes.map((lane) => [lane.dataset.tone, lane]));
    const nodes = [...canvas.querySelectorAll(".fleet-node")];
    const stoppedOnly = new URL(window.location.href).searchParams.get("status") === "stopped";
    const effectiveScope = stoppedOnly ? "all" : scope;
    let visible = 0;
    for (const node of nodes) {
      const lane = byTone.get(node.dataset.tone) || byTone.get("unknown");
      const target = isFlow ? lane.querySelector(".flow-lane-nodes") : cloud;
      if (node.parentElement !== target) target.append(node);
      const hidden = effectiveScope === "active" && node.dataset.tone === "stopped";
      if (node.hidden !== hidden) node.hidden = hidden;
      if (!node.hidden) visible += 1;
      const href = agentUrl(node.dataset.agentId).href;
      if (node.href !== href) node.href = href;
    }
    if (cloud.hidden !== isFlow) cloud.hidden = isFlow;
    if (flow.hidden === isFlow) flow.hidden = !isFlow;
    for (const lane of lanes) {
      const count = [...lane.querySelectorAll(".fleet-node")].filter((node) => !node.hidden).length;
      if (lane.hidden !== !count) lane.hidden = !count;
      const countLabel = lane.querySelector(".flow-lane-count");
      if (countLabel.textContent !== `${count}`) countLabel.textContent = `${count}`;
    }
    const scopeEmpty = document.getElementById("scope-empty");
    const hideScopeEmpty = nodes.length === 0 || visible !== 0;
    if (scopeEmpty && scopeEmpty.hidden !== hideScopeEmpty) scopeEmpty.hidden = hideScopeEmpty;
    const visibleCount = document.getElementById("fleet-visible-count");
    const visibleLabel = `当前展示 ${visible} 个实例`;
    if (visibleCount && visibleCount.textContent !== visibleLabel) visibleCount.textContent = visibleLabel;
    for (const button of document.querySelectorAll("[data-scope]")) {
      const selected = String(button.dataset.scope === effectiveScope);
      if (button.getAttribute("aria-pressed") !== selected) button.setAttribute("aria-pressed", selected);
      if (button.dataset.scope === "active") {
        if (button.disabled !== stoppedOnly) button.disabled = stoppedOnly;
        if (stoppedOnly && button.title !== "当前筛选为已停止实例") button.title = "当前筛选为已停止实例";
        else if (!stoppedOnly && button.hasAttribute("title")) button.removeAttribute("title");
      }
    }
  }

  const syncDetailMode = () => {
    const present = Boolean(document.getElementById("agent-detail"));
    document.body.classList.toggle("has-detail", present);
    document.body.classList.toggle("detail-page", present && window.location.hash === "#detail-page");
    const close = document.querySelector("[data-detail-close]");
    const back = document.querySelector("[data-detail-back]");
    const expand = document.querySelector("[data-detail-expand]");
    if (close) close.href = agentUrl("").href;
    const current = new URL(window.location.href);
    current.hash = "";
    if (back) back.href = current.href;
    if (expand) {
      current.hash = "detail-page";
      expand.href = current.href;
    }
  };

  const openDialog = (id) => {
    const dialog = document.getElementById(id);
    if (!dialog) return;
    if (typeof dialog.showModal === "function") dialog.showModal();
    else dialog.setAttribute("open", "");
  };

  // A closed dialog returns to its server-rendered defaults; opening it never
  // resets a form that is being edited.
  const resetDialogForm = (dialog) => {
    dialog?.querySelector("form")?.reset();
  };
  const closeDialog = (dialog) => {
    if (!dialog) return;
    if (typeof dialog.close === "function") dialog.close();
    else {
      dialog.removeAttribute("open");
      resetDialogForm(dialog);
    }
  };
  document.addEventListener("close", (event) => {
    if (event.target instanceof HTMLDialogElement) {
      resetDialogForm(event.target);
      syncProviderFields();
    }
  }, true);

  for (const dialog of document.querySelectorAll("dialog[open]")) {
    if (typeof dialog.showModal === "function" && !dialog.hasAttribute("data-shown")) {
      dialog.setAttribute("data-shown", "");
      try { dialog.showModal(); } catch { /* already open */ }
    }
  }

  const syncProviderFields = () => {
    const providerApp = document.querySelector("#provider-form [name=app]");
    const authField = document.getElementById("claude-auth-field");
    const apiFormat = document.getElementById("provider-api-format");
    if (!providerApp) return;
    if (authField) authField.classList.toggle("hidden", providerApp.value !== "claude");
    if (!apiFormat) return;
    const options = [...apiFormat.options];
    for (const option of options) option.hidden = option.dataset.app !== providerApp.value;
    const current = apiFormat.selectedOptions[0];
    if (current && current.dataset.app === providerApp.value) return;
    const fallback = options.find((option) => option.dataset.app === providerApp.value && option.dataset.default)
      || options.find((option) => option.dataset.app === providerApp.value);
    if (fallback) apiFormat.value = fallback.value;
  };

  // A full document is fetched, but stable nodes, form values, focus, and the
  // animated canvas are kept in place. No hidden API or duplicate state model.
  const fetchPage = async (url) => {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 8000);
    try {
      const response = await fetch(url, { cache: "no-store", credentials: "same-origin", signal: controller.signal });
      if (!response.ok) throw new Error("panel unavailable");
      const page = new DOMParser().parseFromString(await response.text(), "text/html");
      if (!page.getElementById("fleet-canvas") || !page.getElementById("detail")) {
        throw new Error("panel could not be rendered");
      }
      return page;
    } finally {
      clearTimeout(timeout);
    }
  };

  const setConnection = (online) => {
    const label = document.getElementById("connection");
    if (!label) return;
    label.classList.toggle("online", online);
    label.classList.toggle("offline", !online);
    const text = online ? "本地在线" : "同步中断";
    if (label.lastChild?.nodeType === Node.TEXT_NODE) label.lastChild.textContent = text;
  };

  const replaceSection = (page, selector, force = false) => {
    const current = document.querySelector(selector);
    const next = page.querySelector(selector);
    if (!current || !next || current.outerHTML === next.outerHTML) return;
    const restoreToggleFocus = force && document.activeElement?.matches("a.toggle") && current.contains(document.activeElement);
    if (!force && current.contains(document.activeElement)) return;
    const selection = window.getSelection();
    if (!force && selection && !selection.isCollapsed && current.contains(selection.anchorNode)) return;
    const openDetails = new Set([...current.querySelectorAll("details[open][id]")].map((item) => item.id));
    const replacement = next.cloneNode(true);
    for (const id of openDetails) {
      const details = [...replacement.querySelectorAll("details[id]")].find((item) => item.id === id);
      if (details) details.open = true;
    }
    current.replaceWith(replacement);
    if (restoreToggleFocus) {
      const toggle = replacement.matches("a.toggle") ? replacement : replacement.querySelector("a.toggle");
      toggle?.focus({ preventScroll: true });
    }
  };

  const syncBoard = (page) => {
    const canvas = document.getElementById("fleet-canvas");
    const incoming = page.getElementById("fleet-canvas");
    const cloud = document.getElementById("agent-list");
    if (!canvas || !incoming || !cloud) return;
    const current = new Map([...canvas.querySelectorAll(".fleet-node")].map((node) => [node.dataset.agentId, node]));
    const seen = new Set();
    for (const next of incoming.querySelectorAll(".fleet-node")) {
      const id = next.dataset.agentId;
      seen.add(id);
      const node = current.get(id);
      if (!node) {
        cloud.append(next.cloneNode(true));
        continue;
      }
      // Retain the outer element (and its animation) unless the server says an
      // actual displayed field or status has changed.
      if (node.className !== next.className) node.className = next.className;
      for (const attr of ["data-tone", "data-agent-name", "title", "aria-current"]) {
        const value = next.getAttribute(attr);
        if (value === null && node.hasAttribute(attr)) node.removeAttribute(attr);
        else if (node.getAttribute(attr) !== value) node.setAttribute(attr, value);
      }
      if (node.innerHTML !== next.innerHTML) node.innerHTML = next.innerHTML;
    }
    for (const [id, node] of current) if (!seen.has(id)) node.remove();
    const empty = document.getElementById("board-empty");
    const nextEmpty = page.getElementById("board-empty");
    if (empty && nextEmpty) {
      empty.textContent = nextEmpty.textContent;
      empty.hidden = nextEmpty.hidden;
    }
    arrangeBoard();
  };

  const syncDetail = (page, forceEvents = false) => {
    const current = document.getElementById("detail");
    const next = page.getElementById("detail");
    if (!current || !next) return;
    const existingId = currentAgent();
    const incomingId = next.querySelector("#detail-id")?.textContent.trim() || "";
    if (existingId !== incomingId) {
      saveDraft();
      current.innerHTML = next.innerHTML;
      restoreDraft();
      return;
    }
    if (!existingId) return;
    for (const selector of [
      "#detail .detail-head", "#detail .detail-status-line", "#detail .spec-grid",
      "#detail-error-slot", "#detail .timeline-section",
    ]) replaceSection(page, selector);
    // Keep the event stream live even while its toggle has keyboard focus.
    // Explicitly toggling raw data also replaces the focused link and restores
    // focus to its new counterpart.
    replaceSection(page, "#detail .events-section a.toggle", forceEvents);
    replaceSection(page, "#events", forceEvents);
    // Never replace a user's textarea. Update only the server's enablement and
    // allowed actions, even if a draft is present or its field has focus.
    const field = document.querySelector("#message-form #message");
    const nextField = page.querySelector("#message-form #message");
    if (field && nextField) {
      field.disabled = nextField.disabled;
      field.placeholder = nextField.placeholder;
    }
    replaceSection(page, "#message-form .command-actions");
  };

  const syncClosedDialogs = (page) => {
    if (document.querySelector("dialog[open]")) return;
    for (const id of ["create-dialog", "provider-dialog"]) {
      const current = document.getElementById(id);
      const next = page.getElementById(id);
      if (!current || !next) continue;
      const form = current.querySelector("form");
      const nextForm = next.querySelector("form");
      if (form && nextForm && form.outerHTML !== nextForm.outerHTML) form.replaceWith(nextForm.cloneNode(true));
    }
    syncProviderFields();
  };

  const updatePage = (page, forceEvents = false) => {
    for (const id of ["hero-active", "last-sync", "metric-total", "metric-running", "metric-waiting", "metric-faulted", "metric-carriers", "metric-skills", "agent-count", "skill-count"]) {
      const current = document.getElementById(id);
      const next = page.getElementById(id);
      if (current && next && current.textContent !== next.textContent) current.textContent = next.textContent;
    }
    const navCount = document.querySelector('.main-nav a[href*="view=skills"] span');
    const nextNavCount = page.querySelector('.main-nav a[href*="view=skills"] span');
    if (navCount && nextNavCount) navCount.textContent = nextNavCount.textContent;
    syncBoard(page);
    syncDetail(page, forceEvents);
    for (const selector of ["#skill-manager", "#skills-list", "#provider-switch-status", "#provider-apps", "#panel-warnings"]) {
      replaceSection(page, selector);
    }
    syncClosedDialogs(page);
    syncDetailMode();
    renderedSearch = window.location.search;
  };

  const refresh = async () => {
    if (polling || navigating || document.visibilityState !== "visible" || window.__pihubSubmitting) return;
    polling = true;
    const number = ++requestNumber;
    const url = window.location.href;
    try {
      const page = await fetchPage(url);
      if (number !== requestNumber || url !== window.location.href || window.__pihubSubmitting) return;
      updatePage(page);
      setConnection(true);
    } catch {
      if (number === requestNumber) setConnection(false);
    } finally {
      polling = false;
    }
  };

  const navigate = async (url, push = true, forceEvents = false) => {
    const number = ++requestNumber;
    navigating = true;
    try {
      const page = await fetchPage(url);
      if (number !== requestNumber || window.__pihubSubmitting) return;
      if (push) history.pushState({}, "", url);
      updatePage(page, forceEvents);
      setConnection(true);
    } catch {
      if (number === requestNumber) setConnection(false);
    } finally {
      if (number === requestNumber) navigating = false;
    }
  };

  document.addEventListener("click", (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target) return;
    if (target.closest("#refresh") && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey && event.button === 0) {
      event.preventDefault();
      refresh();
      return;
    }
    const preference = target.closest("[data-preference]");
    if (preference) {
      setPreference(preference.dataset.preference, preference.dataset.value);
      return;
    }
    const scopeButton = target.closest("[data-scope]");
    if (scopeButton) {
      scope = scopeButton.dataset.scope === "all" ? "all" : "active";
      try { localStorage.setItem("pihub.scope", scope); } catch { /* optional */ }
      arrangeBoard();
      return;
    }
    const open = target.closest("[data-dialog]");
    if (open) {
      event.preventDefault();
      openDialog(open.dataset.dialog);
      return;
    }
    const closeDialogButton = target.closest("[data-close-dialog]");
    if (closeDialogButton) {
      event.preventDefault();
      closeDialog(closeDialogButton.closest("dialog"));
      return;
    }
    const expand = target.closest("[data-detail-expand]");
    const back = target.closest("[data-detail-back]");
    if (expand || back) {
      event.preventDefault();
      const url = new URL((expand || back).href);
      history.pushState({}, "", url);
      syncDetailMode();
      window.scrollTo({ top: 0, behavior: "auto" });
      return;
    }
    const close = target.closest("[data-detail-close]");
    const node = target.closest(".fleet-node");
    const eventToggle = target.closest("#detail .events-section a.toggle");
    if ((close || node || eventToggle) && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey && event.button === 0) {
      event.preventDefault();
      const url = close ? agentUrl("") : node ? agentUrl(node.dataset.agentId) : new URL(eventToggle.href);
      if (eventToggle) url.hash = window.location.hash;
      navigate(url, true, Boolean(eventToggle));
    }
  });

  document.addEventListener("change", (event) => {
    if (event.target?.matches("#provider-form [name=app]")) syncProviderFields();
  });
  document.addEventListener("submit", (event) => {
    if (event.target?.matches('form[method="post"]')) window.__pihubSubmitting = true;
  });
  window.addEventListener("popstate", () => {
    if (window.location.search === renderedSearch) syncDetailMode();
    else navigate(window.location.href, false, true);
  });
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") refresh();
  });

  // On narrow screens the canvas precedes its controls visually. Move the
  // original elements as well so keyboard order follows the same sequence;
  // desktop and the no-script document keep the server-rendered order.
  const responsiveLayout = () => {
    const stage = document.querySelector(".agent-stage");
    const toolbar = stage?.querySelector(".stage-toolbar");
    const controls = stage?.querySelector(".stage-controls");
    const filters = stage?.querySelector(".stage-filters");
    const summary = stage?.querySelector(".stage-summary");
    const scroll = stage?.querySelector(".fleet-scroll");
    if (!stage || !toolbar || !controls || !filters || !summary || !scroll) return;
    let media = null;
    try {
      if (typeof window.matchMedia === "function") media = window.matchMedia("(max-width: 600px)");
    } catch { /* use resize and innerWidth when media queries are unavailable */ }
    const reorder = () => {
      const mobile = media ? media.matches : window.innerWidth <= 600;
      if ((mobile && controls.parentElement === stage) ||
          (!mobile && controls.parentElement === toolbar)) return;
      const focused = stage.contains(document.activeElement) ? document.activeElement : null;
      const pageX = window.scrollX;
      const pageY = window.scrollY;
      const boardX = scroll.scrollLeft;
      const boardY = scroll.scrollTop;
      let selection = null;
      try {
        if (focused && typeof focused.selectionStart === "number") {
          selection = [focused.selectionStart, focused.selectionEnd, focused.selectionDirection];
        }
      } catch { /* not every input type supports a selection */ }
      if (mobile) stage.append(summary, scroll, controls, filters);
      else {
        toolbar.append(controls);
        stage.append(filters, summary, scroll);
      }
      if (focused && document.activeElement !== focused && focused.isConnected) {
        focused.focus({ preventScroll: true });
      }
      if (selection && typeof focused?.setSelectionRange === "function") {
        try { focused.setSelectionRange(...selection); } catch { /* unsupported input type */ }
      }
      if (scroll.scrollLeft !== boardX) scroll.scrollLeft = boardX;
      if (scroll.scrollTop !== boardY) scroll.scrollTop = boardY;
      if (window.scrollX !== pageX || window.scrollY !== pageY) window.scrollTo(pageX, pageY);
    };
    reorder();
    if (typeof media?.addEventListener === "function") media.addEventListener("change", reorder);
    else if (typeof media?.addListener === "function") media.addListener(reorder);
    else window.addEventListener("resize", reorder);
  };

  for (const name of Object.keys(preferenceValues)) setPreference(name, readPreference(name), false);
  document.querySelector(".stage-controls")?.removeAttribute("hidden");
  root.classList.add("js-ready");
  responsiveLayout();
  syncProviderFields();
  arrangeBoard();
  syncDetailMode();

  const toast = document.getElementById("toast");
  if (toast?.classList.contains("show")) setTimeout(() => toast.classList.remove("show"), 3200);
  setInterval(refresh, REFRESH_MS);
})();
