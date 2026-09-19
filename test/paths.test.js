import test from "node:test";
import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import { assertStateHomeOutsideApp, isStateHomeOutsideApp, resolveHubPaths } from "../src/paths.js";

test("runtime state must stay outside the source tree", () => {
  const appRoot = resolve("/tmp/pihub-source");
  const unsafe = resolveHubPaths(join(appRoot, "state"), appRoot);
  const safe = resolveHubPaths("/tmp/pihub-state", appRoot);
  assert.equal(isStateHomeOutsideApp(unsafe), false);
  assert.equal(isStateHomeOutsideApp(safe), true);
  assert.throws(() => assertStateHomeOutsideApp(unsafe), /outside the source tree/);
});
