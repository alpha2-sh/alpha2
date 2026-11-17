// Flake statistics over a branch's recent CI runs.

import { isFailure } from "./test-report.mjs";

/**
 * @typedef {object} Run
 * @property {number} id
 * @property {string} workflow
 * @property {string} event
 * @property {string} actor
 * @property {string} sha
 * @property {number} attempts
 * @property {string | null} conclusion The latest attempt's.
 * @property {string} url
 */

/**
 * @typedef {object} Job
 * @property {number} id
 * @property {number} runId
 * @property {number} attempt
 * @property {string} name
 * @property {string | null} conclusion
 */

/**
 * @typedef {object} Report One job's test report.
 * @property {number} runId
 * @property {number} attempt
 * @property {string} lane
 * @property {import("./test-report.mjs").TestRecord[]} records
 * @property {Record<string, unknown>} provenance
 */

/**
 * @typedef {object} Flip
 * @property {string} sha
 * @property {string} name A job name, or `lane: test`.
 * @property {number} passed
 * @property {number} failed
 * @property {string[]} environment Provenance keys whose values differ between the passing and
 *   failing executions.
 */

/** @param {Run} run */
export const isNightly = (run) =>
  run.event === "workflow_dispatch" && run.actor === "github-actions[bot]";

/** @param {number} part @param {number} whole */
const percent = (part, whole) =>
  whole === 0 ? "-" : `${Math.round((100 * part) / whole)}%`;

/**
 * @param {Record<string, unknown>[]} failing
 * @param {Record<string, unknown>[]} passing
 * @returns {string[]}
 */
export function environmentDifference(failing, passing) {
  const keys = new Set(
    [...failing, ...passing].flatMap((record) => Object.keys(record)),
  );
  /** @param {Record<string, unknown>[]} records @param {string} key */
  const values = (records, key) =>
    new Set(records.map((record) => JSON.stringify(record[key] ?? null)));
  /** @type {string[]} */
  const differences = [];
  for (const key of [...keys].sort()) {
    const failed = values(failing, key);
    const passed = values(passing, key);
    const shared = [...failed].some((value) => passed.has(value));
    if (!shared && failing.length > 0 && passing.length > 0)
      differences.push(
        `${key}: failed on ${[...failed].join(" | ")}; passed on ${[...passed].join(" | ")}`,
      );
  }
  return differences;
}

/**
 * @param {{ runs: Run[], jobs: Job[], reports: Report[] }} input
 */
export function buildFlakeReport({ runs, jobs, reports }) {
  const runById = new Map(runs.map((run) => [run.id, run]));

  /** @type {Map<string, { runs: number, green: number, nightly: number, nightlyGreen: number }>} */
  const workflows = new Map();
  for (const run of runs) {
    const entry = workflows.get(run.workflow) ?? {
      runs: 0,
      green: 0,
      nightly: 0,
      nightlyGreen: 0,
    };
    entry.runs += 1;
    if (run.conclusion === "success") entry.green += 1;
    if (isNightly(run)) {
      entry.nightly += 1;
      if (run.conclusion === "success") entry.nightlyGreen += 1;
    }
    workflows.set(run.workflow, entry);
  }

  const uniqueJobs = [...new Map(jobs.map((job) => [job.id, job])).values()];
  /** @type {Map<string, { executions: number, failed: number }>} */
  const lanes = new Map();
  /** @type {Map<string, { sha: string, name: string, passed: number, failed: number }>} */
  const jobFlips = new Map();
  for (const job of uniqueJobs) {
    if (job.conclusion === "skipped" || job.conclusion === null) continue;
    const run = runById.get(job.runId);
    if (!run) continue;
    const failed = job.conclusion !== "success";
    const lane = lanes.get(job.name) ?? { executions: 0, failed: 0 };
    lane.executions += 1;
    if (failed) lane.failed += 1;
    lanes.set(job.name, lane);
    if (job.conclusion === "cancelled") continue;
    const key = `${run.sha}\0${job.name}`;
    const flip = jobFlips.get(key) ?? {
      sha: run.sha,
      name: job.name,
      passed: 0,
      failed: 0,
    };
    if (failed) flip.failed += 1;
    else flip.passed += 1;
    jobFlips.set(key, flip);
  }

  /** @type {Map<string, { sha: string, name: string, failing: Record<string, unknown>[], passing: Record<string, unknown>[] }>} */
  const tests = new Map();
  /** @type {Map<string, { failed: number, executions: number }>} */
  const testFailures = new Map();
  for (const report of reports) {
    const run = runById.get(report.runId);
    if (!run) continue;
    for (const record of report.records) {
      if (record.outcome === "skipped") continue;
      const name = `${report.lane}: ${record.suite} ${record.test}`;
      const key = `${run.sha}\0${name}`;
      const entry = tests.get(key) ?? {
        sha: run.sha,
        name,
        failing: [],
        passing: [],
      };
      const tally = testFailures.get(name) ?? { failed: 0, executions: 0 };
      tally.executions += 1;
      if (isFailure(record.outcome)) {
        entry.failing.push(report.provenance);
        tally.failed += 1;
      } else entry.passing.push(report.provenance);
      tests.set(key, entry);
      testFailures.set(name, tally);
    }
  }

  /** @type {Flip[]} */
  const flippedJobs = [...jobFlips.values()]
    .filter((flip) => flip.passed > 0 && flip.failed > 0)
    .map((flip) => ({ ...flip, environment: [] }));
  /** @type {Flip[]} */
  const flippedTests = [...tests.values()]
    .filter((entry) => entry.failing.length > 0 && entry.passing.length > 0)
    .map((entry) => ({
      sha: entry.sha,
      name: entry.name,
      passed: entry.passing.length,
      failed: entry.failing.length,
      environment: environmentDifference(entry.failing, entry.passing),
    }));

  return {
    workflows: [...workflows.entries()].map(([workflow, entry]) => ({
      workflow,
      ...entry,
    })),
    lanes: [...lanes.entries()]
      .map(([name, lane]) => ({ name, ...lane }))
      .sort(
        (a, b) =>
          b.failed / b.executions - a.failed / a.executions ||
          a.name.localeCompare(b.name),
      ),
    flippedJobs,
    flippedTests,
    failingTests: [...testFailures.entries()]
      .filter(([, tally]) => tally.failed > 0)
      .map(([name, tally]) => ({ name, ...tally }))
      .sort((a, b) => b.failed - a.failed || a.name.localeCompare(b.name)),
  };
}

