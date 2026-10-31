#!/usr/bin/env node
// Writes a release-gate run's report.json from its summary.txt and step logs: every failure each
// step's log names, matched against known-failures.json for the run's machine. Any failure no entry
// excuses, and any problem with the known-failures list, makes the verdict "blocked".
//
// usage: report.mjs <run directory> <machine>

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { excuse, load } from "./known-failures.mjs";

// node --test's spec reporter marks a failed test with U+2716.
const SPEC_FAILED = /^\s*\u2716 (.+?) \(\d+(?:\.\d+)?m?s\)\s*$/;

/**
 * The failures a step's log names: libtest `test <path> ... FAILED` lines, node --test TAP
 * `not ok` and spec reporter lines, pytest `FAILED`/`ERROR` summary lines, the repository
 * harnesses' `FAIL <case>[: detail]` lines, crates that did not compile, and test binaries that
 * crashed rather than failed.
 * Deduplicated, in log order.
 */
export function failuresIn(log) {
  const found = new Set();
  for (const line of log.split(/\r?\n/)) {
    let match;
    if ((match = /^test (.+?) \.\.\. FAILED\s*$/.exec(line)))
      found.add(match[1]);
    else if ((match = /^\s*not ok \d+ - (.+?)\s*(?:#.*)?$/.exec(line)))
      found.add(match[1]);
    else if ((match = SPEC_FAILED.exec(line))) found.add(match[1]);
    else if ((match = /^error: could not compile `([^`]+)`/.exec(line)))
      found.add(`build ${match[1]}`);
    else if ((match = /^(?:FAILED|ERROR) (\S+)/.exec(line)))
      found.add(match[1]);
    else if ((match = /^FAIL (.+?)(?:: .*)?\s*$/.exec(line)))
      found.add(match[1]);
    else if (
      (match =
        /process didn't exit successfully: `?([^`\s]+).*\((signal: \d+[^)]*|exit (?:status|code): (?!101\))[^)]+)\)/.exec(
          line,
        ))
    ) {
      // libtest exits 101 when tests fail; any other end of a test binary is a crash, which
      // names no test.
      const binary = match[1].split(/[\\/]/).pop();
      found.add(`crash ${binary} (${match[2]})`);
    }
  }
  return [...found];
}

/**
 * Parses summary.txt lines `<phase>/<step> rc=<rc> <seconds>s load=<load>`.
 *
 * @returns {{ phase: string, step: string, rc: number, seconds: number, load: string }[]}
 */
export function parseSummary(text) {
  const steps = [];
  for (const line of text.split(/\r?\n/)) {
    const match = /^(\S+?)\/(\S+) rc=(-?\d+) (\d+)s load=(.*)$/.exec(line);
    if (match)
      steps.push({
        phase: match[1],
        step: match[2],
        rc: Number(match[3]),
        seconds: Number(match[4]),
        load: match[5],
      });
  }
  return steps;
}

/**
 * The report of one run: per step its failures and the entries that excuse them, the verdict,
 * and the known-failures entries this machine's run did not need.
 */
export function buildReport({
  machine,
  steps,
  logs,
  knownFailures,
  header = {},
}) {
  const { document, problems } = knownFailures;
  const used = new Set();
  let blocked = problems.length > 0;
  const reported = steps.map((step) => {
    const log = logs.get(`${step.phase}/${step.step}`) ?? "";
    let names = step.rc === 0 ? [] : failuresIn(log);
    // A step that failed without naming a test (a build error, a corrupt capture, a timeout) is
    // one failure under its own name.
    if (step.rc !== 0 && names.length === 0) names = [step.step];
    const failures = names.map((test) => {
      const entry = excuse(document.entries, machine, {
        test,
        phase: step.phase,
        step: step.step,
      });
      if (entry) used.add(entry);
      else blocked = true;
      return entry
        ? { test, excused: true, reason: entry.reason, until: entry.until }
        : { test, excused: false };
    });
    return { ...step, failures };
  });
  const unused = document.entries
    .filter((entry) => entry.machines.includes(machine) && !used.has(entry))
    .filter((entry) => steps.some((step) => step.phase === entry.phase))
    .map((entry) => ({
      test: entry.test,
      phase: entry.phase,
      step: entry.step,
    }));
  return {
    ...header,
    machine,
    verdict: blocked ? "blocked" : "pass",
    knownFailureProblems: problems,
    steps: reported,
    unusedKnownFailures: unused,
  };
}

/** Prints the report's verdict and every unexcused failure; returns the exit code. */
export function printReport(report, log = console.log) {
  for (const problem of report.knownFailureProblems)
    log(`known-failures.json: ${problem}`);
  for (const step of report.steps)
    for (const failure of step.failures)
      log(
        `${failure.excused ? "known" : "BLOCKING"} ${step.phase}/${step.step}: ${failure.test}`,
      );
  if (report.unusedKnownFailures.length > 0)
    log(
      `${report.unusedKnownFailures.length} known failure(s) of the phases run did not fail`,
    );
  log(`verdict ${report.verdict}`);
  return report.verdict === "pass" ? 0 : 1;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const [directory, machine] = process.argv.slice(2);
  if (!directory || !machine) {
    console.log("usage: report.mjs <run directory> <machine>");
    process.exit(2);
  }
  const steps = parseSummary(
    readFileSync(join(directory, "summary.txt"), "utf8"),
  );
  const logs = new Map();
  for (const step of steps) {
    const path = join(directory, `${step.phase}-${step.step}.log`);
    logs.set(
      `${step.phase}/${step.step}`,
      existsSync(path) ? readFileSync(path, "utf8") : "",
    );
  }
  const headerPath = join(directory, "header.json");
  const header = existsSync(headerPath)
    ? JSON.parse(readFileSync(headerPath, "utf8"))
    : {};
  const report = buildReport({
    machine,
    steps,
    logs,
    knownFailures: load(),
    header,
  });
  writeFileSync(
    join(directory, "report.json"),
    JSON.stringify(report, null, 2) + "\n",
  );
  process.exitCode = printReport(report);
}
