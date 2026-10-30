// The test capability contract for the JavaScript suites and harnesses, the same as the Rust and
// Python helpers: ALPHA2_REQUIRE (a comma list of names from tests/capabilities.json) turns a named
// capability's skips into failures, and every skip that is not a failure is printed and appended
// to ALPHA2_SKIP_LOG (an absolute path) as one JSON line.

import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(import.meta.dirname, "..", "..");

/** Every capability: name -> `{ description, probe }`, in the order of tests/capabilities.json. */
export const CAPABILITIES = new Map(
  Object.entries(
    JSON.parse(
      readFileSync(resolve(root, "tests", "capabilities.json"), "utf8"),
    ),
  ),
);

function checkName(name) {
  if (!CAPABILITIES.has(name))
    throw new Error(
      `unknown capability ${JSON.stringify(name)}; known: ${[...CAPABILITIES.keys()].join(", ")}`,
    );
}

/**
 * The capabilities `ALPHA2_REQUIRE` names: a comma list, entries trimmed, empty entries ignored.
 * Throws on an unknown name.
 *
 * @returns {Set<string>}
 */
export function requiredCapabilities(env = process.env) {
  const names = (env.ALPHA2_REQUIRE ?? "")
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  for (const name of names) checkName(name);
  return new Set(names);
}

/** Whether `ALPHA2_REQUIRE` names `name`. Throws on an unknown name. */
export function required(name, env = process.env) {
  checkName(name);
  return requiredCapabilities(env).has(name);
}

/** A file's suite name: its path relative to the repository, with forward slashes. */
export function suiteOf(fileOrUrl) {
  const file = String(fileOrUrl).startsWith("file:")
    ? fileURLToPath(fileOrUrl)
    : String(fileOrUrl);
  return relative(root, file).split("\\").join("/");
}

/**
 * Records that `capability` is missing here and returns `reason`.
 *
 * Throws `<name> is required (ALPHA2_REQUIRE): <reason>` when the run requires it; otherwise prints
 * `skipped [<name>]: <reason>` on stderr and appends one record to `ALPHA2_SKIP_LOG` when set.
 *
 * @param {{ suite: string, test?: string, capability: string, reason: string }} skip
 * @returns {string}
 */
export function recordSkip(
  { suite, test = "*", capability, reason },
  env = process.env,
) {
  checkName(capability);
  if (required(capability, env))
    throw new Error(`${capability} is required (ALPHA2_REQUIRE): ${reason}`);
  process.stderr.write(`skipped [${capability}]: ${reason}\n`);
  const path = env.ALPHA2_SKIP_LOG;
  if (path) {
    if (!isAbsolute(path))
      throw new Error(`ALPHA2_SKIP_LOG must be absolute (${path})`);
    mkdirSync(dirname(path), { recursive: true });
    // One append of one whole line: records of concurrent processes never interleave.
    appendFileSync(
      path,
      `${JSON.stringify({ lang: "js", suite, test, capability, reason, pid: process.pid })}\n`,
    );
  }
  return reason;
}

const recordedSuites = new Set();

/**
 * The `skip` option for node:test when `capability` is missing: records the skip once per suite
 * and capability (test `*`), however many tests share it, and returns the reason. Throws when the
 * run requires the capability, which fails the file at load.
 */
export function missingCapability(
  capability,
  suite,
  reason,
  env = process.env,
) {
  const key = `${suite}\n${capability}`;
  if (recordedSuites.has(key)) return reason;
  recordSkip({ suite, capability, reason }, env);
  recordedSuites.add(key);
  return reason;
}

/** The records of a skip log, one per line; blank lines ignored. */
export function readSkipLog(path) {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line));
}

/**
 * Skip counts per capability, and per suite within it, ordered by name.
 *
 * @returns {{ capability: string, count: number, suites: { suite: string, count: number }[] }[]}
 */
export function summarizeSkipLog(records) {
  const byCapability = new Map();
  for (const record of records) {
    const suites = byCapability.get(record.capability) ?? new Map();
    suites.set(record.suite, (suites.get(record.suite) ?? 0) + 1);
    byCapability.set(record.capability, suites);
  }
  return [...byCapability.entries()]
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([capability, suites]) => ({
      capability,
      count: [...suites.values()].reduce((sum, count) => sum + count, 0),
      suites: [...suites.entries()]
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([suite, count]) => ({ suite, count })),
    }));
}
