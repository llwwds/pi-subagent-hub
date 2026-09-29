import { createReadStream, readFileSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join } from "node:path";
import { APP_VERSION, API_VERSION } from "./constants.js";
import { publicAgent, publicAgentV2 } from "./utils.js";

const MIME = {
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".svg": "image/svg+xml",
};

function json(response, status, body) {
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
  });
  response.end(`${JSON.stringify(body)}\n`);
}

function ok(response, data, status = 200) {
  json(response, status, { ok: true, data, error: null });
}

function fail(response, status, code, message, details = null) {
  json(response, status, { ok: false, data: null, error: { code, message, retryable: status >= 500, details } });
}

async function readBody(request) {
  let size = 0;
  const chunks = [];
  for await (const chunk of request) {
    size += chunk.length;
    if (size > 1024 * 1024) throw new Error("request body exceeds 1 MiB");
    chunks.push(chunk);
  }
  if (chunks.length === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

function serveFile(response, path, extraHeaders = {}) {
  const stat = statSync(path);
  response.writeHead(200, {
    "content-type": MIME[extname(path)] || "application/octet-stream",
    "content-length": stat.size,
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
    "content-security-policy": "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; base-uri 'none'; frame-ancestors 'none'",
    ...extraHeaders,
  });
  createReadStream(path).pipe(response);
}

export function createHubServer({ manager, store, paths, token, onStop }) {
  const dashboardDir = join(paths.appRoot, "src", "dashboard");
  return createServer(async (request, response) => {
    const url = new URL(request.url, "http://127.0.0.1");
    const pathname = url.pathname;

    if (request.method === "GET" && pathname === "/") {
      const template = readFileSync(join(dashboardDir, "index.html"), "utf8");
      const html = template.replace("__PIHUB_TOKEN__", token);
      response.writeHead(200, {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "no-store",
        "content-security-policy": "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; base-uri 'none'; frame-ancestors 'none'",
        "x-content-type-options": "nosniff",
      });
      response.end(html);
      return;
    }
    if (request.method === "GET" && pathname === "/app.js") return serveFile(response, join(dashboardDir, "app.js"));
    if (request.method === "GET" && pathname === "/styles.css") return serveFile(response, join(dashboardDir, "styles.css"));
    if (request.method === "GET" && pathname === "/v1/health") {
      return ok(response, { name: "pi-subagent-hub", version: APP_VERSION, apiVersion: API_VERSION, pid: process.pid });
    }

    const authorization = request.headers.authorization;
    if (authorization !== `Bearer ${token}`) return fail(response, 401, "unauthorized", "valid local control token required");

    try {
      if (request.method === "GET" && pathname === "/v2/snapshot") {
        return ok(response, {
          version: APP_VERSION,
          profile: { name: manager.profile.name, digest: manager.profile.digest, tools: manager.profile.tools },
          agents: manager.listAgents().map(publicAgentV2),
          activeCount: manager.activeCount(),
          managedCount: manager.managedCount(),
          eventCursor: store.latestCursor(),
        });
      }
      if (request.method === "GET" && pathname === "/v2/events") {
        return ok(response, manager.listEvents({ after: url.searchParams.get("after"), limit: url.searchParams.get("limit") }));
      }
      if (request.method === "GET" && pathname === "/v2/carriers") {
        return ok(response, await manager.listCarriers());
      }
      if (request.method === "GET" && pathname === "/v2/providers") {
        return ok(response, await manager.listProviders());
      }
      if (request.method === "POST" && pathname === "/v2/providers") {
        return ok(response, await manager.addProvider(await readBody(request)), 201);
      }
      const providerSwitchMatch = pathname.match(/^\/v2\/providers\/([^/]+)\/([^/]+)\/switch$/);
      if (request.method === "POST" && providerSwitchMatch) {
        return ok(response, await manager.switchProvider(
          decodeURIComponent(providerSwitchMatch[1]), decodeURIComponent(providerSwitchMatch[2]),
        ));
      }
      if (request.method === "GET" && pathname === "/v2/skills") {
        return ok(response, manager.listSkills());
      }
      if (request.method === "POST" && pathname === "/v2/skills/import") {
        const body = await readBody(request);
        return ok(response, await manager.importSkill(body), 201);
      }
      if (request.method === "POST" && pathname === "/v2/agents") {
        const body = await readBody(request);
        return ok(response, publicAgentV2(await manager.createPlatformAgent(body)), 201);
      }
      const platformAgentMatch = pathname.match(/^\/v2\/agents\/([^/]+)(?:\/(.*))?$/);
      if (platformAgentMatch) {
        const id = decodeURIComponent(platformAgentMatch[1]);
        const action = platformAgentMatch[2] || "";
        const agent = manager.getAgent(id);
        if (!agent) return fail(response, 404, "agent_not_found", `unknown agent: ${id}`);
        if (request.method === "GET" && action === "") return ok(response, publicAgentV2(agent));
        if (request.method === "GET" && action === "events") {
          return ok(response, manager.listEvents({ agentId: id, after: url.searchParams.get("after"), limit: url.searchParams.get("limit") }));
        }
        if (request.method === "POST" && new Set(["prompt", "steer", "follow-up"]).has(action)) {
          const body = await readBody(request);
          const kind = action === "follow-up" ? "follow_up" : action;
          return ok(response, await manager.send(id, kind, body.message));
        }
        if (request.method === "POST" && action === "abort") return ok(response, await manager.request(id, "abort"));
        if (request.method === "POST" && action === "stop") return ok(response, publicAgentV2(await manager.stopAgent(id)));
        if (request.method === "POST" && action === "restart") return ok(response, publicAgentV2(await manager.restartAgent(id)));
      }
      if (request.method === "GET" && pathname === "/v1/snapshot") {
        return ok(response, {
          version: APP_VERSION,
          profile: { name: manager.profile.name, digest: manager.profile.digest, tools: manager.profile.tools },
          agents: manager.listAgents({ legacyOnly: true }).map(publicAgent),
          eventCursor: store.latestCursor({ legacyOnly: true }),
        });
      }
      if (request.method === "GET" && pathname === "/v1/agents") {
        return ok(response, manager.listAgents({ includeStopped: url.searchParams.get("all") !== "false", legacyOnly: true }).map(publicAgent));
      }
      if (request.method === "POST" && pathname === "/v1/agents") {
        const body = await readBody(request);
        const created = await manager.createAgent(body);
        return ok(response, publicAgent(created), 201);
      }
      if (request.method === "GET" && pathname === "/v1/events") {
        return ok(response, manager.listEvents({ after: url.searchParams.get("after"), limit: url.searchParams.get("limit"), legacyOnly: true }));
      }

      const agentMatch = pathname.match(/^\/v1\/agents\/([^/]+)(?:\/(.*))?$/);
      if (agentMatch) {
        const id = decodeURIComponent(agentMatch[1]);
        const action = agentMatch[2] || "";
        const agent = manager.getAgent(id);
        if (!agent || agent.skill_authorization) return fail(response, 404, "agent_not_found", `unknown agent: ${id}`);
        if (request.method === "GET" && action === "") return ok(response, publicAgent(agent));
        if (request.method === "GET" && action === "events") {
          return ok(response, manager.listEvents({ agentId: id, after: url.searchParams.get("after"), limit: url.searchParams.get("limit") }));
        }
        if (request.method === "POST" && new Set(["prompt", "steer", "follow-up"]).has(action)) {
          const body = await readBody(request);
          const kind = action === "follow-up" ? "follow_up" : action;
          return ok(response, await manager.send(id, kind, body.message));
        }
        if (request.method === "POST" && action === "abort") return ok(response, await manager.request(id, "abort"));
        if (request.method === "POST" && action === "stop") return ok(response, publicAgent(await manager.stopAgent(id)));
        if (request.method === "POST" && action === "restart") return ok(response, publicAgent(await manager.restartAgent(id)));
      }

      if (request.method === "POST" && pathname === "/v1/daemon/stop") {
        ok(response, { stopping: true });
        setImmediate(onStop);
        return;
      }
      return fail(response, 404, "not_found", "route not found");
    } catch (error) {
      return fail(response, 400, "request_failed", error.message);
    }
  });
}
