// The other half of the coverage (`--expect-unavailable`): a machine without the GPU path (a VM or
// WARP, a GPU without an encoder for the codec, a driver without the API) must say why, still
// export, and refuse only what asks for the path.

import { existsSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";

import {
  gpuRequiredRefusalProblem,
  gpuWaitTimeoutMs,
  readbackFallbackProblem,
  STARTUP_SLACK_MS,
  startupProblem,
  unavailableCapabilityProblem,
} from "../gpu-export-checks.mjs";
import {
  capabilitiesRequest,
  exportFixture,
  FRAMES,
  partialOf,
  readbackCodecOf,
  root,
  START_SAMPLES,
} from "./harness.mjs";

export function runUnavailable(harness) {
  const { check, runExport, scratch, serve } = harness;
  const reported = harness.capabilities().capabilities;
  const gpu = reported?.encode?.gpuOutputPath;
  const unavailable = unavailableCapabilityProblem(gpu);
  check("the host reports no GPU-resident output path, and why", () => {
    if (unavailable) throw new Error(unavailable);
    process.stdout.write(`  reason: ${gpu.reason}\n`);
  });
  // The rest asserts what an unavailable path does; on a host that has one it would only fail.
  if (unavailable) {
    process.stdout.write(`${harness.failures} GPU export check(s) failed\n`);
    return;
  }
  harness.videoCodec = readbackCodecOf(reported);
  const { videoCodec } = harness;
  const source = join(root, "tests", "corpus", "long-gop.mp4");
  const projectPath = join(scratch, "project.json");
  writeFileSync(projectPath, JSON.stringify(exportFixture(), null, 1));

  check("an auto export completes on the readback path and says why", () => {
    const output = join(scratch, "auto.mp4");
    const exported = runExport({
      project: projectPath,
      assets: ["--asset", `video-source=${source}`],
      output,
      extra: ["--input-policy", "auto"],
    });
    const problem = readbackFallbackProblem(
      { ...exported, outputExists: existsSync(output) },
      FRAMES,
    );
    if (problem) {
      throw new Error(`${problem}: ${exported.stderr.slice(-1000)}`);
    }
    process.stdout.write(
      `  gpuOutputPathUnavailable: ${exported.result.gpuOutputPathUnavailable}\n`,
    );
  });

  // Through the protocol, which carries the refusal's code; the CLI prints only its message.
  check(
    "a gpu-required export fails GPU_OUTPUT_UNAVAILABLE before any output exists",
    () => {
      const output = join(scratch, "required.mp4");
      const { answer } = serve({
        operation: "export",
        requestId: "gpu-export-required",
        project: exportFixture(),
        assets: { "video-source": source },
        startTicks: 0,
        endTicks: 120_000,
        frameRate: { num: FRAMES, den: 1 },
        includeAudio: false,
        videoCodec,
        inputPolicy: "gpu-required",
        output,
      });
      const leftBehind = [output, partialOf(output)]
        .filter((path) => existsSync(path))
        .map((path) => basename(path));
      const problem = gpuRequiredRefusalProblem(answer, leftBehind);
      if (problem) throw new Error(problem);
    },
  );

  // The probe runs once per process at start, bounded by the GPU wait timeout: a host that cannot
  // encode on the GPU must not pay more for finding out. Each start is timed to its answer, against
  // a start with the probe switched off.
  check("the GPU encode probe keeps the host's start within its bound", () => {
    const timeoutMs = gpuWaitTimeoutMs(process.env.ALPHA2_GPU_TIMEOUT_MS);
    const probed = [];
    const baseline = [];
    for (let sample = 0; sample < START_SAMPLES; sample += 1) {
      probed.push(serve(capabilitiesRequest()).ms);
      baseline.push(
        serve(capabilitiesRequest(), { ALPHA2_GPU_ENCODE: "0" }).ms,
      );
    }
    // Every start must hold the bound; the quickest start without the probe is what it adds to.
    const probedMs = Math.max(...probed);
    const baselineMs = Math.min(...baseline);
    const problem = startupProblem({ probedMs, baselineMs, timeoutMs });
    if (problem) throw new Error(problem);
    process.stdout.write(
      `  start: at most ${Math.round(probedMs)} ms with the probe, ${Math.round(baselineMs)} ms ` +
        `without it; bound ${timeoutMs} ms + ${STARTUP_SLACK_MS} ms\n`,
    );
  });

  process.stdout.write(
    harness.failures === 0
      ? "verified that this host refuses the GPU-resident path, says why, and falls back\n"
      : `${harness.failures} GPU export check(s) failed\n`,
  );
}
