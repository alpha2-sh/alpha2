// Requested quality must reach the video and AAC encoders on both output paths.

import { statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { assertEqual, audioFixture, root } from "./harness.mjs";

export function qualityChecks(harness, { projectPath, assets, readbackCodec }) {
  const { check, runExport, scratch, videoCodec } = harness;
  // MPEG-4 hits its minimum quantizer below every quality level on this fixture, so an explicit
  // 20 kbps low rate is what makes a measurable size difference.
  for (const [path, env, codec, qualities] of [
    ["gpu-resident", {}, videoCodec, ["very-low", "very-high"]],
    [
      "readback",
      { ALPHA2_GPU_ENCODE: "0" },
      readbackCodec,
      readbackCodec === "h264"
        ? ["very-low", "very-high"]
        : ["20000", "very-high"],
    ],
  ]) {
    check(`videoQuality reaches the encoder on the ${path} path`, () => {
      const exports = qualities.map((quality) => {
        const output = join(scratch, `${path}-${quality}.mp4`);
        const exported = runExport({
          project: projectPath,
          assets,
          output,
          env,
          codec,
          extra: ["--video-quality", quality],
        });
        if (exported.status !== 0) {
          throw new Error(
            `${quality} export failed (${exported.status}): ${exported.stderr.slice(-2000)}`,
          );
        }
        assertEqual(exported.result.videoPipeline, path, "videoPipeline");
        const rate = exported.result.videoRateControl;
        assertEqual(
          rate?.source,
          "request",
          `${quality} videoRateControl.source`,
        );
        const level = /^\d+$/.test(quality) ? null : quality;
        assertEqual(rate?.level, level, `${quality} videoRateControl.level`);
        return { quality, rate, bytes: statSync(output).size };
      });
      const [low, high] = exports;
      if (!(low.rate.bitrate < high.rate.bitrate)) {
        throw new Error(
          `rates resolved out of order: ${JSON.stringify(exports)}`,
        );
      }
      if (!(low.bytes < high.bytes)) {
        throw new Error(
          `${codec} on the ${path} path wrote ${low.bytes} bytes at ${low.quality} and ` +
            `${high.bytes} at ${high.quality}: ${JSON.stringify(exports)}`,
        );
      }
      process.stdout.write(
        `  ${path} ${codec}: ${low.quality} ${low.rate.bitrate} bps -> ${low.bytes} B, ` +
          `${high.quality} ${high.rate.bitrate} bps -> ${high.bytes} B\n`,
      );
    });
  }

  // Identical video settings, so the output size difference comes from audio.
  const audioProject = join(scratch, "audio-project.json");
  writeFileSync(audioProject, JSON.stringify(audioFixture(), null, 1));
  const audioAssets = [
    "--asset",
    `image-source=${join(root, "tests", "corpus", "cross_topology_parity.png")}`,
    "--asset",
    `audio-source=${join(root, "tests", "corpus", "audio-only-aac-priming.m4a")}`,
  ];
  for (const [path, env, codec] of [
    ["gpu-resident", {}, videoCodec],
    ["readback", { ALPHA2_GPU_ENCODE: "0" }, readbackCodec],
  ]) {
    check(`audioQuality reaches the AAC encoder on the ${path} path`, () => {
      const exports = ["very-low", "very-high"].map((level) => {
        const output = join(scratch, `audio-${path}-${level}.mp4`);
        const exported = runExport({
          project: audioProject,
          assets: audioAssets,
          output,
          env,
          codec,
          audio: true,
          endTicks: 48_000,
          frameRate: "30/1",
          extra: ["--video-quality", "2000000", "--audio-quality", level],
        });
        if (exported.status !== 0) {
          throw new Error(
            `${level} export failed (${exported.status}): ${exported.stderr.slice(-2000)}`,
          );
        }
        assertEqual(exported.result.videoPipeline, path, "videoPipeline");
        const rate = exported.result.audioRateControl;
        assertEqual(rate?.level, level, `${level} audioRateControl.level`);
        return { level, rate, bytes: statSync(output).size };
      });
      const [low, high] = exports;
      if (!(low.rate.bitrate < high.rate.bitrate && low.bytes < high.bytes)) {
        throw new Error(
          `audio at very-low and very-high on the ${path} path: ${JSON.stringify(exports)}`,
        );
      }
      process.stdout.write(
        `  ${path}: very-low ${low.rate.bitrate} bps -> ${low.bytes} B, ` +
          `very-high ${high.rate.bitrate} bps -> ${high.bytes} B\n`,
      );
    });
  }
}
