#!/usr/bin/env node
// usage: ci-flake-report.mjs --branch <branch> [--runs <n>] [--repo <owner/name>] [--out <dir>]

import { execFileSync } from "node:child_process";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";

import { buildFlakeReport, flakeMarkdown } from "./lib/flake-report.mjs";

const USAGE =
  "usage: ci-flake-report.mjs --branch <branch> [--runs <n>] [--repo <owner/name>] [--out <dir>]\n";
const WORKFLOWS = ["ci.yml", "observe.yml"];
const MERGED = /^test-reports-(\w+)-attempt-(\d+)$/;
const JOB_REPORT = /^test-report-(\w+)-(.+)-attempt-(\d+)$/;

const { values } = parseArgs({
  options: {
    branch: { type: "string" },
    runs: { type: "string", default: "30" },
    repo: { type: "string", default: process.env.GITHUB_REPOSITORY },
    out: {
      type: "string",
      default: join(import.meta.dirname, "..", "target", "flake-report"),
    },
  },
});
if (!values.branch || !values.repo) {
  process.stderr.write(USAGE);
  process.exit(2);
}
const { branch, repo } = values;
const runCount = Number(values.runs);
const out = resolve(values.out);
mkdirSync(out, { recursive: true });

const API_ATTEMPTS = 4;
const API_BACKOFF_MS = 5_000;

/** @param {string} path */
function api(path) {
  for (let attempt = 1; ; attempt += 1) {
    try {
      return JSON.parse(
        execFileSync("gh", ["api", path], {
          encoding: "utf8",
          maxBuffer: 64 << 20,
          stdio: ["ignore", "pipe", "pipe"],
        }),
      );
    } catch (error) {
      const stderr = String(
        /** @type {{ stderr?: unknown }} */ (error).stderr ?? "",
      );
      if (attempt === API_ATTEMPTS || !/HTTP 5\d\d/.test(stderr)) throw error;
      Atomics.wait(
        new Int32Array(new SharedArrayBuffer(4)),
        0,
        0,
        API_BACKOFF_MS * attempt,
      );
    }
  }
}

/** @type {import("./lib/flake-report.mjs").Run[]} */
const runs = [];
/** @type {import("./lib/flake-report.mjs").Job[]} */
const jobs = [];
/** @type {import("./lib/flake-report.mjs").Report[]} */
const reports = [];

for (const workflow of WORKFLOWS) {
  /** @type {any[]} */
  let listed;
  try {
    listed = api(
      `repos/${repo}/actions/workflows/${workflow}/runs?branch=${encodeURIComponent(branch)}&status=completed&per_page=${runCount}`,
    ).workflow_runs;
  } catch (error) {
    const stderr = String(
      /** @type {{ stderr?: unknown }} */ (error).stderr ?? "",
    );
    if (!/HTTP 404/.test(stderr)) throw error;
    process.stderr.write(`${workflow} has no runs in ${repo}\n`);
    continue;
  }
  for (const run of listed) {
    runs.push({
      id: run.id,
      workflow: run.name,
      event: run.event,
      actor: run.triggering_actor?.login ?? run.actor?.login ?? "",
      sha: run.head_sha,
      attempts: run.run_attempt,
      conclusion: run.conclusion,
      url: run.html_url,
    });
    for (let attempt = 1; attempt <= run.run_attempt; attempt += 1)
      for (const job of api(
        `repos/${repo}/actions/runs/${run.id}/attempts/${attempt}/jobs?per_page=100`,
      ).jobs)
        jobs.push({
          id: job.id,
          runId: run.id,
          attempt: job.run_attempt ?? attempt,
          name: job.name,
          conclusion: job.conclusion,
        });
    const artifacts = api(
      `repos/${repo}/actions/runs/${run.id}/artifacts?per_page=100`,
    ).artifacts;
    for (const artifact of artifacts) {
      if (artifact.expired || !MERGED.test(artifact.name)) continue;
      const directory = join(out, String(run.id), artifact.name);
      if (!existsSync(directory))
        try {
          execFileSync(
            "gh",
            [
              "run",
              "download",
              String(run.id),
              "-R",
              repo,
              "-n",
              artifact.name,
              "-D",
              directory,
            ],
            { stdio: ["ignore", "ignore", "inherit"] },
          );
        } catch {
          process.stderr.write(
            `could not download ${artifact.name} of run ${run.id}\n`,
          );
          continue;
        }
      for (const name of readdirSync(directory)) {
        const match = JOB_REPORT.exec(name);
        if (!match) continue;
        /** @param {string} file */
        const read = (file) => {
          const path = join(directory, name, file);
          return existsSync(path) ? readFileSync(path, "utf8") : null;
        };
        reports.push({
          runId: run.id,
          attempt: Number(match[3]),
          lane: match[2],
          records: (read("test-report.jsonl") ?? "")
            .split("\n")
            .filter(Boolean)
            .map((line) => JSON.parse(line)),
          provenance: JSON.parse(read("provenance.json") ?? "{}"),
        });
      }
    }
  }
}

const report = buildFlakeReport({ runs, jobs, reports });
writeFileSync(
  join(out, "flake-report.json"),
  `${JSON.stringify({ branch, runs, report }, null, 2)}\n`,
);
const markdown = flakeMarkdown(report, { branch, runs: runCount });
process.stdout.write(markdown);
if (process.env.GITHUB_STEP_SUMMARY)
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, markdown);
