#!/usr/bin/env node
// usage: run-node-tests.mjs <report name> <node --test arguments and files>

import { spawnSync } from "node:child_process";

import { nodeTestReporterArgs } from "./lib/test-report.mjs";

const [name, ...rest] = process.argv.slice(2);
if (!name || rest.length === 0) {
  process.stderr.write(
    "usage: run-node-tests.mjs <report name> <node --test arguments and files>\n",
  );
  process.exit(2);
}
const result = spawnSync(
  process.execPath,
  ["--test", ...nodeTestReporterArgs(name), ...rest],
  { stdio: "inherit" },
);
if (result.error) throw result.error;
process.exit(result.status ?? 1);
