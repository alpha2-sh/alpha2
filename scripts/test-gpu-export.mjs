#!/usr/bin/env node

// Verify GPU-resident export and in-flight cancellation with a release host. Skip unavailable
// hardware (recorded as the `gpu-resident` capability; ALPHA2_REQUIRE=gpu-resident makes it a
// failure). `--expect-unavailable` verifies a
// machine without the path: the host says why, `auto` falls back, `gpu-required` is refused before
// any output exists. The cases are in scripts/lib/gpu-export/.

import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { required } from "./lib/capabilities.mjs";
import { executable } from "./lib/cargo-target.mjs";
import { runAvailable } from "./lib/gpu-export/available.mjs";
import { createHarness } from "./lib/gpu-export/harness.mjs";
import { runUnavailable } from "./lib/gpu-export/unavailable.mjs";

const scratch = mkdtempSync(join(tmpdir(), "alpha2-gpu-export-"));
let harness = null;
let preconditionFailed = false;

try {
  harness = createHarness(parseArgs(process.argv.slice(2)), scratch);
  if (harness.options.expectUnavailable) runUnavailable(harness);
  else runAvailable(harness);
} catch (error) {
  // A thrown error here is a failed precondition, not a failed assertion; reported the same way.
  process.stdout.write(`FAIL ${error.message}\n`);
  preconditionFailed = true;
} finally {
  rmSync(scratch, { recursive: true, force: true });
}

process.exit(!preconditionFailed && (harness?.failures ?? 0) === 0 ? 0 : 1);

function parseArgs(argv) {
  const parsed = {
    host: resolve(
      process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "release"),
    ),
    repeats: 20,
    expectUnavailable: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--host") parsed.host = resolve(argv[(index += 1)]);
    else if (flag === "--repeats")
      parsed.repeats = Number.parseInt(argv[(index += 1)], 10);
    else if (flag === "--expect-unavailable") parsed.expectUnavailable = true;
    else throw new Error(`unknown argument ${flag}`);
  }
  if (parsed.expectUnavailable && required("gpu-resident")) {
    throw new Error(
      "--expect-unavailable asserts there is no GPU-resident path, and ALPHA2_REQUIRE names " +
        "gpu-resident, that there is one: pass one of them",
    );
  }
  if (!existsSync(parsed.host)) {
    throw new Error(
      `${parsed.host} does not exist. Run \`cargo build --release -p alpha2-host\` first, ` +
        "or pass --host.",
    );
  }
  return parsed;
}
