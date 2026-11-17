#!/usr/bin/env node
// usage: test-report-summary.mjs --lane <lane> [--observe <ledger.json>] [--release]

import {
  appendFileSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { parseArgs } from "node:util";

import {
  collectRecords,
  judge,
  markdownSummary,
  validateLedger,
  workspaceVersion,
} from "./lib/test-report.mjs";

const root = resolve(import.meta.dirname, "..");
const USAGE =
  "usage: test-report-summary.mjs --lane <lane> [--observe <ledger.json>] [--release]\n";

const { values } = parseArgs({
  options: {
    lane: { type: "string" },
    observe: { type: "string" },
    release: { type: "boolean", default: false },
  },
});
if (!values.lane) {
  process.stderr.write(USAGE);
  process.exit(2);
}
const lane = values.lane;

const records = collectRecords({ lane });
const reportPath = join(root, "target", "test-report.jsonl");
mkdirSync(dirname(reportPath), { recursive: true });
writeFileSync(
  reportPath,
  records.map((record) => `${JSON.stringify(record)}\n`).join(""),
);

const steps = JSON.parse(process.env.ALPHA2_CI_STEPS || "{}");
const failures = judge(records, steps, [], {
  lane,
  version: "0.0.0",
  release: false,
}).failures;
/** @type {import("./lib/test-report.mjs").Verdict | undefined} */
let verdict;
/** @type {string | null} */
let ledgerProblem = null;
if (values.observe) {
  try {
    const ledger = validateLedger(
      JSON.parse(readFileSync(resolve(values.observe), "utf8")),
    );
    verdict = judge(records, steps, ledger, {
      lane,
      version: workspaceVersion(),
      release: values.release,
    });
  } catch (error) {
    ledgerProblem = error instanceof Error ? error.message : String(error);
    verdict = judge(records, steps, [], {
      lane,
      version: "0.0.0",
      release: false,
    });
  }
}

let summary = markdownSummary(records, { lane, failures, verdict });
if (ledgerProblem) summary += `\n**Ledger unreadable:** ${ledgerProblem}\n`;
process.stdout.write(summary);
if (process.env.GITHUB_STEP_SUMMARY)
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, summary);

const blocking = verdict ? verdict.unexplained : failures;
/** @param {string} text */
const escapeData = (text) =>
  text.replace(/%/g, "%25").replace(/\r/g, "%0D").replace(/\n/g, "%0A");
/** @param {string} text */
const escapeProperty = (text) =>
  escapeData(text).replace(/:/g, "%3A").replace(/,/g, "%2C");
for (const record of blocking.slice(0, 10))
  process.stdout.write(
    `::error title=${escapeProperty(`${lane} ${record.outcome}`)}::${escapeData(`${record.suite} ${record.test}`)}\n`,
  );

if (verdict) {
  const red =
    ledgerProblem !== null ||
    verdict.unexplained.length > 0 ||
    verdict.expired.length > 0;
  if (red)
    process.stdout.write(
      `${lane}: ${verdict.unexplained.length} failure(s) the ledger does not explain, ${verdict.expired.length} expired entr(ies)\n`,
    );
  process.exit(red ? 1 : 0);
}
