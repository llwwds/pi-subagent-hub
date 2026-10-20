// Generic browser glue only.
//
// All application behaviour lives in Rust: this file opens/closes dialogs,
// hides the toast after a moment, and reloads the server-rendered page on a
// fixed cadence. It contains no Hub business logic, no rendering and no data
// shaping; every view is produced by the SSR templates.
(() => {
  const REFRESH_MS = 2400;

  const openDialog = (id) => {
    const dialog = document.getElementById(id);
    if (!dialog) return;
    if (typeof dialog.showModal === "function") dialog.showModal();
    else dialog.setAttribute("open", "");
  };

  // Closing a dialog discards whatever the browser put in its form, so the
  // next open starts from the server-rendered defaults. Escape fires `close`,
  // and the no-`close()` fallback resets explicitly because
  // `removeAttribute("open")` dispatches no event. Nothing resets on open, so
  // submitted values and validation state survive while the dialog is used.
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

  for (const dialog of document.querySelectorAll("dialog")) {
    dialog.addEventListener("close", () => resetDialogForm(dialog));
  }

  // Buttons that open a dialog: <button data-dialog="create-dialog">.
  for (const button of document.querySelectorAll("[data-dialog]")) {
    button.addEventListener("click", (event) => {
      event.preventDefault();
      openDialog(button.dataset.dialog);
    });
  }

  // Any [data-close-dialog] element closes its enclosing dialog.
  for (const button of document.querySelectorAll("[data-close-dialog]")) {
    button.addEventListener("click", (event) => {
      event.preventDefault();
      closeDialog(button.closest("dialog"));
    });
  }

  // A dialog rendered with the `open` attribute by the server is promoted to a
  // modal dialog once scripting is available.
  for (const dialog of document.querySelectorAll("dialog[open]")) {
    if (typeof dialog.showModal === "function" && !dialog.hasAttribute("data-shown")) {
      dialog.setAttribute("data-shown", "");
      try { dialog.showModal(); } catch { /* already modal */ }
    }
  }

  // Provider form: the authentication selector only applies to Claude.
  const providerApp = document.querySelector("#provider-form [name=app]");
  const authField = document.getElementById("claude-auth-field");
  const syncAuthField = () => {
    if (!providerApp || !authField) return;
    authField.classList.toggle("hidden", providerApp.value !== "claude");
  };

  // The API protocol list is scoped per carrier. The server renders every
  // option with its `data-app` and marks that carrier's default, so this only
  // shows the matching subset and falls back to the carrier default when the
  // current choice belongs to another carrier.
  const apiFormat = document.getElementById("provider-api-format");
  const syncApiFormats = () => {
    if (!providerApp || !apiFormat) return;
    const app = providerApp.value;
    const options = [...apiFormat.options];
    for (const option of options) option.hidden = option.dataset.app !== app;
    const current = apiFormat.selectedOptions[0];
    if (current && current.dataset.app === app) return;
    const fallback = options.find((option) => option.dataset.app === app && option.dataset.default)
      || options.find((option) => option.dataset.app === app);
    if (fallback) apiFormat.value = fallback.value;
  };

  if (providerApp) {
    providerApp.addEventListener("change", () => {
      syncAuthField();
      syncApiFormats();
    });
  }
  syncAuthField();
  syncApiFormats();

  // Toast auto-dismiss.
  const toast = document.getElementById("toast");
  if (toast && toast.classList.contains("show")) {
    setTimeout(() => toast.classList.remove("show"), 3200);
  }

  // Auto refresh. Skipped while a dialog is open, while the user is typing in a
  // field, or when the tab is hidden, so form input is never discarded.
  setInterval(() => {
    if (document.visibilityState !== "visible") return;
    if (document.querySelector("dialog[open]")) return;
    const active = document.activeElement;
    if (active && ["INPUT", "TEXTAREA", "SELECT"].includes(active.tagName)) return;
    if (window.__pihubSubmitting) return;
    window.location.reload();
  }, REFRESH_MS);

  // Guard against a refresh racing a submit.
  for (const form of document.querySelectorAll("form[method=post]")) {
    form.addEventListener("submit", () => { window.__pihubSubmitting = true; });
  }
})();
