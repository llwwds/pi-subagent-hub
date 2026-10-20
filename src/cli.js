import { execFileSync, spawn } from "node:child_process";
import { closeSync, existsSync, openSync, readFileSync, unlinkSync } from "node:fs";
import { platform } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { APP_VERSION } from "./constants.js";
import { HubClient } from "./client.js";
import { assertStateHomeOutsideApp, ensureHubLayout, hasBuiltPi, isStateHomeOutsideApp, resolveHubPaths } from "./paths.js";
import { isProcessAlive, parseModel, readJson, sleep } from "./utils.js";
import { ensurePiConfigFiles, inspectPiConfig } from "./config-files.js";

const cliDir = dirname(fileURLToPath(import.meta.url));

function help() {
  return `pi-subagent-hub ${APP_VERSION}

Usage:
  pihub daemon start|stop|status [--json]
  pihub spawn --name NAME --cwd DIR [--model PROVIDER/MODEL] [--thinking LEVEL] [--prompt TEXT] [--json]
  pihub spawn --manifest FILE|- [--json]
  pihub list|ps [--json]
  pihub inspect AGENT_ID [--json]
  pihub prompt|send AGENT_ID [TEXT|--stdin] [--json]
  pihub steer AGENT_ID [TEXT|--stdin] [--json]
  pihub follow-up AGENT_ID [TEXT|--stdin] [--json]
  pihub abort|stop|restart AGENT_ID [--json]
  pihub wait AGENT_ID... [--timeout SECONDS] [--json]
  pihub logs AGENT_ID [--after CURSOR] [--json]
  pihub panel [--no-open]
  pihub doctor [--json]
  pihub config init|validate|paths [--json]
  pihub carriers list [--json]
  pihub providers list [--json]
  pihub providers add --stdin [--json]  # JSON with app, URL, models and API key
  pihub providers switch --app pi|codex|claude --id ID [--json]
  pihub skills list|import --id ID --source DIR [--label NAME] [--json]
  pihub skills manager status|connect|sync|disconnect [--cli-path FILE] [--preset ID,ID] [--ids ID,ID] [--tags TAG,TAG] [--json]
  pihub prompts list [--json]
  pihub agents create --carrier pi|codex|claude --name NAME --cwd DIR [--skills all|none|ID,ID] [--principles all|none|ID,ID] [--system-prompt TEXT|--system-prompt-file FILE|--system-prompt-stdin] [--json]
  pihub agents create --manifest FILE|- [--json]
  pihub agents list|inspect AGENT_ID [--json]
  pihub agents prompt AGENT_ID [TEXT|--stdin] [--json]
  pihub agents wait AGENT_ID... [--timeout SECONDS] [--json]
  pihub agents logs AGENT_ID [--after CURSOR] [--json]
  pihub agents abort|stop|restart AGENT_ID [--json]

Machine interface:
  --json keeps stdout as one stable JSON envelope. Diagnostics go to stderr.
  Long prompts should use --stdin to avoid shell quoting and command-history exposure.
  System prompt text is never executed: it is passed as one argv element or read from
  a file/stdin. Long multi-line text should use --system-prompt-file or
  --system-prompt-stdin for the same reason.
`;
}

function parseArgs(args) {
  const options = {};
  const positional = [];
  for (let index = 0; index < args.length; index += 1) {
    const value = args[index];
    if (!value.startsWith("--")) {
      positional.push(value);
      continue;
    }
    const key = value.slice(2);
    if (new Set(["json", "stdin", "no-open", "all", "live", "force", "purge-snapshot", "system-prompt-stdin"]).has(key)) options[key] = true;
    else {
      if (index + 1 >= args.length) throw new Error(`missing value for ${value}`);
      options[key] = args[index + 1];
      index += 1;
    }
  }
  return { options, positional };
}

function output(data, jsonMode = false) {
  if (jsonMode) {
    process.stdout.write(`${JSON.stringify({ ok: true, data, error: null })}\n`);
    return;
  }
  if (typeof data === "string") process.stdout.write(`${data}\n`);
  else process.stdout.write(`${JSON.stringify(data, null, 2)}\n`);
}

function readStdin() {
  return readFileSync(0, "utf8").replace(/\n$/, "");
}

async function daemonStatus(paths) {
  const metadata = readJson(paths.daemonPath);
  if (!metadata?.pid || !isProcessAlive(metadata.pid)) return { running: false, metadata: metadata || null };
  try {
    const health = await new HubClient(paths).health();
    return { running: true, metadata, health };
  } catch (error) {
    return { running: false, metadata, error: error.message };
  }
}

