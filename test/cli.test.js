import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

test("--json keeps command errors in one machine-readable envelope", () => {
  const result = spawnSync(process.execPath, [resolve("bin/pihub.js"), "not-a-command", "--json"], {
    cwd: resolve("."),
    encoding: "utf8",
  });
  assert.equal(result.status, 1);
  assert.equal(result.stderr, "");
  const envelope = JSON.parse(result.stdout);
  assert.deepEqual(envelope, {
    ok: false,
    data: null,
    error: {
      code: "command_failed",
      message: "unknown command: not-a-command",
      retryable: false,
      details: null,
    },
  });
});
