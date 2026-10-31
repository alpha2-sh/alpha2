#!/usr/bin/env node
// Exports one stress project with the release host under a ten-minute limit and records the
// outcome and wall time in <out>/<name>.result.json. An export that finishes passes, and so does
// one the host refuses with a classified GPU-work error (a timed-out or lost device); anything
// else, including running past the limit, fails.
//
// usage: stress-export.mjs <stress directory> <project file name> [<effects file name>]

import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";

import { executable } from "../lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..", "..");
const LIMIT_MS = 600_000;
// The GPU-work outcomes the host classifies; a stress export may end in one of them.
const CLASSIFIED = new Set(["GPU_TIMEOUT", "GPU_DEVICE_LOST"]);

const [directory, projectName, effectsName] = process.argv.slice(2);
if (!directory || !projectName) {
  console.log(
    "usage: stress-export.mjs <stress directory> <project file name> [<effects file name>]",
  );
  process.exit(2);
}
const name = basename(projectName, ".json");
const host =
  process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "release");
const project = JSON.parse(readFileSync(join(directory, projectName), "utf8"));
const end = Math.max(
  ...project.timeline.tracks.flatMap((track) =>
    track.clips.map((clip) => clip.timeline_range.end),
  ),
);
// Through `serve`, whose answer carries the error code that `export` only prints as a message.
const request = {
  protocolVersion: 1,
  requestId: "stress",
  operation: "export",
  project,
  assets: {
    video: join(root, "tests", "corpus", "parity-rgb.mov"),
    still: join(root, "tests", "corpus", "parity-image.png"),
  },
  effects: effectsName
    ? JSON.parse(readFileSync(join(directory, effectsName), "utf8"))
    : [],
  startTicks: 0,
  endTicks: end,
  frameRate: project.settings.frame_rate,
  includeAudio: false,
  output: join(directory, `${name}.mp4`),
  overwrite: true,
};
const started = performance.now();
const result = spawnSync(host, ["serve"], {
  input: `${JSON.stringify(request)}\n`,
  encoding: "utf8",
  timeout: LIMIT_MS,
  killSignal: "SIGKILL",
  maxBuffer: 64 << 20,
});
const wallMs = Math.round(performance.now() - started);
let answer = null;
for (const line of (result.stdout ?? "").trim().split(/\r?\n/).reverse()) {
  try {
    const message = JSON.parse(line);
    if ("ok" in message) {
      answer = message;
      break;
    }
  } catch {
    // progress or a partial line
  }
}
const code = answer?.error?.code ?? null;
let outcome;
if (result.error?.code === "ETIMEDOUT") outcome = "timeout";
else if (answer?.ok) outcome = "finished";
else if (CLASSIFIED.has(code)) outcome = "refused";
else outcome = "failed";
writeFileSync(
  join(directory, `${name}.result.json`),
  JSON.stringify(
    {
      project: projectName,
      outcome,
      code,
      rc: result.status,
      wallMs,
      answer,
      stderr: result.stderr?.slice(-8000),
    },
    null,
    2,
  ) + "\n",
);
process.stderr.write(result.stderr?.slice(-4000) ?? "");
if (outcome === "finished" || outcome === "refused")
  console.log(
    `ok ${name}: ${outcome}${code ? ` (${code})` : ""} in ${(wallMs / 1000).toFixed(1)} s`,
  );
else
  console.log(
    `FAIL ${name}: ${outcome === "timeout" ? `still running after ${LIMIT_MS / 1000} s` : `exit ${result.status}${code ? ` ${code}` : ""}`}`,
  );
process.exitCode = outcome === "finished" || outcome === "refused" ? 0 : 1;