async function startDaemon(paths) {
  assertStateHomeOutsideApp(paths);
  const current = await daemonStatus(paths);
  if (current.running) return { started: false, ...current };
  if (existsSync(paths.daemonPath)) unlinkSync(paths.daemonPath);
  ensureHubLayout(paths);
  const stdoutFd = openSync(paths.daemonStdoutPath, "a", 0o600);
  const stderrFd = openSync(paths.daemonStderrPath, "a", 0o600);
  const daemonEntry = join(cliDir, "daemon-entry.js");
  const child = spawn(process.execPath, [daemonEntry, "--home", paths.home], {
    detached: true,
    stdio: ["ignore", stdoutFd, stderrFd],
    env: { ...process.env, PIHUB_HOME: paths.home },
  });
  child.unref();
  closeSync(stdoutFd);
  closeSync(stderrFd);
  for (let attempt = 0; attempt < 100; attempt += 1) {
    await sleep(100);
    const status = await daemonStatus(paths);
    if (status.running) return { started: true, ...status };
    if (!isProcessAlive(child.pid)) break;
  }
  const stderrTail = existsSync(paths.daemonStderrPath)
    ? readFileSync(paths.daemonStderrPath, "utf8").split("\n").slice(-20).join("\n")
    : "";
  throw new Error(`daemon failed to start${stderrTail ? `\n${stderrTail}` : ""}`);
}

function normalizeCreateSpec(options) {
  const parsed = parseModel(options.model);
  return {
    agentId: options.id,
    name: options.name,
    cwd: options.cwd || process.cwd(),
    provider: options.provider || parsed.provider,
    model: parsed.model,
    thinking: options.thinking,
    workspaceMode: options["workspace-mode"] || "shared",
    prompt: options.prompt,
  };
}

function normalizeSkillPolicy(value) {
  if (!value || value === "all") return { mode: "all" };
  return { mode: "only", ids: value === "none" ? [] : value.split(",").map((id) => id.trim()) };
}

function readSystemPromptText(options) {
  const sources = [
    ["--system-prompt", options["system-prompt"]],
    ["--system-prompt-file", options["system-prompt-file"]],
    ["--system-prompt-stdin", options["system-prompt-stdin"]],
  ].filter(([, value]) => value !== undefined && value !== false);
  if (sources.length > 1) throw new Error("use only one of --system-prompt, --system-prompt-file, or --system-prompt-stdin");
  if (sources.length === 0) return undefined;
  const [flag, value] = sources[0];
  if (flag === "--system-prompt") return String(value);
  if (flag === "--system-prompt-stdin") {
    if (process.stdin.isTTY) throw new Error("--system-prompt-stdin requires piped input");
    return readStdin();
  }
  return readFileSync(value, "utf8");
}

function normalizePromptConfig(options) {
  const customText = readSystemPromptText(options);
  const principles = options.principles;
  if (customText === undefined && principles === undefined) return undefined;
  const config = {};
  if (principles !== undefined && principles !== "all") {
    config.principleIds = principles === "none" ? [] : String(principles).split(",").map((id) => id.trim()).filter(Boolean);
  }
  if (customText !== undefined) config.customText = customText;
  return config;
}

function normalizePlatformSpec(options) {
  return {
    ...normalizeCreateSpec(options),
    carrier: options.carrier || "pi",
    skillPolicy: normalizeSkillPolicy(options.skills),
    promptConfig: normalizePromptConfig(options),
  };
}

function manifestPromptConfig(manifest, item) {
  const base = manifest.promptConfig && typeof manifest.promptConfig === "object" ? manifest.promptConfig : {};
  if (item.promptConfig && typeof item.promptConfig === "object") return { ...base, ...item.promptConfig };
  if (item.systemPrompt !== undefined) return { ...base, customText: item.systemPrompt };
  if (item.principles !== undefined) return { ...base, principleIds: item.principles };
  return Object.keys(base).length > 0 ? base : undefined;
}

