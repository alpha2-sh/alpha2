#!/usr/bin/env node
// The release gate's list of known failures (known-failures.json): failures that do not block a
// release on the machines and phases they name, each with an owner, the date it was added and the
// release it must be gone by. An entry whose `until` is at or below the workspace version fails
// the gate, as does any entry that does not follow the format.
//
// usage: known-failures.mjs [known-failures.json]   (validates; exits 1 on a problem)

import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "..", "..");
export const DEFAULT_PATH = resolve(import.meta.dirname, "known-failures.json");
export const MACHINES = ["lin", "win", "mac"];
export const PHASES = [
  "env",
  "p1",
  "adapters",
  "corruption",
  "validation",
  "radv",
  "browser",
  "e2e",
  "stress",
  "perf",
];

const REQUIRED = [
  "test",
  "machines",
  "phase",
  "reason",
  "owner",
  "added",
  "until",
];
const OPTIONAL = ["step"];
const SEMVER = /^(\d+)\.(\d+)\.(\d+)$/;

/** -1, 0 or 1 as semver `a` is below, equal to or above `b` (release versions only). */
export function compareVersions(a, b) {
  const [, ...x] = SEMVER.exec(a).map(Number);
  const [, ...y] = SEMVER.exec(b).map(Number);
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] < y[i] ? -1 : 1;
  return 0;
}

/** The workspace version from the root Cargo.toml's `[workspace.package]`. */
export function workspaceVersion(
  cargoToml = readFileSync(resolve(root, "Cargo.toml"), "utf8"),
) {
  const section = cargoToml.split(/^\[workspace\.package\]\r?$/m)[1] ?? "";
  const version = /^version = "([^"]+)"\r?$/m.exec(section.split(/^\[/m)[0]);
  if (!version)
    throw new Error("Cargo.toml has no [workspace.package] version");
  return version[1];
}

function ascii(text) {
  return /^[\x20-\x7e]*$/.test(text);
}

/**
 * The problems of a parsed known-failures document against `version` (the workspace version):
 * an empty list means it is valid.
 */
export function validate(document, version) {
  const problems = [];
  if (
    typeof document !== "object" ||
    document === null ||
    Array.isArray(document)
  )
    return ["the document is not an object"];
  for (const key of Object.keys(document))
    if (key !== "release" && key !== "entries")
      problems.push(`unknown key ${key}`);
  if (document.release !== version)
    problems.push(
      `release is ${JSON.stringify(document.release)}, the workspace is ${version}`,
    );
  if (!Array.isArray(document.entries))
    return [...problems, "entries is not a list"];
  document.entries.forEach((entry, index) => {
    const at = `entry ${index} (${entry?.test ?? "no test"})`;
    if (typeof entry !== "object" || entry === null || Array.isArray(entry)) {
      problems.push(`${at}: not an object`);
      return;
    }
    for (const key of Object.keys(entry))
      if (!REQUIRED.includes(key) && !OPTIONAL.includes(key))
        problems.push(`${at}: unknown key ${key}`);
    for (const key of REQUIRED)
      if (!(key in entry)) problems.push(`${at}: missing ${key}`);
    for (const key of [
      "test",
      "phase",
      "reason",
      "owner",
      "added",
      "until",
      "step",
    ])
      if (key in entry && (typeof entry[key] !== "string" || entry[key] === ""))
        problems.push(`${at}: ${key} is not a non-empty string`);
      else if (key in entry && !ascii(entry[key]))
        problems.push(`${at}: ${key} is not ASCII`);
    if ("machines" in entry) {
      if (!Array.isArray(entry.machines) || entry.machines.length === 0)
        problems.push(`${at}: machines is not a non-empty list`);
      else
        for (const machine of entry.machines)
          if (!MACHINES.includes(machine))
            problems.push(`${at}: unknown machine ${machine}`);
    }
    if (typeof entry.phase === "string" && !PHASES.includes(entry.phase))
      problems.push(`${at}: unknown phase ${entry.phase}`);
    if (
      typeof entry.added === "string" &&
      !/^\d{4}-\d{2}-\d{2}$/.test(entry.added)
    )
      problems.push(`${at}: added is not YYYY-MM-DD`);
    if (typeof entry.until === "string") {
      if (!SEMVER.test(entry.until))
        problems.push(`${at}: until ${entry.until} is not a semver`);
      else if (compareVersions(entry.until, version) <= 0)
        problems.push(
          `${at}: expired (until ${entry.until}, the workspace is ${version})`,
        );
    }
  });
  return problems;
}

/** The entry that excuses `failure` ({ test, phase, step }) on `machine`, or undefined. */
export function excuse(entries, machine, failure) {
  return entries.find(
    (entry) =>
      entry.test === failure.test &&
      entry.phase === failure.phase &&
      entry.machines.includes(machine) &&
      (entry.step === undefined || entry.step === failure.step),
  );
}

/** Reads and validates a known-failures file; returns `{ document, problems }`. */
export function load(path = DEFAULT_PATH, version = workspaceVersion()) {
  let document;
  try {
    document = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    return {
      document: { entries: [] },
      problems: [`${path}: ${error.message}`],
    };
  }
  const problems = validate(document, version);
  const entries = Array.isArray(document?.entries)
    ? document.entries.filter((entry) => Array.isArray(entry?.machines))
    : [];
  return { document: { ...document, entries }, problems };
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const { document, problems } = load(process.argv[2] ?? DEFAULT_PATH);
  for (const problem of problems) console.log(`FAIL ${problem}`);
  if (problems.length === 0)
    console.log(`ok ${document.entries.length} known failure(s)`);
  process.exitCode = problems.length ? 1 : 0;
}
