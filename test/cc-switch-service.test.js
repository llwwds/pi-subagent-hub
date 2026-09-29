import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { CcSwitchService } from "../src/cc-switch/service.js";
import { resolveHubPaths } from "../src/paths.js";

const homes = [];
afterEach(() => { while (homes.length) rmSync(homes.pop(), { recursive: true, force: true }); });

function fixture() {
  const home = mkdtempSync(join(realpathSync(tmpdir()), "pihub-ccswitch-test-"));
  homes.push(home);
  const paths = resolveHubPaths(home);
  mkdirSync(join(home, "toolchains", "cc-switch-cli"), { recursive: true });
  writeFileSync(paths.ccSwitchCliPath, `#!${process.execPath}
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
const args = process.argv.slice(2);
writeFileSync(join(process.env.CC_SWITCH_CONFIG_DIR, "probe.json"), JSON.stringify({
  args, home: process.env.HOME, configDir: process.env.CC_SWITCH_CONFIG_DIR,
  testHome: process.env.CC_SWITCH_TEST_HOME, claudeDir: process.env.CLAUDE_CONFIG_DIR,
  codexDir: process.env.CODEX_HOME, piDir: process.env.PI_CODING_AGENT_DIR,
  hostKey: process.env.OPENAI_API_KEY ?? null,
}));
if (args[0] === "--version") { console.log("cc-switch 5.10.5"); process.exit(0); }
const configPath = args[args.indexOf("--config-file") + 1];
const config = JSON.parse(readFileSync(configPath, "utf8"));
writeFileSync(join(process.env.CC_SWITCH_CONFIG_DIR, "auth-mode.json"), JSON.stringify({
  apiKey: Boolean(config.env?.ANTHROPIC_API_KEY), bearer: Boolean(config.env?.ANTHROPIC_AUTH_TOKEN),
}));
const secret = config.env?.ANTHROPIC_API_KEY ?? config.env?.ANTHROPIC_AUTH_TOKEN ?? config.auth?.OPENAI_API_KEY ?? config.apiKey;
console.log(secret);
console.error(secret);
process.exit(9);
`, { mode: 0o700 });
  return { home, paths, service: new CcSwitchService(paths) };
}

test("CC Switch 调用只使用私有环境；失败输出和命令参数不泄漏 API Key", async () => {
  const { paths, service } = fixture();
  const diagnosis = await service.diagnose();
  assert.equal(diagnosis.available, true);
  await assert.rejects(service.addProvider({
    app: "claude", id: "fake", name: "Fake", baseUrl: "https://example.invalid/v1",
    apiKey: "secret-test-key", modelList: ["fake-model"],
  }), (error) => {
    assert.equal(error.code, "CC_SWITCH_COMMAND_FAILED");
    assert.equal(JSON.stringify(error).includes("secret-test-key"), false);
    return true;
  });
  const probe = JSON.parse(readFileSync(join(paths.ccSwitchConfigDir, "probe.json"), "utf8"));
  assert.equal(probe.home, paths.ccSwitchHome);
  assert.equal(probe.configDir, paths.ccSwitchConfigDir);
  assert.equal(probe.testHome, paths.ccSwitchHome);
  assert.equal(probe.piDir, paths.ccSwitchPiDir);
  assert.equal(probe.hostKey, null);
  assert.equal(JSON.stringify(probe.args).includes("secret-test-key"), false);
  assert.equal(readdirSync(join(paths.home, "cc-switch", "tmp")).length, 0);
  assert.equal(statSync(join(paths.ccSwitchConfigDir, "probe.json")).mode & 0o777, 0o600);
  assert.equal(statSync(paths.ccSwitchConfigDir).mode & 0o777, 0o700);
  assert.deepEqual(JSON.parse(readFileSync(join(paths.ccSwitchConfigDir, "auth-mode.json"), "utf8")), { apiKey: true, bearer: false });
});

test("Claude 的 Bearer 网关认证必须显式选择", async () => {
  const { paths, service } = fixture();
  await assert.rejects(service.addProvider({
    app: "claude", id: "bearer", name: "Gateway", baseUrl: "https://example.invalid/v1",
    apiKey: "dummy-test-key", authMode: "bearer", modelList: ["fake-model"],
  }), { code: "CC_SWITCH_COMMAND_FAILED" });
  assert.deepEqual(JSON.parse(readFileSync(join(paths.ccSwitchConfigDir, "auth-mode.json"), "utf8")), { apiKey: false, bearer: true });
  await assert.rejects(service.addProvider({
    app: "claude", id: "invalid", name: "Invalid", baseUrl: "https://example.invalid/v1",
    apiKey: "dummy-test-key", authMode: "other", modelList: ["fake-model"],
  }), { code: "CC_SWITCH_UNSUPPORTED" });
});

