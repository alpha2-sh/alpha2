// Native checks on encoded files: pipelined vs synchronous exports, decoder cursor concurrency,
// `serve` vs the one-shot CLI, and quality-level resolution, native and wasm alike.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { createInterface } from "node:readline";

import {
  assetPath,
  executable,
  PINNED_RATE,
  PINNED_RATE_ARGUMENTS,
  projectPath,
  root,
  sharedSourceAssetPath,
  sharedSourceProjectPath,
} from "./fixtures.mjs";
import { packagedWasm, run, runJson } from "./processes.mjs";

/**
 * Runs the checks on `project` (the parity project) under `scratch`, and returns the one-shot
 * CLI's export, which the browser half compares with the golden.
 */
export async function nativeOutputChecks(scratch, project) {
  const cliOutput = join(scratch, "cli.mp4");
  const synchronousOutput = join(scratch, "synchronous.mp4");
  const serveOutput = join(scratch, "serve.mp4");
  const sharedSourceOutput = join(scratch, "shared-source.mp4");
  const sharedSourceSynchronousOutput = join(
    scratch,
    "shared-source-synchronous.mp4",
  );
  const sharedSourceConcurrencyOutputs = new Map(
    [1, 2, 4].map((concurrency) => [
      concurrency,
      join(scratch, `shared-source-concurrency-${concurrency}.mp4`),
    ]),
  );
  await run(executable, [
    "export",
    "--project",
    projectPath,
    "--asset",
    `image-source=${assetPath}`,
    "--out",
    cliOutput,
    "--video-codec",
    "mpeg4-part2",
    ...PINNED_RATE_ARGUMENTS,
    "--no-audio",
  ]);
  await run(
    executable,
    [
      "export",
      "--project",
      projectPath,
      "--asset",
      `image-source=${assetPath}`,
      "--out",
      synchronousOutput,
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
    ],
    { ALPHA2_EXPORT_PIPELINE: "off" },
  );
  assert.deepEqual(
    await readFile(synchronousOutput),
    await readFile(cliOutput),
    "pipelined and synchronous exports must be byte-identical",
  );
  console.log("verified byte-identical pipelined/synchronous output");

  const sharedSourceArguments = [
    "export",
    "--project",
    sharedSourceProjectPath,
    "--asset",
    `video-source=${sharedSourceAssetPath}`,
    "--video-codec",
    "mpeg4-part2",
    ...PINNED_RATE_ARGUMENTS,
    "--no-audio",
  ];
  await run(executable, [
    ...sharedSourceArguments,
    "--out",
    sharedSourceOutput,
  ]);
  await run(
    executable,
    [...sharedSourceArguments, "--out", sharedSourceSynchronousOutput],
    { ALPHA2_EXPORT_PIPELINE: "off" },
  );
  assert.deepEqual(
    await readFile(sharedSourceSynchronousOutput),
    await readFile(sharedSourceOutput),
    "shared-source clips must preserve pipeline/synchronous parity",
  );
  const synchronousSharedSource = await readFile(sharedSourceSynchronousOutput);
  for (const [concurrency, output] of sharedSourceConcurrencyOutputs) {
    await run(executable, [...sharedSourceArguments, "--out", output], {
      ALPHA2_DECODE_CONCURRENCY: String(concurrency),
    });
    assert.deepEqual(
      await readFile(output),
      synchronousSharedSource,
      `decode concurrency ${concurrency} must preserve output bytes`,
    );
  }
  console.log("verified independent decoder cursors at concurrency 1/2/4");

  const child = spawn(executable, ["serve"], {
    cwd: root,
    stdio: ["pipe", "pipe", "inherit"],
  });
  const response = new Promise((resolveResponse, reject) => {
    createInterface({ input: child.stdout }).on("line", (line) => {
      const message = JSON.parse(line);
      if (message.type === "progress") return;
      if (message.requestId !== "parity-serve") return;
      if (message.ok) resolveResponse(message.result);
      else reject(new Error(`${message.error.code}: ${message.error.message}`));
    });
    child.once("error", reject);
    child.once("exit", (code) => {
      if (code !== 0) reject(new Error(`alpha2-host serve exited ${code}`));
    });
  });
  child.stdin.end(
    `${JSON.stringify({
      protocolVersion: 1,
      operation: "export",
      requestId: "parity-serve",
      project,
      assets: { "image-source": assetPath },
      effects: [],
      startTicks: 0,
      endTicks: 5_000,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      videoCodec: "mpeg4-part2",
      videoQuality: PINNED_RATE,
      audioCodec: "aac",
      output: serveOutput,
    })}\n`,
  );
  await response;
  assert.deepEqual(
    await readFile(serveOutput),
    await readFile(cliOutput),
    "serve and one-shot CLI must produce byte-identical output",
  );
  console.log("verified byte-identical alpha2-host serve/export output");

  // The host resolves a level before any sink opens, so naming the resolved rate back must
  // reproduce the file byte for byte.
  const levelArguments = [
    "export",
    "--project",
    projectPath,
    "--asset",
    `image-source=${assetPath}`,
    "--video-codec",
    "mpeg4-part2",
    "--no-audio",
  ];
  const levelOutput = join(scratch, "level.mp4");
  const explicitOutput = join(scratch, "explicit.mp4");
  const levelResult = await runJson(executable, [
    ...levelArguments,
    "--video-quality",
    "medium",
    "--out",
    levelOutput,
  ]);
  const resolved = levelResult.result.videoRateControl;
  assert.ok(
    Number.isInteger(resolved?.bitrate) && resolved.bitrate > 0,
    `a level export must report its resolved rate: ${JSON.stringify(resolved)}`,
  );
  await run(executable, [
    ...levelArguments,
    "--video-quality",
    String(resolved.bitrate),
    ...(resolved.quantizer === null
      ? []
      : ["--video-quantizer", String(resolved.quantizer)]),
    "--out",
    explicitOutput,
  ]);
  assert.deepEqual(
    await readFile(explicitOutput),
    await readFile(levelOutput),
    "a level and the bitrate it resolves to must produce byte-identical output",
  );
  console.log(
    `verified level medium and its resolved ${resolved.bitrate} bps are byte-identical`,
  );

  // Both sinks call one function, so a disagreement here is two definitions, not rounding.
  const engine = await packagedWasm();
  const { width, height, frame_rate: frameRate } = project.settings;
  const browserResolved = JSON.parse(
    engine.resolveRateControl(
      "mpeg4-part2",
      width,
      height,
      frameRate.num,
      frameRate.den,
      "medium",
    ),
  );
  // Guards against silently measuring the env override instead of the level.
  assert.equal(
    resolved.source,
    "request",
    "the level must be what decided the rate, not ALPHA2_EXPORT_VIDEO_BITRATE",
  );
  assert.equal(resolved.level, "medium");
  const { bitrate, mode, quantizer } = resolved;
  assert.deepEqual(
    { bitrate, mode, quantizer },
    browserResolved,
    "native and browser sinks must resolve one level to one rate control",
  );
  console.log(
    `verified native and wasm resolve medium to ${JSON.stringify(resolved)}`,
  );
  return cliOutput;
}