/** @param {string} text */
const cell = (text) => text.replace(/\|/g, "\\|").replace(/\r?\n/g, " ");

/**
 * @param {ReturnType<typeof buildFlakeReport>} report
 * @param {{ branch: string, runs: number, limit?: number }} scope
 */
export function flakeMarkdown(report, { branch, runs, limit = 40 }) {
  const lines = [
    `## CI flake report: ${branch}`,
    "",
    `The last ${runs} completed runs of each workflow.`,
    "",
    "| Workflow | Runs | Green | Nightly runs | Nightly green |",
    "| --- | --- | --- | --- | --- |",
    ...report.workflows.map(
      (w) =>
        `| ${w.workflow} | ${w.runs} | ${percent(w.green, w.runs)} | ${w.nightly} | ${percent(w.nightlyGreen, w.nightly)} |`,
    ),
    "",
  ];
  /** @param {string} title @param {Flip[]} flips */
  const flipTable = (title, flips) => {
    lines.push(`### ${title} (${flips.length})`, "");
    if (flips.length === 0) {
      lines.push("None.", "");
      return;
    }
    lines.push(
      "| Commit | Name | Passed | Failed | Environment difference |",
      "| --- | --- | --- | --- | --- |",
    );
    for (const flip of flips.slice(0, limit))
      lines.push(
        `| ${flip.sha.slice(0, 8)} | ${cell(flip.name)} | ${flip.passed} | ${flip.failed} | ${cell(flip.environment.join("; ") || "-")} |`,
      );
    lines.push("");
  };
  flipTable("Tests whose result changed at one commit", report.flippedTests);
  flipTable("Jobs whose result changed at one commit", report.flippedJobs);
  lines.push(
    "### Lane pass rates",
    "",
    "| Lane | Executions | Failed | Pass rate |",
    "| --- | --- | --- | --- |",
    ...report.lanes
      .filter((lane) => lane.failed > 0)
      .slice(0, limit)
      .map(
        (lane) =>
          `| ${cell(lane.name)} | ${lane.executions} | ${lane.failed} | ${percent(lane.executions - lane.failed, lane.executions)} |`,
      ),
    "",
    `Lanes without a failure: ${report.lanes.filter((lane) => lane.failed === 0).length}.`,
    "",
    "### Failing tests",
    "",
    "| Test | Failed | Executions |",
    "| --- | --- | --- |",
    ...report.failingTests
      .slice(0, limit)
      .map((t) => `| ${cell(t.name)} | ${t.failed} | ${t.executions} |`),
    "",
  );
  return `${lines.join("\n")}\n`;
}