async function createPlatformFromManifest(client, path) {
  const raw = path === "-" ? readStdin() : readFileSync(path, "utf8");
  const manifest = JSON.parse(raw);
  if (manifest.schemaVersion !== 2 || !Array.isArray(manifest.agents)) {
    throw new Error("platform manifest must have schemaVersion 2 and an agents array");
  }
  const results = [];
  for (const item of manifest.agents) {
    try {
      const parsedModel = typeof item.model === "object" ? item.model : parseModel(item.model);
      const created = await client.request("POST", "/v2/agents", {
        agentId: item.agentId,
        name: item.name,
        cwd: item.cwd,
        workspaceMode: item.workspaceMode || "shared",
        provider: parsedModel?.provider || item.provider,
        model: parsedModel?.id || parsedModel?.model || null,
        thinking: parsedModel?.thinking || item.thinking,
        prompt: item.prompt,
        carrier: item.carrier || "pi",
        skillPolicy: item.skillPolicy || { mode: "all" },
        promptConfig: manifestPromptConfig(manifest, item),
      });
      results.push({ ok: true, agentId: created.id, agent: created });
    } catch (error) {
      results.push({ ok: false, agentId: item.agentId || null, error: { code: error.code || "create_failed", message: error.message } });
    }
  }
  return { groupName: manifest.groupName || null, results };
}

async function createFromManifest(client, path) {
  const raw = path === "-" ? readStdin() : readFileSync(path, "utf8");
  const manifest = JSON.parse(raw);
  if (manifest.schemaVersion !== 1 || !Array.isArray(manifest.agents)) {
    throw new Error("manifest must have schemaVersion 1 and an agents array");
  }
  const results = [];
  for (const item of manifest.agents) {
    try {
      const model = typeof item.model === "object" ? item.model : parseModel(item.model);
      const created = await client.request("POST", "/v1/agents", {
        agentId: item.agentId,
        name: item.name,
        cwd: item.cwd,
        workspaceMode: item.workspaceMode || "shared",
        provider: model?.provider || item.provider,
        model: model?.id || model?.model || null,
        thinking: model?.thinking || item.thinking,
        prompt: item.prompt,
        promptConfig: item.promptConfig,
      });
      results.push({ ok: true, agentId: created.id, agent: created });
    } catch (error) {
      results.push({ ok: false, agentId: item.agentId || null, error: { code: error.code || "create_failed", message: error.message } });
    }
  }
  return { groupName: manifest.groupName || null, results };
}

async function waitForAgents(client, ids, timeoutSeconds, apiVersion = "v1") {
  const deadline = Date.now() + timeoutSeconds * 1000;
  const settled = new Set(["idle", "stopped", "crashed", "failed", "waiting_input"]);
  while (Date.now() < deadline) {
    const agents = await Promise.all(ids.map((id) => client.request("GET", `/${apiVersion}/agents/${encodeURIComponent(id)}`)));
    if (agents.every((agent) => settled.has(agent.status))) return agents;
    await sleep(250);
  }
  const error = new Error(`timed out waiting for agents after ${timeoutSeconds}s`);
  error.exitCode = 4;
  throw error;
}

