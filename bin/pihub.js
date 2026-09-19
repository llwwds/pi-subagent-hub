#!/usr/bin/env node
import { main } from "../src/cli.js";

main(process.argv.slice(2)).catch((error) => {
  if (process.argv.includes("--json")) {
    process.stdout.write(`${JSON.stringify({
      ok: false,
      data: null,
      error: {
        code: error.code || "command_failed",
        message: error.message,
        retryable: false,
        details: null,
      },
    })}\n`);
  } else {
    process.stderr.write(`pihub: ${error.message}\n`);
  }
  process.exitCode = error.exitCode || 1;
});
