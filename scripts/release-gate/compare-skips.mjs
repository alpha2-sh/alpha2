#!/usr/bin/env node
// Lists the skip records (ALPHA2_SKIP_LOG) one release-gate run added and removed against the
// previous release's, by test and reason, for the release notes. An added skip of a capability
// ALPHA2_REQUIRE declares is a failure (the capability contract already fails such a run; this
// names the difference).
//
// usage: compare-skips.mjs <previous skip log> <this skip log>

import { existsSync } from "node:fs";
import { pathToFileURL } from "node:url";

import { readSkipLog, requiredCapabilities } from "../lib/capabilities.mjs";

function key(record) {
  return `${record.suite} :: ${record.test} :: ${record.reason}`;
}

/** The records of `current` absent from `previous` (`added`) and the reverse (`removed`). */
export function compareSkips(previous, current) {
  const before = new Map(previous.map((record) => [key(record), record]));
  const after = new Map(current.map((record) => [key(record), record]));
  return {
    added: [...after].filter(([k]) => !before.has(k)).map(([, r]) => r),
    removed: [...before].filter(([k]) => !after.has(k)).map(([, r]) => r),
  };
}

export function main(args, log = console.log, env = process.env) {
  if (args.length !== 2) {
    log("usage: compare-skips.mjs <previous skip log> <this skip log>");
    return 2;
  }
  const [previous, current] = args.map((path) =>
    existsSync(path) ? readSkipLog(path) : [],
  );
  const { added, removed } = compareSkips(previous, current);
  const declared = requiredCapabilities(env);
  let failed = false;
  for (const record of added) {
    const failure = declared.has(record.capability);
    failed ||= failure;
    log(
      `${failure ? "FAIL added" : "added"} [${record.capability}] ${key(record)}`,
    );
  }
  for (const record of removed)
    log(`removed [${record.capability}] ${key(record)}`);
  log(`${added.length} added, ${removed.length} removed`);
  return failed ? 1 : 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  process.exitCode = main(process.argv.slice(2));
