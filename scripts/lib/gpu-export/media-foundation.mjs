// Windows only: `hevc_d3d12va` takes GPU frames only, so outputs cannot be compared by framemd5
// against the readback path; each is held to a PSNR-Y floor against its encoder's input instead.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import {
  PSNR_Y_FLOOR_DB,
  psnr,
  psnrFloorProblems,
  renderCacheLuma,
} from "../gpu-export-checks.mjs";
import {
  assertEqual,
  HEIGHT,
  SAMPLE_FRAMES,
  TICKS_PER_FRAME,
  WIDTH,
} from "./harness.mjs";

export function compareWithMediaFoundation(
  harness,
  { baseline, project, assets, lumaCodec },
) {
  const { check, runExport, scratch, videoCodec } = harness;
  const encoder = `${videoCodec}_mf`;
  const output = join(scratch, "readback-mf.mp4");
  let readback = null;
  // Reported, not asserted: at this size the ratio is mostly start-up, but a far slower GPU path is
  // worth a look.
  check(`the readback path runs ${encoder} for comparison`, () => {
    const exported = runExport({
      project,
      assets,
      output,
      env: { ALPHA2_GPU_ENCODE: "0", ALPHA2_VIDEO_ENCODER_HYBRID: "hardware" },
    });
    if (exported.status !== 0) {
      throw new Error(
        `the ${encoder} export failed (${exported.status}): ${exported.stderr.slice(-2000)}`,
      );
    }
    assertEqual(exported.result.videoOutputPath, "readback", "videoOutputPath");
    assertEqual(exported.result.videoEncoder, encoder, "videoEncoder");
    readback = exported;
    const way = exported.result.videoEncoderHardware
      ? "its hardware transform"
      : "the CPU, as no hardware transform opened";
    process.stdout.write(
      `  gpu-resident ${baseline.result.videoEncoder}: ${baseline.result.fps?.toFixed(1)} fps; ` +
        `readback ${encoder} on ${way}: ${exported.result.fps?.toFixed(1)} fps\n`,
    );
  });

  check(
    `each output holds ${PSNR_Y_FLOOR_DB} dB PSNR-Y against the planes its encoder was given`,
    () => {
      const reference = new Map(
        SAMPLE_FRAMES.map((frame) => [
          frame,
          renderedLuma(harness, {
            project,
            assets,
            frame,
            label: "source",
            lumaCodec,
          }),
        ]),
      );
      const outputs = [["gpu-resident", join(scratch, "baseline.mp4")]];
      if (readback !== null) outputs.push([encoder, output]);
      const problems = [];
      for (const [label, path] of outputs) {
        // The output is decoded by rendering it again, at its own size, into the render cache.
        const samples = SAMPLE_FRAMES.map((frame) => ({
          frame,
          psnr: psnr(
            renderedLuma(harness, {
              project,
              assets: ["--asset", `video-source=${path}`],
              frame,
              label,
              lumaCodec,
            }),
            reference.get(frame),
          ),
        }));
        process.stdout.write(
          `  ${label}: ${samples.map((sample) => `${sample.frame} ${sample.psnr.toFixed(2)} dB`).join(", ")}\n`,
        );
        problems.push(
          ...psnrFloorProblems(samples).map(
            (problem) => `${label}: ${problem}`,
          ),
        );
      }
      if (problems.length > 0) throw new Error(problems.join("; "));
    },
  );
}

/**
 * The luma the renderer gives `frame` of `project` with `assets`: a one-frame readback export that
 * bakes it into a fresh render cache, which stores the YUV planes the encoder received.
 */
function renderedLuma(harness, { project, assets, frame, label, lumaCodec }) {
  const { runExport, scratch } = harness;
  const name = `luma-${label.replace(/[^\w-]/g, "_")}-${frame}`;
  const directory = join(scratch, name);
  const exported = runExport({
    project,
    assets,
    output: join(scratch, `${name}.mp4`),
    env: { ALPHA2_GPU_ENCODE: "0" },
    codec: lumaCodec,
    startTicks: frame * TICKS_PER_FRAME,
    endTicks: (frame + 1) * TICKS_PER_FRAME,
    extra: [
      "--input-policy",
      "cpu-readback",
      "--render-cache",
      directory,
      "--render-cache-backlog",
      "wait",
    ],
  });
  if (exported.status !== 0) {
    throw new Error(
      `the ${label} frame ${frame} export failed (${exported.status}): ${exported.stderr.slice(-1000)}`,
    );
  }
  const frames = readdirSync(directory, { recursive: true }).filter((path) =>
    path.endsWith(".frame"),
  );
  if (exported.result.renderCache?.writes !== 1 || frames.length !== 1) {
    throw new Error(
      `the ${label} frame ${frame} export baked ${frames.length} frames (renderCache ` +
        `${JSON.stringify(exported.result.renderCache)}), expected one`,
    );
  }
  const luma = renderCacheLuma(readFileSync(join(directory, frames[0])));
  if (luma.width !== WIDTH || luma.height !== HEIGHT) {
    throw new Error(
      `the ${label} frame ${frame} is ${luma.width}x${luma.height}, expected ${WIDTH}x${HEIGHT}`,
    );
  }
  return luma.y;
}
