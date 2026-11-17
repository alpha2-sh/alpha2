import test from "node:test";
import assert from "node:assert/strict";

import {
  buildFlakeReport,
  environmentDifference,
  flakeMarkdown,
  isNightly,
} from "./flake-report.mjs";

/** @param {number} id @param {Partial<import("./flake-report.mjs").Run>} [overrides] */
const run = (id, overrides = {}) => ({
  id,
  workflow: "ci",
  event: "workflow_dispatch",
  actor: "github-actions[bot]",
  sha: "aaaaaaaa11",
  attempts: 1,
  conclusion: "success",
  url: `https://example.invalid/${id}`,
  ...overrides,
});

/** @param {number} id @param {number} runId @param {string} name @param {string} conclusion @param {number} [attempt] */
const job = (id, runId, name, conclusion, attempt = 1) => ({
  id,
  runId,
  attempt,
  name,
  conclusion,
});

/** @param {number} runId @param {string} test @param {"passed" | "failed"} outcome @param {Record<string, unknown>} provenance */
const report = (runId, test, outcome, provenance) => ({
  runId,
  attempt: 1,
  lane: "browser-platforms-macos",
  provenance,
  records: [
    {
      lane: "browser-platforms-macos",
      step: "cases",
      suite: "cases",
      test,
      outcome,
      seconds: null,
    },
  ],
});

test("nightly runs are the schedule's dispatches", () => {
  assert.equal(isNightly(run(1)), true);
  assert.equal(isNightly(run(1, { event: "push" })), false);
  assert.equal(isNightly(run(1, { actor: "someone" })), false);
});

test("only provenance keys with no value in common are environment differences", () => {
  assert.deepEqual(
    environmentDifference(
      [{ image: "macos26 1", chrome: "140" }],
      [
        { image: "macos26 2", chrome: "140" },
        { image: "macos26 3", chrome: "140" },
      ],
    ),
    ['image: failed on "macos26 1"; passed on "macos26 2" | "macos26 3"'],
  );
});

test("the report counts green runs and finds jobs and tests that changed at one commit", () => {
  const runs = [
    run(1),
    run(2, { conclusion: "failure" }),
    run(3, { event: "push", sha: "bbbbbbbb22", conclusion: "failure" }),
  ];
  const jobs = [
    job(10, 1, "Browser (macOS VM)", "success"),
    job(11, 2, "Browser (macOS VM)", "failure"),
    job(11, 2, "Browser (macOS VM)", "failure", 2),
    job(12, 1, "Rust tests", "success"),
    job(13, 2, "Rust tests", "success"),
    job(14, 3, "Rust tests", "failure"),
    job(15, 3, "Mobile lint", "skipped"),
  ];
  const reports = [
    report(1, "audio-clock", "passed", { image: "macos26 2" }),
    report(2, "audio-clock", "failed", { image: "macos26 1" }),
    report(3, "export", "failed", { image: "macos26 2" }),
  ];
  const result = buildFlakeReport({ runs, jobs, reports });
  assert.deepEqual(result.workflows, [
    { workflow: "ci", runs: 3, green: 1, nightly: 2, nightlyGreen: 1 },
  ]);
  assert.deepEqual(
    result.flippedJobs.map(({ sha, name, passed, failed }) => [
      sha,
      name,
      passed,
      failed,
    ]),
    [["aaaaaaaa11", "Browser (macOS VM)", 1, 1]],
  );
  assert.equal(result.flippedTests.length, 1);
  assert.equal(
    result.flippedTests[0].name,
    "browser-platforms-macos: cases audio-clock",
  );
  assert.deepEqual(result.flippedTests[0].environment, [
    'image: failed on "macos26 1"; passed on "macos26 2"',
  ]);
  assert.deepEqual(
    result.lanes.map(({ name, executions, failed }) => [
      name,
      executions,
      failed,
    ]),
    [
      ["Browser (macOS VM)", 2, 1],
      ["Rust tests", 3, 1],
    ],
  );
  const markdown = flakeMarkdown(result, {
    branch: "prerelease-v0.6.0",
    runs: 3,
  });
  assert.match(markdown, /\| ci \| 3 \| 33% \| 2 \| 50% \|/);
  assert.match(markdown, /Tests whose result changed at one commit \(1\)/);
  assert.match(
    markdown,
    /\| aaaaaaaa \| browser-platforms-macos: cases audio-clock \| 1 \| 1 \|/,
  );
});
