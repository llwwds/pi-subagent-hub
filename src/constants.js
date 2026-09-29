export const APP_NAME = "pi-subagent-hub";
export const CLI_NAME = "pihub";
export const APP_VERSION = "2.0.0.0";
export const RELEASE_CHANNEL = "development";
export const API_VERSION = 1;
export const DEFAULT_TOOLS = ["read", "bash", "edit", "write", "grep", "find", "ls"];
export const ACTIVE_STATES = new Set(["provisioning", "starting", "idle", "running", "waiting_input", "stopping"]);
export const TERMINAL_STATES = new Set(["stopped", "crashed", "failed"]);
