import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ensurePiConfigFiles, inspectPiConfig } from "../src/config-files.js";
import { ensureHubLayout, resolveHubPaths } from "../src/paths.js";

function temporaryPaths() {
  const root = mkdtempSync(join(tmpdir(), "pihub-config-"));
  const paths = resolveHubPaths(root);
  ensureHubLayout(paths);
  return { root, paths };
}

test("config init creates private shared Pi configuration files", () => {
  const { root, paths } = temporaryPaths();
  const result = ensurePiConfigFiles(paths);
  assert.equal(result.modelsMode, "600");
  assert.equal(result.authMode, "600");
  assert.deepEqual(result.providers, []);
  assert.deepEqual(result.authProviders, []);
  rmSync(root, { recursive: true, force: true });
});

test("config validation returns metadata without exposing credential values", () => {
  const { root, paths } = temporaryPaths();
  ensurePiConfigFiles(paths);
  const secret = "do-not-return-this-secret";
  writeFileSync(paths.modelsPath, JSON.stringify({
    providers: {
      gateway: {
        baseUrl: "https://api.example.com/v1",
        api: "openai-responses",
        apiKey: secret,
        headers: { "x-secret": secret },
        models: [{ id: "model-a" }],
      },
    },
  }), { mode: 0o600 });
  const result = inspectPiConfig(paths);
  assert.equal(result.providers[0].hasCredentialReference, true);
  assert.equal(result.providers[0].hasCustomHeaders, true);
  assert.deepEqual(result.providers[0].models, ["model-a"]);
  assert.equal(JSON.stringify(result).includes(secret), false);
  rmSync(root, { recursive: true, force: true });
});

test("config validation fails closed on invalid model entries", () => {
  const { root, paths } = temporaryPaths();
  ensurePiConfigFiles(paths);
  writeFileSync(paths.modelsPath, JSON.stringify({ providers: { invalid: { models: [{}] } } }), { mode: 0o600 });
  assert.throws(() => inspectPiConfig(paths), /requires a non-empty id/);
  rmSync(root, { recursive: true, force: true });
});

test("config init repairs overly broad permissions", () => {
  const { root, paths } = temporaryPaths();
  ensurePiConfigFiles(paths);
  chmodSync(paths.modelsPath, 0o644);
  assert.throws(() => inspectPiConfig(paths), /must use mode 600/);
  assert.equal(ensurePiConfigFiles(paths).modelsMode, "600");
  rmSync(root, { recursive: true, force: true });
});
