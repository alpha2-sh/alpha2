// Per-test results of a CI job and the known-failure ledger observe lanes are judged against.

import { existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import { basename, extname, join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..", "..");

/** Where test steps write their console output (`<suite>.log`). */
export const TEST_LOGS_DIR = join(root, "target", "test-logs");
/** Where test steps write JUnit XML (`<step>.xml` or `<step>.<part>.xml`). */
export const TEST_RESULTS_DIR = join(root, "target", "test-results");

/**
 * Arguments that make `node --test` print its spec output and also write JUnit to
 * `target/test-results/<name>.xml`.
 *
 * @param {string} name
 */
export function nodeTestReporterArgs(name) {
  mkdirSync(TEST_RESULTS_DIR, { recursive: true });
  return [
    "--test-reporter=spec",
    "--test-reporter-destination=stdout",
    "--test-reporter=junit",
    `--test-reporter-destination=${join(TEST_RESULTS_DIR, `${name}.xml`)}`,
  ];
}

/** @typedef {"passed" | "failed" | "skipped" | "hung" | "crashed"} Outcome */

/**
 * @typedef {object} TestRecord
 * @property {string} lane
 * @property {string} step The test step: the log or JUnit file name up to its first `.`.
 * @property {string} suite The test binary, JUnit suite or harness.
 * @property {string} test
 * @property {Outcome} outcome
 * @property {number | null} seconds
 */

/** @param {Outcome} outcome */
export const isFailure = (outcome) =>
  outcome === "failed" || outcome === "hung" || outcome === "crashed";

const CARGO_RUNNING = /^\s+Running (?:(.+?) \((.+)\)|(\S+))$/;
const CARGO_DOCTESTS = /^\s+Doc-tests (\S+)$/;
const LIBTEST_RUNNING = /^running \d+ tests?$/;
const LIBTEST_LINE = /^test (.+?) \.\.\. ?(.*)$/;
const LIBTEST_STATUS = /^(ok|FAILED|ignored(?:, .*)?)$/;
const LIBTEST_SLOW = /^test (.+) has been running for over \d+ seconds$/;
const LIBTEST_RESULT = /^test result: (?:ok|FAILED)\./;
const LIBTEST_CAPTURE = /^---- (.+) (?:stdout|stderr) ----$/;
const CARGO_COMPILE_ERROR = /^error: could not compile `([^`]+)`/;
const HARNESS_LINE = /^(ok|FAIL|skip) (.+)$/;
const XCTEST_LINE =
  /^Test Case '-\[(\S+) (\S+)\]' (passed|failed|skipped) \(([\d.]+) seconds\)\.?$/;
const INSTRUMENTATION_FAILURE = /^\d+\) (\w+)\(([\w.$]+)\)$/;
const WATCHDOG_LINE =
  /^test-watchdog: (\S+) exceeded \d+s; still running: (.+)$/;

const CARGO_HASH = /-[0-9a-f]{16}\b/;

/** @param {string} path */
function binaryName(path) {
  return basename(path.trim())
    .replace(/\.exe$/i, "")
    .replace(CARGO_HASH, "");
}

/**
 * Reads one test step's console output: libtest (cargo, emulator and simulator binaries), the
 * repository's harness lines (`ok <case>`, `FAIL <case>: <why>`, `skip <case>: <why>`), XCTest and
 * Android instrumentation.
 *
 * @param {string} text
 * @param {{ lane: string, step: string, suite?: string }} where `suite` names the tests that no
 *   `Running` line places; the step by default.
 * @returns {TestRecord[]}
 */
export function parseTestLog(text, { lane, step, suite: initial = step }) {
  /** @type {TestRecord[]} */
  const records = [];
  /** @param {string} suite @param {string} test @param {Outcome} outcome @param {number | null} [seconds] */
  const add = (suite, test, outcome, seconds = null) =>
    records.push({ lane, step, suite, test, outcome, seconds });

  let suite = initial;
  let inBinary = false;
  let capturing = false;
  let failureList = false;
  /** @type {string | null} */
  let pending = null;
  const slow = new Set();
  /** @type {Set<string>} */
  let seen = new Set();
  /** @type {{ binary: string, name: string }[]} */
  const watchdog = [];
  let instrumentationFailures = false;

  const endBinary = (finished) => {
    if (inBinary && !finished) {
      if (pending !== null)
        add(suite, pending, slow.has(pending) ? "hung" : "crashed");
      else add(suite, "(test binary exited without a result)", "crashed");
    }
    inBinary = false;
    capturing = false;
    failureList = false;
    pending = null;
    slow.clear();
    seen = new Set();
  };
  /** @param {string} name @param {string} status */
  const finish = (name, status) => {
    seen.add(name);
    add(
      suite,
      name,
      status === "ok" ? "passed" : status === "FAILED" ? "failed" : "skipped",
    );
  };

  for (const raw of text.split(/\r?\n/)) {
    const line = raw.replace(/\x1b\[[0-9;]*m/g, "");
    const running = capturing ? null : CARGO_RUNNING.exec(line);
    const doctests = capturing ? null : CARGO_DOCTESTS.exec(line);
    if (running || doctests) {
      endBinary(false);
      suite = doctests
        ? `${doctests[1]} doctests`
        : binaryName(running?.[2] ?? running?.[3] ?? "");
      continue;
    }
    const watched = WATCHDOG_LINE.exec(line);
    if (watched) {
      const binary = binaryName(watched[1]);
      for (const name of watched[2].split(/,\s*/))
        if (name && name !== "<unknown>") watchdog.push({ binary, name });
      if (inBinary && suite === binary) endBinary(true);
      continue;
    }
    if (LIBTEST_RUNNING.test(line)) {
      inBinary = true;
      continue;
    }
    if (inBinary) {
      if (LIBTEST_RESULT.test(line)) {
        endBinary(true);
        continue;
      }
      if (LIBTEST_CAPTURE.test(line)) {
        capturing = true;
        failureList = false;
        continue;
      }
      if (line === "failures:") {
        capturing = false;
        failureList = true;
        continue;
      }
      if (failureList) {
        const name = /^ {4}(\S.*)$/.exec(line)?.[1];
        if (name && !seen.has(name)) finish(name, "FAILED");
        continue;
      }
      if (capturing) continue;
      const slowTest = LIBTEST_SLOW.exec(line);
      if (slowTest) {
        slow.add(slowTest[1]);
        continue;
      }
      const result = LIBTEST_LINE.exec(line);
      if (result) {
        if (LIBTEST_STATUS.test(result[2])) {
          finish(result[1], result[2]);
          if (pending === result[1]) pending = null;
        } else pending = result[1];
        continue;
      }
      if (pending !== null && LIBTEST_STATUS.test(line.trim())) {
        finish(pending, line.trim());
        pending = null;
      }
      continue;
    }
    const compile = CARGO_COMPILE_ERROR.exec(line);
    if (compile) {
      add("build", `compile ${compile[1]}`, "failed");
      continue;
    }
    const xctest = XCTEST_LINE.exec(line);
    if (xctest) {
      const outcome =
        xctest[3] === "passed"
          ? "passed"
          : xctest[3] === "failed"
            ? "failed"
            : "skipped";
      add(xctest[1], xctest[2], outcome, Number(xctest[4]));
      continue;
    }
    if (/^There (?:was|were) \d+ failures?:$/.test(line)) {
      instrumentationFailures = true;
      continue;
    }
    if (instrumentationFailures) {
      const failed = INSTRUMENTATION_FAILURE.exec(line);
      if (failed) add(failed[2], failed[1], "failed");
      continue;
    }
    const harness = HARNESS_LINE.exec(line);
    if (harness) {
      const [, status, rest] = harness;
      const name = (
        status === "ok" ? rest.split(" ")[0] : rest.split(": ")[0].trim()
      )
        .replace(CARGO_HASH, "")
        .replace(/ on .*$/, "");
      add(
        step,
        name,
        status === "ok" ? "passed" : status === "FAIL" ? "failed" : "skipped",
      );
    }
  }
  endBinary(false);
  for (const { binary, name } of watchdog) {
    const known = records.find(
      (record) =>
        record.suite === binary &&
        record.test === name &&
        isFailure(record.outcome),
    );
    if (known) known.outcome = "hung";
    else add(binary, name, "hung");
  }
  return records;
}

/** @param {string} value */
function unescapeXml(value) {
  return value
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&#(\d+);/g, (_, code) => String.fromCodePoint(Number(code)))
    .replace(/&amp;/g, "&");
}

/** @param {string} tag @param {string} name */
function attribute(tag, name) {
  const match = new RegExp(`\\s${name}="([^"]*)"`).exec(tag);
  return match ? unescapeXml(match[1]) : null;
}