async function runDoctor(paths) {
  const checks = [];
  const add = (name, ok, detail) => checks.push({ name, ok, detail });
  add("node", Number(process.versions.node.split(".")[0]) >= 22, process.version);
  const lock = readJson(paths.piLockPath);
  add("pi-lock", Boolean(lock?.tag && lock?.commit), lock || "missing");
  add("pi-cli", hasBuiltPi(paths), paths.piCliPath);
  if (existsSync(paths.piCliPath)) {
    try {
      const version = execFileSync(process.execPath, [paths.piCliPath, "--version"], { encoding: "utf8", timeout: 15_000 }).trim();
      add("pi-version", version.includes(lock?.tag?.replace(/^v/, "")), version);
    } catch (error) {
      add("pi-version", false, error.message);
    }
  }
  const vendorGit = join(paths.appRoot, "vendor", "pi", ".git");
  if (existsSync(vendorGit) && lock?.commit) {
    try {
      const head = execFileSync("git", ["-C", join(paths.appRoot, "vendor", "pi"), "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
      add("pi-commit", head === lock.commit, head);
    } catch (error) {
      add("pi-commit", false, error.message);
    }
  } else add("pi-commit", Boolean(lock?.commit), "deployment uses lock file; source .git not present");
  add("deployment-boundary", isStateHomeOutsideApp(paths), `app=${paths.appRoot}; state=${paths.home}`);
  try {
    add("ai-api-config", true, inspectPiConfig(paths));
  } catch (error) {
    add("ai-api-config", false, error.message);
  }
  const daemon = await daemonStatus(paths);
  add("daemon", daemon.running, daemon.running ? daemon.health : "stopped");
  return { ok: checks.filter((check) => check.name !== "daemon").every((check) => check.ok), checks };
}

export async function main(argv) {
  if (argv.length === 0 || argv.includes("--help") || argv.includes("-h")) {
    process.stdout.write(help());
    return;
  }
  if (argv[0] === "--version" || argv[0] === "-v") return output(APP_VERSION);

  const command = argv[0];
  const subcommand = argv[1];
  const parsed = parseArgs(argv.slice(new Set(["daemon", "config", "carriers", "providers", "skills", "agents", "prompts"]).has(command) ? 2 : 1));
  const paths = resolveHubPaths(process.env.PIHUB_HOME);
  const jsonMode = Boolean(parsed.options.json);

  if (command === "daemon") {
    if (subcommand === "start") return output(await startDaemon(paths), jsonMode);
    if (subcommand === "status") return output(await daemonStatus(paths), jsonMode);
    if (subcommand === "stop") {
      const status = await daemonStatus(paths);
      if (!status.running) return output({ stopped: false, reason: "not running" }, jsonMode);
      const result = await new HubClient(paths).request("POST", "/v1/daemon/stop", {});
      return output(result, jsonMode);
    }
    throw new Error("daemon requires start, stop, or status");
  }
  if (command === "doctor") return output(await runDoctor(paths), jsonMode);
  if (command === "config" && subcommand === "init") {
    assertStateHomeOutsideApp(paths);
    return output(ensurePiConfigFiles(paths), jsonMode);
  }
  if (command === "config" && subcommand === "validate") return output(inspectPiConfig(paths), jsonMode);
  if (command === "config" && subcommand === "paths") return output(paths, jsonMode);
  if (command === "config") throw new Error("config requires init, validate, or paths");
  if (command === "panel") {
    const status = await startDaemon(paths);
    const url = `http://127.0.0.1:${status.metadata.port}/`;
    if (!parsed.options["no-open"] && platform() === "darwin") spawn("open", [url], { detached: true, stdio: "ignore" }).unref();
    return output({ url, opened: !parsed.options["no-open"] && platform() === "darwin" }, jsonMode);
  }

  const client = new HubClient(paths);
  if (command === "carriers" && subcommand === "list") {
    return output(await client.request("GET", "/v2/carriers"), jsonMode);
  }
  if (command === "providers" && subcommand === "list") {
    return output(await client.request("GET", "/v2/providers"), jsonMode);
  }
  if (command === "providers" && subcommand === "add") {
    if (!parsed.options.stdin) throw new Error("providers add requires --stdin so credentials do not enter command history");
    return output(await client.request("POST", "/v2/providers", JSON.parse(readStdin())), jsonMode);
  }
  if (command === "providers" && subcommand === "switch") {
    if (!parsed.options.app || !parsed.options.id) throw new Error("providers switch requires --app and --id");
    return output(await client.request("POST", `/v2/providers/${encodeURIComponent(parsed.options.app)}/${encodeURIComponent(parsed.options.id)}/switch`, {}), jsonMode);
  }
  if (command === "skills" && subcommand === "list") {
    return output(await client.request("GET", "/v2/skills"), jsonMode);
  }
  if (command === "skills" && subcommand === "manager") {
    const action = parsed.positional[0] || "status";
    if (action === "status") return output(await client.request("GET", `/v2/skills/manager?live=${parsed.options.live ? "true" : "false"}`), jsonMode);
    if (!new Set(["connect", "sync", "disconnect"]).has(action)) throw new Error("skills manager requires status, connect, sync, or disconnect");
    const body = {};
    for (const [flag, key] of [["cli-path", "cliPath"], ["manager-base-dir", "managerBaseDir"],
      ["manager-skills-dir", "managerSkillsDir"], ["agent-key", "agentKey"]]) {
      if (parsed.options[flag] !== undefined) body[key] = parsed.options[flag];
    }
    if (parsed.options.source !== undefined) body.source = parsed.options.source;
    if (parsed.options.preset) body.selection = { ...(body.selection || {}), presetIds: String(parsed.options.preset).split(",").map((id) => id.trim()).filter(Boolean) };
    if (parsed.options.ids) body.selection = { ...(body.selection || {}), ids: String(parsed.options.ids).split(",").map((id) => id.trim()).filter(Boolean) };
    if (parsed.options.tags) body.selection = { ...(body.selection || {}), tags: String(parsed.options.tags).split(",").map((id) => id.trim()).filter(Boolean) };
    if (action === "sync" && parsed.options.force) body.force = true;
    if (action === "disconnect" && parsed.options["purge-snapshot"]) body.purgeSnapshot = true;
    return output(await client.request("POST", `/v2/skills/manager/${action}`, body), jsonMode);
  }
  if (command === "prompts" && subcommand === "list") {
    return output(await client.request("GET", "/v2/prompts"), jsonMode);
  }
  if (command === "skills" && subcommand === "import") {
    if (!parsed.options.id || !parsed.options.source) throw new Error("skills import requires --id and --source");
    return output(await client.request("POST", "/v2/skills/import", {
      id: parsed.options.id,
      sourcePath: parsed.options.source,
      ...(parsed.options.label ? { sourceLabel: parsed.options.label } : {}),
    }), jsonMode);
  }
  if (command === "agents" && subcommand === "create") {
    if (parsed.options.manifest) return output(await createPlatformFromManifest(client, parsed.options.manifest), jsonMode);
    return output(await client.request("POST", "/v2/agents", normalizePlatformSpec(parsed.options)), jsonMode);
  }
  if (command === "agents" && subcommand === "list") {
    const snapshot = await client.request("GET", "/v2/snapshot");
    return output(snapshot.agents, jsonMode);
  }
  if (command === "agents" && subcommand === "inspect") {
    const id = parsed.positional[0];
    if (!id) throw new Error("agents inspect requires AGENT_ID");
    return output(await client.request("GET", `/v2/agents/${encodeURIComponent(id)}`), jsonMode);
  }
  if (command === "agents" && subcommand === "prompt") {
    const id = parsed.positional.shift();
    if (!id) throw new Error("agents prompt requires AGENT_ID");
    const message = parsed.options.stdin ? readStdin() : parsed.positional.join(" ");
    return output(await client.request("POST", `/v2/agents/${encodeURIComponent(id)}/prompt`, { message }), jsonMode);
  }
  if (command === "agents" && subcommand === "wait") {
    if (!parsed.positional.length) throw new Error("agents wait requires AGENT_ID");
    const timeout = Number(parsed.options.timeout || 120);
    return output(await waitForAgents(client, parsed.positional, timeout, "v2"), jsonMode);
  }
  if (command === "agents" && subcommand === "logs") {
    const id = parsed.positional[0];
    if (!id) throw new Error("agents logs requires AGENT_ID");
    const after = parsed.options.after || 0;
    return output(await client.request("GET", `/v2/agents/${encodeURIComponent(id)}/events?after=${after}`), jsonMode);
  }
  if (command === "agents" && new Set(["abort", "stop", "restart"]).has(subcommand)) {
    const id = parsed.positional[0];
    if (!id) throw new Error(`agents ${subcommand} requires AGENT_ID`);
    return output(await client.request("POST", `/v2/agents/${encodeURIComponent(id)}/${subcommand}`, {}), jsonMode);
  }
  if (command === "spawn") {
    if (parsed.options.manifest) return output(await createFromManifest(client, parsed.options.manifest), jsonMode);
    return output(await client.request("POST", "/v1/agents", normalizeCreateSpec(parsed.options)), jsonMode);
  }
  if (command === "list" || command === "ps") return output(await client.request("GET", `/v1/agents?all=${parsed.options.all ? "true" : "false"}`), jsonMode);
  if (command === "inspect") return output(await client.request("GET", `/v1/agents/${encodeURIComponent(parsed.positional[0])}`), jsonMode);
  if (new Set(["prompt", "send", "steer", "follow-up"]).has(command)) {
    const id = parsed.positional.shift();
    const message = parsed.options.stdin ? readStdin() : parsed.positional.join(" ");
    const action = command === "send" ? "prompt" : command;
    return output(await client.request("POST", `/v1/agents/${encodeURIComponent(id)}/${action}`, { message }), jsonMode);
  }
  if (new Set(["abort", "stop", "restart"]).has(command)) {
    const id = parsed.positional[0];
    return output(await client.request("POST", `/v1/agents/${encodeURIComponent(id)}/${command}`, {}), jsonMode);
  }
  if (command === "wait") {
    const timeout = Number(parsed.options.timeout || 120);
    return output(await waitForAgents(client, parsed.positional, timeout), jsonMode);
  }
  if (command === "logs") {
    const id = parsed.positional[0];
    const after = parsed.options.after || 0;
    return output(await client.request("GET", `/v1/agents/${encodeURIComponent(id)}/events?after=${after}`), jsonMode);
  }
  throw new Error(`unknown command: ${command}`);
}
