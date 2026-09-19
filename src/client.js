import { readJson } from "./utils.js";

export class HubClient {
  constructor(paths) {
    this.paths = paths;
  }

  connection() {
    const daemon = readJson(this.paths.daemonPath);
    const control = readJson(this.paths.controlPath);
    if (!daemon?.port || !control?.token) throw new Error("pi-subagent-hub daemon is not running");
    return { baseUrl: `http://127.0.0.1:${daemon.port}`, token: control.token, daemon };
  }

  async request(method, path, body = undefined) {
    const { baseUrl, token } = this.connection();
    const response = await fetch(`${baseUrl}${path}`, {
      method,
      headers: {
        authorization: `Bearer ${token}`,
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const payload = await response.json();
    if (!response.ok || !payload.ok) {
      const error = new Error(payload.error?.message || `Hub API failed with HTTP ${response.status}`);
      error.code = payload.error?.code || "api_error";
      throw error;
    }
    return payload.data;
  }

  async health() {
    const { baseUrl } = this.connection();
    const response = await fetch(`${baseUrl}/v1/health`);
    if (!response.ok) throw new Error(`Hub health check failed with HTTP ${response.status}`);
    return (await response.json()).data;
  }
}
