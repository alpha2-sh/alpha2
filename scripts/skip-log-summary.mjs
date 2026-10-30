#!/usr/bin/env node
// Prints what a test run skipped, per capability and suite, from its ALPHA2_SKIP_LOG.
// usage: skip-log-summary.mjs <skip-log.jsonl>

import { existsSync } from "node:fs";

import { readSkipLog, summarizeSkipLog } from "./lib/capabilities.mjs";

const [path] = process.argv.slice(2);
if (!path) {
  process.stderr.write("usage: skip-log-summary.mjs <skip-log.jsonl>\n");
  process.exit(2);
}
if (!existsSync(path)) {
  process.stdout.write(`no skips recorded (${path} does not exist)\n`);
  process.exit(0);
}
const summary = summarizeSkipLog(readSkipLog(path));
if (summary.length === 0) process.stdout.write("no skips recorded\n");
for (const { capability, count, suites } of summary) {
  process.stdout.write(`${capability}: ${count} skip(s)\n`);
  for (const { suite, count: inSuite } of suites)
    process.stdout.write(`  ${suite}: ${inSuite}\n`);
}
