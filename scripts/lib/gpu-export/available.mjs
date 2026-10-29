// Cases for a host that reports a GPU-resident output path.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import {
  copyLedgerProblem,
  encoderInputCopyLedger,
} from "../gpu-export-checks.mjs";
import { budgetChecks } from "./budget.mjs";
import { cancelChecks } from "./cancel.mjs";
import {
  assertEqual,
  exportFixture,
  FRAMES,
  HEIGHT,
  readbackCodecOf,
  root,
  WIDTH,
} from "./harness.mjs";
import { compareWithMediaFoundation } from "./media-foundation.mjs";
import { qualityChecks } from "./quality.mjs";

export function runAvailable(harness) {
  const { check, options, runExport, scratch } = harness;
  // Skipping on the reported capability rather than a failed export keeps an unavailable path
  // distinct from a broken one.
  const reported = harness.capabilities().capabilities;
  const gpu = reported?.encode?.gpuOutputPath ?? {};
  if (gpu.available !== true) {
    const message = `this host reports no GPU-resident output path: ${
      gpu.reason ?? "the capability is missing"
    }`;
    if (options.requireGpu) throw new Error(message);
    process.stderr.write(`skipped: ${message}\n`);
    return;
  }
  // Not H.264 everywhere: the Windows path on FFmpeg 8.0.3 encodes HEVC only.
  if (typeof gpu.videoCodec !== "string") {
    throw new Error(
      "the host reports a GPU-resident output path without its videoCodec",
    );
  }
  harness.videoCodec = gpu.videoCodec;

  const projectPath = join(scratch, "project.json");
  writeFileSync(projectPath, JSON.stringify(exportFixture(), null, 1));
  const assets = [
    "--asset",
    `video-source=${join(root, "tests", "corpus", "long-gop.mp4")}`,
  ];

  const baseline = runExport({
    project: projectPath,
    assets,
    output: join(scratch, "baseline.mp4"),
  });
  if (baseline.status !== 0) {
    throw new Error(
      `the baseline export failed (${baseline.status}): ${baseline.stderr.slice(-2000)}`,
    );
  }
  check("the export reports the GPU-resident path", () => {
    assertEqual(baseline.result.videoPipeline, "gpu-resident", "videoPipeline");
    assertEqual(baseline.result.videoEncoder, gpu.encoder, "videoEncoder");
    assertEqual(
      baseline.result.performance.videoPipeline,
      "gpu-resident",
      "performance.videoPipeline",
    );
    assertEqual(
      baseline.result.performance.readbackCopyBytes,
      0,
      "readbackCopyBytes",
    );
    assertEqual(
      baseline.result.performance.readbackPackMs,
      0,
      "readbackPackMs",
    );
    assertEqual(baseline.result.frames, FRAMES, "frames");
  });

  // Each plane of each frame is copied once; more is unaccounted, fewer means missing planes.
  const ledger = encoderInputCopyLedger(
    gpu.inputHandoff,
    FRAMES,
    WIDTH,
    HEIGHT,
  );
  if (ledger !== null) {
    check("the gpu-copy handoff copies each frame's two planes once", () => {
      const problem = copyLedgerProblem(baseline.result.performance, ledger);
      if (problem) throw new Error(problem);
    });
  }

  const readbackCodec = readbackCodecOf(reported);
  if (process.platform === "win32") {
    compareWithMediaFoundation(harness, {
      baseline,
      project: projectPath,
      assets,
      lumaCodec: readbackCodec,
    });
  }

  const context = { gpu, projectPath, assets, baseline, readbackCodec };
  qualityChecks(harness, context);
  budgetChecks(harness, context);
  cancelChecks(harness, context);

  process.stdout.write(
    harness.failures === 0
      ? "verified the GPU-resident export path and its cancel contract\n"
      : `${harness.failures} GPU export check(s) failed\n`,
  );
}