test("CC Switch 私有路径中的符号链接会被拒绝", async () => {
  const { home, paths, service } = fixture();
  const outside = mkdtempSync(join(realpathSync(tmpdir()), "pihub-ccswitch-outside-"));
  homes.push(outside);
  mkdirSync(join(home, "cc-switch"));
  symlinkSync(outside, paths.ccSwitchConfigDir);
  await assert.rejects(service.listProviders("claude"), { code: "CC_SWITCH_PATH_UNSAFE" });
  assert.deepEqual(readdirSync(outside), []);
});

test("CC Switch 服务不写全局载体目录覆盖，并拒绝会覆盖 per-agent HOME 的旧设置", async () => {
  const { paths, service } = fixture();
  await service.diagnose();
  const settingsPath = join(paths.ccSwitchConfigDir, "settings.json");
  assert.equal(existsSync(settingsPath), false);
  writeFileSync(settingsPath, JSON.stringify({ codexConfigDir: join(paths.ccSwitchHome, ".codex") }));
  await assert.rejects(service.listProviders("codex"), { code: "CC_SWITCH_PATH_UNSAFE" });
});

test("只读列表保留模型与状态，但不会返回凭据；官方空配置不可运行", async () => {
  const { paths, service } = fixture();
  await service.diagnose();
  const db = new DatabaseSync(join(paths.ccSwitchConfigDir, "cc-switch.db"));
  db.exec("CREATE TABLE providers (id TEXT, app_type TEXT, name TEXT, settings_config TEXT, is_current INTEGER, sort_index INTEGER, created_at INTEGER)");
  db.prepare("INSERT INTO providers VALUES (?, ?, ?, ?, ?, ?, ?)").run(
    "configured", "claude", "Configured", JSON.stringify({ env: {
      ANTHROPIC_BASE_URL: "https://example.invalid/v1", ANTHROPIC_AUTH_TOKEN: "secret-test-key", ANTHROPIC_MODEL: "model-one",
    } }), 1, 0, 1,
  );
  db.prepare("INSERT INTO providers VALUES (?, ?, ?, ?, ?, ?, ?)").run(
    "claude-official", "claude", "Claude Official", JSON.stringify({ env: {} }), 0, 1, 2,
  );
  db.prepare("INSERT INTO providers VALUES (?, ?, ?, ?, ?, ?, ?)").run(
    "with-url-secret", "claude", "With URL Secret", JSON.stringify({ env: {
      ANTHROPIC_BASE_URL: "https://example.invalid/v1?api_key=secret-in-url",
      ANTHROPIC_AUTH_TOKEN: "secret-test-key", ANTHROPIC_MODEL: "model-one",
    } }), 0, 2, 3,
  );
  db.close();
  writeFileSync(join(paths.home, "cc-switch", "models-index.json"), JSON.stringify({ version: 1, providers: {
    claude: { configured: { modelList: ["model-one", "model-two"], defaultModel: "model-one", apiFormat: "anthropic" } },
  } }));
  const providers = await service.listProviders("claude");
  assert.equal(providers[0].configured, true);
  assert.equal(providers[0].ready, true);
  assert.equal(providers[0].apiFormat, "anthropic");
  assert.equal(providers[0].authMode, "bearer");
  assert.deepEqual(providers[0].modelList, ["model-one", "model-two"]);
  assert.equal(providers[1].ready, false);
  assert.equal(providers[2].baseUrl, "https://example.invalid/v1");
  assert.equal(JSON.stringify(providers).includes("secret-test-key"), false);
  assert.equal(JSON.stringify(providers).includes("secret-in-url"), false);
  await assert.rejects(service.switchProvider("claude", "claude-official"), { code: "CC_SWITCH_PROVIDER_NOT_READY" });
  const probe = JSON.parse(readFileSync(join(paths.ccSwitchConfigDir, "probe.json"), "utf8"));
  assert.deepEqual(probe.args, ["--version"]);
});
