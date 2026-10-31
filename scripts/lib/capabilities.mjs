// The test capability contract, as in the Rust and Python helpers: ALPHA2_REQUIRE turns a named
// capability's skips into failures; other skips are appended to ALPHA2_SKIP_LOG as JSON lines.

import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(import.meta.dirname, "..", "..");

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

export function required(name, env = process.env) {
  checkName(name);
  return requiredCapabilities(env).has(name);
}

export function suiteOf(fileOrUrl) {
  const file = String(fileOrUrl).startsWith("file:")
    ? fileURLToPath(fileOrUrl)
    : String(fileOrUrl);
  return relative(root, file).split("\\").join("/");
}

/**
 * Throws when the run requires `capability`; otherwise logs the skip and returns `reason`.
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
    // One append per line so concurrent processes never interleave.
    appendFileSync(
      path,
      `${JSON.stringify({ lang: "js", suite, test, capability, reason, pid: process.pid })}\n`,
    );
  }
  return reason;
}

const recordedSuites = new Set();

/** The node:test `skip` option; records the skip once per suite and capability. */
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

export function readSkipLog(path) {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line));
}

/**
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
