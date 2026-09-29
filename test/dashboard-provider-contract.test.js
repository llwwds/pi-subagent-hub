import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

test("dashboard offers the CC Switch API formats accepted by each carrier", () => {
  const source = readFileSync(join(import.meta.dirname, "..", "src", "dashboard", "app.js"), "utf8");
  const match = /^const API_FORMATS = (\{[\s\S]*?^\});/m.exec(source);
  assert.ok(match, "dashboard API format choices should be declared");
  const formats = JSON.parse(JSON.stringify(runInNewContext(`(${match[1]})`)));

  assert.deepEqual(formats.pi.map(([id]) => id), ["openai-completions", "openai-responses", "anthropic-messages"]);
  assert.deepEqual(formats.codex.map(([id]) => id), ["responses"]);
  assert.deepEqual(formats.claude.map(([id]) => id), ["anthropic"]);
});

test("Claude provider form offers API key and bearer authentication", () => {
  const source = readFileSync(join(import.meta.dirname, "..", "src", "dashboard", "app.js"), "utf8");
  const html = readFileSync(join(import.meta.dirname, "..", "src", "dashboard", "index.html"), "utf8");
  const match = /^const CLAUDE_AUTH_MODES = (\[[^\n]+\]);/m.exec(source);
  assert.ok(match, "Claude auth modes should be declared");
  const modes = JSON.parse(JSON.stringify(runInNewContext(`(${match[1]})`)));
  assert.deepEqual(modes.map(([id]) => id), ["api-key", "bearer"]);
  assert.match(html, /<select id="claude-auth-mode" name="authMode"><\/select>/);
});