/**
 * Reads a JUnit XML file as Node's test runner and pytest write it. A test inside nested
 * `<testsuite>` elements is named by their names joined with ` > `; a pytest test by its
 * `classname::name`.
 *
 * @param {string} xml
 * @param {{ lane: string, step: string, suite: string }} where
 * @returns {TestRecord[]}
 */
export function parseJunit(xml, { lane, step, suite }) {
  /** @type {TestRecord[]} */
  const records = [];
  /** @type {string[]} */
  const suites = [];
  const tags =
    /<(\/?)(testsuites?|testcase|failure|error|skipped)\b((?:[^>"]|"[^"]*")*?)(\/?)>/g;
  /** @type {{ test: string, seconds: number | null, outcome: Outcome } | null} */
  let open = null;
  for (const match of xml.matchAll(tags)) {
    const [, closing, name, attributes, selfClosing] = match;
    if (name === "testsuite") {
      if (closing) suites.pop();
      else if (!selfClosing) suites.push(attribute(attributes, "name") ?? "");
      continue;
    }
    if (name === "testcase") {
      if (closing) {
        if (open) records.push({ lane, step, suite, ...open });
        open = null;
        continue;
      }
      const test = attribute(attributes, "name") ?? "";
      const classname = attribute(attributes, "classname") ?? "";
      const time = attribute(attributes, "time");
      const nested = suites.filter((entry) => entry && entry !== "pytest");
      const full =
        classname && classname !== "test"
          ? `${classname}::${test}`
          : [...nested, test].join(" > ");
      const testcase = {
        test: full,
        seconds: time === null ? null : Number(time),
        /** @type {Outcome} */
        outcome: "passed",
      };
      if (selfClosing) records.push({ lane, step, suite, ...testcase });
      else open = testcase;
      continue;
    }
    if (open && !closing)
      open.outcome = name === "skipped" ? "skipped" : "failed";
  }
  return records;
}

/** @param {string} file */
const stepOf = (file) => basename(file).split(".")[0];

/** @param {string} file */
function suiteOf(file) {
  const [step, ...parts] = basename(file, extname(file)).split(".");
  return parts.length === 0 ? step : binaryName(parts.join("."));
}

/**
 * Every record under the logs and results directories.
 *
 * @param {{ lane: string, logsDir?: string, resultsDir?: string }} options
 * @returns {TestRecord[]}
 */
export function collectRecords({
  lane,
  logsDir = TEST_LOGS_DIR,
  resultsDir = TEST_RESULTS_DIR,
}) {
  /** @type {TestRecord[]} */
  const records = [];
  /** @param {string} directory @param {string} extension */
  const files = (directory, extension) =>
    existsSync(directory)
      ? readdirSync(directory)
          .filter((name) => extname(name) === extension)
          .sort()
          .map((name) => join(directory, name))
      : [];
  for (const file of files(logsDir, ".log"))
    records.push(
      ...parseTestLog(readFileSync(file, "utf8"), {
        lane,
        step: stepOf(file),
        suite: suiteOf(file),
      }),
    );
  for (const file of files(resultsDir, ".xml"))
    records.push(
      ...parseJunit(readFileSync(file, "utf8"), {
        lane,
        step: stepOf(file),
        suite: basename(file, ".xml"),
      }),
    );
  return records;
}

/**
 * @typedef {object} LedgerEntry
 * @property {string} lane
 * @property {string} test A test name as the report records it, or `*` for any test of the lane.
 * @property {number} [maxFailing] With `*`: how many failures the entry explains at most.
 * @property {string} reason
 * @property {string} added `YYYY-MM-DD`
 * @property {string} until The release that must ship without this failure.
 */

const ENTRY_KEYS = new Set([
  "lane",
  "test",
  "maxFailing",
  "reason",
  "added",
  "until",
]);
const VERSION = /^\d+\.\d+\.\d+$/;

/**
 * The ledger's entries, or throws naming every malformed one.
 *
 * @param {unknown} ledger
 * @returns {LedgerEntry[]}
 */
export function validateLedger(ledger) {
  const failures =
    ledger !== null && typeof ledger === "object" && "failures" in ledger
      ? ledger.failures
      : undefined;
  if (!Array.isArray(failures))
    throw new Error('the ledger is not an object with a "failures" array');
  /** @type {string[]} */
  const problems = [];
  failures.forEach((entry, index) => {
    const where = `failures[${index}]`;
    if (entry === null || typeof entry !== "object") {
      problems.push(`${where} is not an object`);
      return;
    }
    for (const key of Object.keys(entry))
      if (!ENTRY_KEYS.has(key))
        problems.push(`${where} has unknown key ${key}`);
    for (const key of ["lane", "test", "reason"])
      if (typeof entry[key] !== "string" || entry[key].trim() === "")
        problems.push(`${where}.${key} is not a non-empty string`);
    if (
      typeof entry.added !== "string" ||
      !/^\d{4}-\d{2}-\d{2}$/.test(entry.added)
    )
      problems.push(`${where}.added is not YYYY-MM-DD`);
    if (typeof entry.until !== "string" || !VERSION.test(entry.until))
      problems.push(`${where}.until is not a release version x.y.z`);
    const wildcard = entry.test === "*";
    if (
      wildcard &&
      !(Number.isInteger(entry.maxFailing) && entry.maxFailing > 0)
    )
      problems.push(`${where}.maxFailing must be a positive integer with "*"`);
    if (!wildcard && entry.maxFailing !== undefined)
      problems.push(`${where}.maxFailing is only for test "*"`);
  });
  if (problems.length > 0)
    throw new Error(`malformed ledger:\n  ${problems.join("\n  ")}`);
  return /** @type {LedgerEntry[]} */ (failures);
}

/** @param {string} a @param {string} b */
export function compareVersions(a, b) {
  const left = a.split(".").map(Number);
  const right = b.split(".").map(Number);
  for (let i = 0; i < 3; i += 1)
    if (left[i] !== right[i]) return left[i] < right[i] ? -1 : 1;
  return 0;
}

/**
 * Whether an entry no longer explains anything: its release has shipped (the workspace is past
 * it), or this is that release's own run.
 *
 * @param {LedgerEntry} entry
 * @param {{ version: string, release: boolean }} at
 */
export function expired(entry, { version, release }) {
  const order = compareVersions(version, entry.until);
  return order > 0 || (release && order === 0);
}

/**
 * @typedef {object} StepState
 * @property {string} [outcome]
 * @property {string} [conclusion]
 */

/**
 * @typedef {object} Verdict
 * @property {TestRecord[]} failures Every failed, hung or crashed test, plus one record for each
 *   failed step that reported no failed test.
 * @property {{ record: TestRecord, entry: LedgerEntry }[]} known
 * @property {TestRecord[]} unexplained
 * @property {LedgerEntry[]} expired
 */

/**
 * Judges an observe lane: every failure must match a live ledger entry of the lane, and no entry
 * of the lane may have expired.
 *
 * @param {TestRecord[]} records
 * @param {Record<string, StepState>} steps The job's `steps` context.
 * @param {LedgerEntry[]} ledger
 * @param {{ lane: string, version: string, release: boolean }} options
 * @returns {Verdict}
 */
export function judge(records, steps, ledger, { lane, version, release }) {
  const failures = records.filter((record) => isFailure(record.outcome));
  for (const [id, state] of Object.entries(steps))
    if (
      state.outcome === "failure" &&
      !failures.some((record) => record.step === id)
    )
      failures.push({
        lane,
        step: id,
        suite: id,
        test: `step ${id} failed without a failed test`,
        outcome: "failed",
        seconds: null,
      });
  const entries = ledger.filter((entry) => entry.lane === lane);
  const live = entries.filter((entry) => !expired(entry, { version, release }));
  /** @type {Verdict["known"]} */
  const known = [];
  /** @type {TestRecord[]} */
  const rest = [];
  for (const record of failures) {
    const entry = live.find(
      (candidate) => candidate.test !== "*" && candidate.test === record.test,
    );
    if (entry) known.push({ record, entry });
    else rest.push(record);
  }
  const wildcard = live.find((entry) => entry.test === "*");
  if (wildcard && rest.length <= (wildcard.maxFailing ?? 0)) {
    known.push(...rest.map((record) => ({ record, entry: wildcard })));
    rest.length = 0;
  }
  return {
    failures,
    known,
    unexplained: rest,
    expired: entries.filter((entry) => expired(entry, { version, release })),
  };
}

/** The workspace version from the root `Cargo.toml`. */
export function workspaceVersion(cargoToml = join(root, "Cargo.toml")) {
  const text = readFileSync(cargoToml, "utf8");
  const section = /^\[workspace\.package\]$([\s\S]*?)^\[/m.exec(text);
  const version = section && /^version = "([^"]+)"$/m.exec(section[1]);
  if (!version)
    throw new Error(`no [workspace.package] version in ${cargoToml}`);
  return version[1];
}

/** @param {string} text */
const cell = (text) => text.replace(/\|/g, "\\|").replace(/\r?\n/g, " ");

/**
 * The Markdown a job writes to its step summary.
 *
 * @param {TestRecord[]} records
 * @param {{ lane: string, failures?: TestRecord[], verdict?: Verdict, limit?: number }} options
 *   `failures` defaults to the failed records; `verdict` makes it an observe lane's summary.
 */
export function markdownSummary(
  records,
  { lane, failures: given, verdict, limit = 50 },
) {
  /** @type {Record<string, number>} */
  const counts = {};
  for (const record of records)
    counts[record.outcome] = (counts[record.outcome] ?? 0) + 1;
  const tally =
    Object.entries(counts)
      .map(([outcome, count]) => `${count} ${outcome}`)
      .join(", ") || "no test results recorded";
  const lines = [`### Tests: ${lane}`, "", tally, ""];
  const failures =
    verdict?.failures ??
    given ??
    records.filter((record) => isFailure(record.outcome));
  /** @param {string} title @param {TestRecord[]} rows @param {(record: TestRecord) => string} [note] */
  const table = (title, rows, note) => {
    if (rows.length === 0) return;
    lines.push(
      `**${title}** (${rows.length})`,
      "",
      `| Outcome | Suite | Test |${note ? " Ledger |" : ""}`,
      `| --- | --- | --- |${note ? " --- |" : ""}`,
    );
    for (const record of rows.slice(0, limit))
      lines.push(
        `| ${record.outcome} | ${cell(record.suite)} | ${cell(record.test)} |${note ? ` ${cell(note(record))} |` : ""}`,
      );
    if (rows.length > limit)
      lines.push(`| ... | | ${rows.length - limit} more |`);
    lines.push("");
  };
  if (!verdict) {
    table("Failed", failures);
    return `${lines.join("\n")}\n`;
  }
  table("Not in the known-failure ledger", verdict.unexplained);
  const reasons = new Map(
    verdict.known.map(({ record, entry }) => [
      record,
      `${entry.reason} (until ${entry.until})`,
    ]),
  );
  table(
    "Known failures (ledger)",
    verdict.known.map(({ record }) => record),
    (record) => reasons.get(record) ?? "",
  );
  if (verdict.expired.length > 0) {
    lines.push(`**Expired ledger entries** (${verdict.expired.length})`, "");
    for (const entry of verdict.expired)
      lines.push(
        `- ${cell(entry.test)}: until ${entry.until}, ${cell(entry.reason)}`,
      );
    lines.push("");
  }
  return `${lines.join("\n")}\n`;
}
