// Exercises the published Node client against a real alpha2-host process.
// `npm run test:node-e2e` builds the host binary first.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { mkdtemp, rm, stat, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";

import {
  Alpha2ConformError,
  Alpha2GpuInitializationError,
  Alpha2NodeClient,
  Alpha2ProjectValidationError,
} from "../../dist/index.js";

const root = resolve(import.meta.dirname, "..", "..", "..", "..");
const hostPath =
  process.env.ALPHA2_HOST_BINARY ??
  join(
    root,
    "target",
    "debug",
    `alpha2-host${process.platform === "win32" ? ".exe" : ""}`,
  );
const projectPath = join(root, "tests", "cases", "host-parity-project.json");
const assetPath = join(root, "tests", "corpus", "cross_topology_parity.png");

const client = new Alpha2NodeClient({ hostPath });
const project = JSON.parse(await readFile(projectPath, "utf8"));

// The pinned LGPL FFmpeg has no software H.264 encoder, so H.264 tests skip on hosts without a
// platform or GPU encoder. ALPHA2_REQUIRE_H264=1 turns the skip into a failure.
const requireH264 = process.env.ALPHA2_REQUIRE_H264 === "1";
const encode = (await client.getCapabilities()).capabilities.encode;
const h264Available =
  encode.video.some((codec) => codec.id === "h264") ||
  (encode.gpuOutputPath?.available === true &&
    encode.gpuOutputPath.videoCodec === "h264");
const h264Absence = `this host cannot produce H.264: the runtime offers ${encode.video
  .map((codec) => `${codec.id}:${codec.encoder}`)
  .join(", ")} and the GPU path is ${encode.gpuOutputPath?.reason ?? "absent"}`;
const skipWithoutH264 =
  h264Available || requireH264 ? false : `skipped: ${h264Absence}`;

/**
 * Asserts that `performance` has the shapes @alpha2-sh/node declares (`ExportPerformance`): a scalar or
 * `null` per counter, and the declared non-scalar counters, which a copy does not report.
 */
function assertDeclaredPerformance(performance, path) {
  const declared = {
    hardwareFallbackReasons: (value) =>
      typeof value === "object" &&
      value !== null &&
      Object.values(value).every(Number.isSafeInteger),
    encodeChannelDepthSamples: (value) =>
      Array.isArray(value) && value.every(Number.isSafeInteger),
    gpuEncodeBudget: (value) =>
      value === null ||
      [
        "surfaceBytes",
        "planeRingBytes",
        "leaseBound",
        "retainedSurfaces",
        "surfaceCeiling",
        "reservedBytes",
      ].every((key) => typeof value[key] === "number"),
  };
  for (const [key, value] of Object.entries(performance)) {
    if (key in declared)
      assert.ok(declared[key](value), `${key}: ${JSON.stringify(value)}`);
    else
      assert.ok(
        value === null ||
          ["number", "boolean", "string"].includes(typeof value),
        `${key} is a counter the declarations do not name, and not a scalar: ${JSON.stringify(value)}`,
      );
  }
  for (const key of Object.keys(declared))
    assert.equal(key in performance, path !== "packet-copy", key);
  if (path === "readback") assert.equal(performance.gpuEncodeBudget, null);
  if (path === "gpu-resident")
    assert.notEqual(performance.gpuEncodeBudget, null);
}

test("the client reports the host's encode capabilities", async () => {
  const capabilities = await client.getCapabilities();
  assert.equal(capabilities.status, "ready");
  assert.equal(capabilities.capabilities.encode.container, "mp4");
  assert.ok(
    capabilities.capabilities.encode.video.some(
      (codec) => codec.id === "mpeg4-part2",
    ),
    "the reference software encoder must always be reported",
  );
  // The GPU-resident path's encoder takes hardware frames, so it is deliberately absent here.
  const h264 = capabilities.capabilities.encode.video.filter(
    (codec) => codec.id === "h264",
  );
  if (requireH264) assert.ok(h264Available, h264Absence);
  if (!h264Available)
    assert.equal(
      typeof capabilities.capabilities.encode.gpuOutputPath?.reason,
      "string",
      "a host without H.264 reports why its GPU path is unavailable",
    );
  assert.equal(
    h264.filter((codec) => codec.preferred).length,
    h264.length > 0 ? 1 : 0,
    "exactly one H.264 encoder is the preferred one",
  );
  for (const codec of capabilities.capabilities.encode.video) {
    assert.ok(
      ["software", "hardware", "hybrid"].includes(codec.encoderClass),
      `${codec.encoder} must report an encoder class`,
    );
  }
  assert.ok(capabilities.effectCatalog.effects.length > 0);
  assert.ok(
    typeof capabilities.adapter === "string" && capabilities.adapter.length > 0,
  );
  const { hardwareUnavailable } = capabilities.capabilities.decode;
  assert.ok(
    hardwareUnavailable === null || typeof hardwareUnavailable === "string",
    `decode.hardwareUnavailable: ${JSON.stringify(hardwareUnavailable)}`,
  );
});

test("the client can tell in advance whether an export will be GPU-resident", async () => {
  const capabilities = await client.getCapabilities();
  const gpu = capabilities.capabilities.encode.gpuOutputPath;
  assert.ok(gpu, "the host must describe the GPU-resident output path");
  assert.equal(typeof gpu.available, "boolean");
  assert.equal(gpu.requiresEvenDimensions, true);
  if (!gpu.available) {
    assert.equal(
      typeof gpu.reason,
      "string",
      "an unavailable GPU path must say why",
    );
    assert.ok(gpu.reason.length > 0);
    return;
  }
  assert.equal(gpu.reason, null);
  assert.equal(gpu.videoCodec, "h264");
  assert.equal(typeof gpu.encoder, "string");
  // Below the device's minimum coded extent an output silently falls back to the readback path.
  for (const key of [
    "minimumWidth",
    "minimumHeight",
    "maximumWidth",
    "maximumHeight",
  ]) {
    assert.ok(
      Number.isInteger(gpu[key]) && gpu[key] > 0,
      `${key} must be a positive integer, got ${gpu[key]}`,
    );
  }
  assert.ok(gpu.minimumWidth <= gpu.maximumWidth);
  assert.ok(gpu.minimumHeight <= gpu.maximumHeight);

  assert.ok(Number.isInteger(gpu.widthAlignment) && gpu.widthAlignment > 0);
  assert.ok(Number.isInteger(gpu.heightAlignment) && gpu.heightAlignment > 0);

  // The extents apply to the aligned coded size. Below the minimum, a runtime with a software
  // H.264 encoder falls back to readback and one without fails naming the encoder; neither may
  // claim the GPU path. A 2x2 minimum (VideoToolbox) leaves no even size to check.
  const below = {
    width: even(Math.max(2, gpu.minimumWidth - gpu.widthAlignment)),
    height: even(Math.max(2, gpu.minimumHeight - gpu.heightAlignment)),
  };
  const hasBelow =
    below.width < gpu.minimumWidth || below.height < gpu.minimumHeight;
  if (hasBelow) {
    let belowMinimum = null;
    let refused = null;
    try {
      belowMinimum = await exportSize({ ...below, name: "below-minimum.mp4" });
    } catch (error) {
      refused = error;
    }
    if (refused) {
      assert.match(
        refused.message,
        /encoder/i,
        `an output below the minimum extent failed for an unexpected reason: ${refused.message}`,
      );
    } else {
      assert.equal(
        belowMinimum.videoOutputPath,
        "readback",
        "an output below the encoder's minimum extent must not claim the GPU path",
      );
    }
  }

  // Inside it must take the GPU path, unless the capability reported it unavailable above.
  const inside = await exportSize({
    width: even(Math.max(gpu.minimumWidth, 256)),
    height: even(Math.max(gpu.minimumHeight, 192)),
    name: "inside-extent.mp4",
  });
  assert.equal(
    inside.videoOutputPath,
    "gpu-resident",
    `an output inside the advertised extent took the ${inside.videoOutputPath} path`,
  );
  assert.equal(inside.videoEncoder, gpu.encoder);
  assert.equal(inside.inputPolicy, "auto");
  assert.equal(
    inside.performance.readbackCopyBytes,
    0,
    "the GPU-resident path must read nothing back",
  );

  for (const options of [
    { videoEncoder: gpu.encoder },
    { inputPolicy: "gpu-required" },
  ]) {
    const pinned = await exportSize({
      width: even(Math.max(gpu.minimumWidth, 256)),
      height: even(Math.max(gpu.minimumHeight, 192)),
      name: "required.mp4",
      ...options,
    });
    assert.equal(
      pinned.videoOutputPath,
      "gpu-resident",
      JSON.stringify(options),
    );
  }
  // Required outside the extent is an error naming the path, never a fallback.
  const outside = [
    {
      width: gpu.maximumWidth + even(gpu.widthAlignment),
      height: even(Math.max(gpu.minimumHeight, 192)),
      name: "required-above-maximum.mp4",
    },
  ];
  if (hasBelow) outside.push({ ...below, name: "required-below-minimum.mp4" });
  for (const size of outside) {
    await assert.rejects(
      exportSize({ ...size, inputPolicy: "gpu-required" }),
      (error) => error.code === "GPU_OUTPUT_UNAVAILABLE",
      `${size.width}x${size.height}`,
    );
  }
});

function even(value) {
  return value % 2 === 0 ? value : value + 1;
}

async function exportSize({ width, height, name, ...options }) {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-"));
  const output = join(scratch, name);
  let result;
  try {
    await client.exportProject({
      project: { ...project, settings: { ...project.settings, width, height } },
      assets: { "image-source": assetPath },
      startTicks: 0,
      endTicks: 5_000,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      output,
      ...options,
      onResult: (value) => {
        result = value;
      },
    });
    return result;
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

test("the client exports a project through the host binary", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-"));
  const output = join(scratch, "export.mp4");
  const progress = [];
  let result;
  try {
    const capabilities = await client.getCapabilities();
    const encoder = capabilities.capabilities.encode.video.find(
      (candidate) =>
        candidate.id === "mpeg4-part2" && !candidate.hardwareAccelerated,
    );
    assert.ok(encoder, "the reference software encoder must be selectable");
    const written = await client.exportProject({
      project,
      assets: { "image-source": assetPath },
      startTicks: 0,
      endTicks: 5_000,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      videoCodec: "mpeg4-part2",
      videoEncoder: encoder.encoder,
      output,
      onProgress: (value) => progress.push(value),
      onResult: (value) => {
        result = value;
      },
    });

    assert.equal(written, output);
    const { size } = await stat(output);
    assert.ok(size > 0, "the export must not be empty");
    // ISO base media files carry `ftyp` at byte four.
    const header = (await readFile(output)).subarray(4, 8).toString("ascii");
    assert.equal(header, "ftyp");

    assert.ok(progress.length > 0, "the host must report export progress");
    for (const { completed, total } of progress) {
      assert.ok(Number.isSafeInteger(completed) && completed >= 0);
      assert.ok(completed <= total);
    }
    assert.equal(progress.at(-1)?.completed, progress.at(-1)?.total);
    assert.equal(progress.at(-1)?.phase, "finalizing");
    assert.equal(result?.frames, 1);
    assert.deepEqual(result?.effectiveRange, {
      startTicks: 0,
      endTicks: 5_000,
    });
    assert.equal(result?.color?.workingSpace, "linear_bt709_sdr");
    assert.equal(result?.color?.output, "bt709_sdr");
    assert.equal(result?.color?.hdrSources, "tone_map");
    assert.ok(
      ["rgba16float", "rgba8unorm"].includes(result?.color?.intermediateFormat),
    );
    assert.equal(typeof result?.performance?.frameSubmitMs, "number");
    assertDeclaredPerformance(result.performance, result.videoOutputPath);
    assert.deepEqual(result?.decodeDiagnostics, []);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("a failed readback reaches the client classified, at its frame, with the host's evidence", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-failure-"));
  const output = join(scratch, "failed.mp4");
  const previous = process.env.ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME;
  process.env.ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME = "1";
  try {
    await assert.rejects(
      client.exportProject({
        project,
        assets: { "image-source": assetPath },
        startTicks: 0,
        endTicks: 15_000,
        frameRate: { num: 24, den: 1 },
        includeAudio: false,
        videoCodec: "mpeg4-part2",
        inputPolicy: "cpu-readback",
        output,
      }),
      (error) => {
        assert.equal(error.code, "ALPHA2_ERROR");
        assert.match(error.message, /GPU texture readback failed: map-failed/);
        assert.equal(error.failure.code, "READBACK_FAILED");
        assert.equal(error.failure.resource, "unknown");
        assert.equal(error.failure.phase, "readback");
        assert.equal(error.failure.stage, "map-failed");
        assert.equal(error.failure.frameIndex, 1);
        assert.equal(error.failure.origin, "native-host");
        const diagnostics = error.failure.diagnostics;
        assert.equal(diagnostics.videoOutputPath, "readback");
        assert.equal(diagnostics.request.startTicks, "0");
        assert.equal(diagnostics.gpu.deviceLost, false);
        assert.equal(
          typeof diagnostics.memory.gpu.textures.highWaterBytes,
          "number",
        );
        assert.equal(typeof diagnostics.environment.adapter.name, "string");
        assert.equal(typeof error.stderr, "string");
        return true;
      },
    );
  } finally {
    if (previous === undefined)
      delete process.env.ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME;
    else process.env.ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME = previous;
    await rm(scratch, { recursive: true, force: true });
  }
});

test("the smallest budget that holds one frame writes the same file as the default", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-budget-"));
  const run = async (name, memory) => {
    let result;
    const output = join(scratch, name);
    await client.exportProject({
      project,
      assets: { "image-source": assetPath },
      startTicks: 0,
      endTicks: 40_000,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      videoCodec: "mpeg4-part2",
      inputPolicy: "cpu-readback",
      output,
      ...(memory ? { memory } : {}),
      onResult: (value) => {
        result = value;
      },
    });
    return { result, bytes: await readFile(output) };
  };
  try {
    const roomy = await run("roomy.mp4");
    assert.equal(roomy.result.memoryPolicy.inFlightBytes.source, "default");
    const { stagingBytesPerFrame, packedBytesPerFrame } =
      roomy.result.performance;
    // One readback, the frame being packed, and the frame being encoded: a rendezvous channel.
    const least = stagingBytesPerFrame + 2 * packedBytesPerFrame;
    const tight = await run("tight.mp4", {
      inFlightBytes: least,
      decodeQueueBytes: 64 * 1024 * 1024,
    });
    assert.equal(tight.result.memoryPolicy.inFlightBytes.bytes, least);
    assert.equal(tight.result.memoryPolicy.inFlightBytes.source, "request");
    assert.equal(tight.result.performance.inFlightDepth, 1);
    assert.equal(tight.result.performance.encodeChannelDepth, 0);
    assert.ok(tight.result.performance.peakReadbackBytes <= least);
    assert.equal(tight.result.frames, roomy.result.frames);
    assert.ok(tight.bytes.equals(roomy.bytes), "the outputs differ");
    await assert.rejects(
      run("starved.mp4", { inFlightBytes: least - 1 }),
      (error) =>
        error.code === "RESOURCE_LIMIT" &&
        error.failure?.code === "RESOURCE_LIMIT",
    );
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("a retry after a failure takes the frames already rendered and writes what an uninterrupted run writes", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-resume-"));
  const frames = 12;
  const exportTo = async (name, renderCache) => {
    let result;
    const output = join(scratch, name);
    await client.exportProject({
      project,
      assets: { "image-source": assetPath },
      startTicks: 0,
      endTicks: (frames * 120_000) / 24,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      videoCodec: "mpeg4-part2",
      inputPolicy: "cpu-readback",
      output,
      ...(renderCache ? { renderCache } : {}),
      onResult: (value) => {
        result = value;
      },
    });
    return { result, bytes: await readFile(output) };
  };
  try {
    const clean = await exportTo("clean.mp4");
    for (const failAt of [1, frames - 1]) {
      const renderCache = {
        directory: join(scratch, `cache-${failAt}`),
        quotaBytes: 64 * 1024 * 1024,
        backlog: "wait",
      };
      process.env.ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME = String(failAt);
      process.env.ALPHA2_EXPORT_FAIL_ONCE_MARKER = join(
        scratch,
        `failed-${failAt}`,
      );
      try {
        await assert.rejects(
          exportTo(`interrupted-${failAt}.mp4`, renderCache),
          (error) =>
            error.failure?.code === "READBACK_FAILED" &&
            error.failure.frameIndex === failAt,
        );
        const resumed = await exportTo(`resumed-${failAt}.mp4`, renderCache);
        assert.equal(resumed.result.renderCache.hits, failAt);
        assert.equal(resumed.result.renderCache.writes, frames - failAt);
        assert.equal(resumed.result.frames, clean.result.frames);
        assert.ok(resumed.bytes.equals(clean.bytes), `resumed at ${failAt}`);
      } finally {
        delete process.env.ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME;
        delete process.env.ALPHA2_EXPORT_FAIL_ONCE_MARKER;
      }
    }
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("a second export of the same snapshot takes its frames from the render cache", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-bake-"));
  try {
    const exported = async (name) => {
      let result;
      await client.exportProject({
        project,
        assets: { "image-source": assetPath },
        startTicks: 0,
        endTicks: 20_000,
        frameRate: { num: 24, den: 1 },
        includeAudio: false,
        videoCodec: "mpeg4-part2",
        renderCache: { directory: join(scratch, "cache"), backlog: "wait" },
        output: join(scratch, name),
        onResult: (value) => {
          result = value;
        },
      });
      return result.renderCache;
    };
    const first = await exported("first.mp4");
    assert.equal(first.status, "used");
    assert.equal(first.writes, 4);
    const second = await exported("second.mp4");
    assert.deepEqual([second.hits, second.writes], [4, 0]);
    assert.deepEqual(
      await readFile(join(scratch, "first.mp4")),
      await readFile(join(scratch, "second.mp4")),
    );
    await assert.rejects(
      client.exportProject({
        project,
        assets: { "image-source": assetPath },
        startTicks: 0,
        endTicks: 5_000,
        frameRate: { num: 24, den: 1 },
        renderCache: { directory: "" },
        output: join(scratch, "invalid.mp4"),
      }),
      (error) => error.code === "INVALID_INPUT",
    );
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test(
  "without a GPU the host still conforms media, and a capability probe is an Alpha2GpuInitializationError",
  {
    skip:
      process.platform !== "linux" &&
      "hiding every GPU from the host this way (no Vulkan ICD, the Vulkan backend forced) is Linux's",
  },
  async () => {
    const hidden = {
      VK_ICD_FILENAMES: "/nonexistent",
      WGPU_BACKEND: "vulkan",
    };
    const saved = Object.fromEntries(
      Object.keys(hidden).map((key) => [key, process.env[key]]),
    );
    Object.assign(process.env, hidden);
    try {
      // A client of its own: this one's capability report is not the shared client's.
      const gpuless = new Alpha2NodeClient({ hostPath });
      const conformed = await gpuless.conform({
        project,
        assets: { "image-source": assetPath },
      });
      assert.equal(conformed.report.ready, true);
      const error = await gpuless.getCapabilities().then(
        () => assert.fail("the probe resolved without a GPU"),
        (cause) => cause,
      );
      assert.ok(error instanceof Alpha2GpuInitializationError, String(error));
      assert.equal(error.code, "GPU_INIT_FAILED");
      assert.equal(typeof error.stderr, "string");
    } finally {
      for (const [key, value] of Object.entries(saved))
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
    }
  },
);

test("the host conforms media before it renders, and says what an export read", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-conform-"));
  try {
    const ready = await client.conform({
      project,
      assets: { "image-source": assetPath },
    });
    assert.equal(ready.report.ready, true);
    assert.equal(ready.catalog.assets[0].original.state, "online");
    assert.deepEqual(ready.report.sources[0].selection, { media: "original" });

    const request = {
      project,
      assets: { "image-source": join(scratch, "moved-away.png") },
      startTicks: 0,
      endTicks: 5_000,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      videoCodec: "mpeg4-part2",
      output: join(scratch, "refused.mp4"),
    };
    await assert.rejects(client.exportProject(request), (error) => {
      assert.ok(error instanceof Alpha2ConformError, String(error));
      assert.equal(error.report.ready, false);
      assert.deepEqual(error.report.sources[0].issues, [
        { kind: "offline", blocking: true },
      ]);
      return true;
    });
    await assert.rejects(stat(request.output), "no output is started");

    let result;
    await client.exportProject({
      ...request,
      output: join(scratch, "proxied.mp4"),
      proxies: { "image-source": [{ path: assetPath }] },
      media: "proxies",
      onResult: (value) => {
        result = value;
      },
    });
    assert.deepEqual(result.media["image-source"], {
      media: "proxy",
      id: "image-source#proxy0",
    });
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("an export that names no codec gets H.264, or is refused without it", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-"));
  const output = join(scratch, "default-codec.mp4");
  let result;
  try {
    const exporting = client.exportProject({
      // The 64x48 parity fixture is below every NVENC encoder's minimum frame size.
      project: {
        ...project,
        settings: { ...project.settings, width: 256, height: 192 },
      },
      assets: { "image-source": assetPath },
      startTicks: 0,
      endTicks: 5_000,
      frameRate: { num: 24, den: 1 },
      includeAudio: false,
      output,
      onResult: (value) => {
        result = value;
      },
    });
    if (!h264Available && !requireH264) {
      // No silent downgrade to MPEG-4 Part 2: the export fails and writes nothing.
      await assert.rejects(exporting, (error) => {
        // Decided from the host's own encoder list: a retry meets it again.
        assert.equal(error.code, "INVALID_INPUT");
        assert.equal(error.failure?.code, "INVALID_INPUT");
        assert.match(error.message, /no usable h264 encoder/);
        return true;
      });
      await assert.rejects(stat(output), { code: "ENOENT" });
      return;
    }
    await exporting;
    assert.equal(result?.videoCodec, "h264");
    assert.equal(typeof result?.videoEncoder, "string");
    assert.ok(
      ["software", "hardware", "hybrid"].includes(result?.videoEncoderClass),
    );
    assert.ok(Array.isArray(result?.videoEncoderFallbacks));
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("an invalid project is rejected before a host process is started", async () => {
  await assert.rejects(
    () =>
      client.exportProject({
        project: { ...project, schema_version: 999 },
        assets: { "image-source": assetPath },
        startTicks: 0,
        endTicks: 5_000,
        frameRate: { num: 24, den: 1 },
        output: join(tmpdir(), "alpha2-node-e2e-never-written.mp4"),
      }),
    Alpha2ProjectValidationError,
  );
});

test("an empty export range is rejected before a host process is started", async () => {
  await assert.rejects(
    () =>
      client.exportProject({
        project,
        assets: { "image-source": assetPath },
        startTicks: 5_000,
        endTicks: 5_000,
        frameRate: { num: 24, den: 1 },
        output: join(tmpdir(), "alpha2-node-e2e-never-written.mp4"),
      }),
    /range/,
  );
});

// MPEG-4 Part 2 at 256x192 is at its quality ceiling, so this needs H.264.
test(
  "videoQuality reaches the encoder: low writes fewer bytes than very-high",
  { skip: skipWithoutH264 },
  async () => {
    // A single frame is at the encoder's quality ceiling at every level, so use a second of video.
    const videoProject = JSON.parse(
      await readFile(
        join(root, "tests", "cases", "host-video-export-project.json"),
        "utf8",
      ),
    );
    videoProject.settings = {
      ...videoProject.settings,
      width: 256,
      height: 192,
      frame_rate: { num: 30, den: 1 },
    };
    const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-"));
    try {
      const exported = {};
      for (const level of ["low", "very-high"]) {
        const output = join(scratch, `${level}.mp4`);
        let result;
        await client.exportProject({
          project: videoProject,
          assets: {
            "video-source": join(root, "tests", "corpus", "long-gop.mp4"),
          },
          startTicks: 0,
          endTicks: 120_000,
          frameRate: { num: 30, den: 1 },
          includeAudio: false,
          videoQuality: level,
          output,
          onResult: (value) => {
            result = value;
          },
        });
        assert.equal(result?.videoRateControl?.level, level);
        assert.equal(result?.videoRateControl?.source, "request");
        exported[level] = {
          bitrate: result.videoRateControl.bitrate,
          bytes: (await stat(output)).size,
        };
      }
      assert.ok(
        exported.low.bitrate < exported["very-high"].bitrate,
        JSON.stringify(exported),
      );
      assert.ok(
        exported.low.bytes < exported["very-high"].bytes,
        `low must write fewer bytes than very-high: ${JSON.stringify(exported)}`,
      );
    } finally {
      await rm(scratch, { recursive: true, force: true });
    }
  },
);

test("an invalid quality is rejected before a host process is started, naming the field", async () => {
  const base = {
    project,
    assets: { "image-source": assetPath },
    startTicks: 0,
    endTicks: 5_000,
    frameRate: { num: 24, den: 1 },
    output: join(tmpdir(), "alpha2-node-e2e-never-written.mp4"),
  };
  for (const [field, value, needle] of [
    ["videoQuality", "ultra", "videoQuality"],
    ["videoQuality", "lossless", "videoQuality"],
    ["videoQuality", { bitrate: 0 }, "videoQuality.bitrate"],
    ["videoQuality", { bitrate: 1.5 }, "videoQuality.bitrate"],
    ["videoQuality", { bitrate: 8_000_000, mode: "cbr" }, "videoQuality.mode"],
    ["videoQuality", { bitrate: 8_000_000, crf: 23 }, "videoQuality"],
    ["audioQuality", "loud", "audioQuality"],
    ["audioQuality", { bitrate: 128_000, mode: "constant" }, "audioQuality"],
  ]) {
    await assert.rejects(
      () => client.exportProject({ ...base, [field]: value }),
      (error) =>
        error.code === "INVALID_INPUT" && error.message.includes(needle),
      `${field}=${JSON.stringify(value)}`,
    );
  }
});

test("a range that is its source untouched is copied, and a copy that cannot be made is refused by name", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-"));
  const second = 120_000;
  const copyProject = (sourceStart, duration, hasAudio = false) => ({
    schema_version: 1,
    id: "copy",
    name: "Copy",
    settings: {
      width: 128,
      height: 96,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48_000,
    },
    sources: [
      { id: "video", locator: "copy.mp4", kind: "video", has_audio: hasAudio },
    ],
    timeline: {
      tracks: [
        {
          id: "v",
          name: "V",
          kind: "video",
          clips: [
            {
              id: "clip",
              source_id: "video",
              timeline_range: { start: 0, end: duration },
              source_range: { start: sourceStart, end: sourceStart + duration },
            },
          ],
        },
      ],
    },
  });
  const assets = {
    video: join(root, "tests", "corpus", "h264-bt709-copy.mp4"),
  };
  try {
    // No encoder takes part, so this holds on a host that cannot encode H.264 at all.
    let result;
    await client.exportProject({
      project: copyProject(second, second),
      assets,
      startTicks: 0,
      endTicks: second,
      frameRate: { num: 30, den: 1 },
      includeAudio: false,
      strategy: "require-copy",
      output: join(scratch, "gop.mp4"),
      onResult: (value) => {
        result = value;
      },
    });
    assert.equal(result.videoOutputPath, "packet-copy");
    assertDeclaredPerformance(result.performance, "packet-copy");
    assert.equal(result.strategy, "copy");
    assert.equal(result.requestedStrategy, "require-copy");
    assert.equal(result.frames, 30);
    assert.equal(result.videoRateControl, null);
    assert.equal(result.performance.copiedVideoPackets, 30);
    assert.ok((await stat(join(scratch, "gop.mp4"))).size > 0);

    await client.exportProject({
      project: copyProject(second, second, true),
      assets,
      startTicks: 0,
      endTicks: second,
      frameRate: { num: 30, den: 1 },
      includeAudio: true,
      strategy: "require-copy",
      output: join(scratch, "gop-audio.mp4"),
      onResult: (value) => {
        result = value;
      },
    });
    assert.equal(result.videoOutputPath, "packet-copy");
    assert.equal(result.audio, true);
    assert.equal(result.audioStrategy, "encode");
    assert.match(result.audioCopyIneligible, /limiter|whole/);
    assert.equal(result.performance.copiedVideoPackets, 30);

    await assert.rejects(
      client.exportProject({
        project: copyProject(second / 2, second),
        assets,
        startTicks: 0,
        endTicks: second,
        frameRate: { num: 30, den: 1 },
        includeAudio: false,
        strategy: "require-copy",
        output: join(scratch, "mid-gop.mp4"),
      }),
      (error) => error.code === "COPY_INELIGIBLE" && /IDR/.test(error.message),
    );
    await assert.rejects(stat(join(scratch, "mid-gop.mp4")));
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("a clip shown as it is reaches the GPU encoder as its decoded planes", async (context) => {
  const gpu = encode.gpuOutputPath;
  const [width, height] = [256, 144];
  if (
    gpu?.available !== true ||
    width < gpu.minimumWidth ||
    height < gpu.minimumHeight
  ) {
    context.skip(
      `this host has no GPU-resident path for ${width}x${height}: ${gpu?.reason ?? "below its extent"}`,
    );
    return;
  }
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-"));
  const half = 60_000;
  const transcodeProject = (outputWidth, outputHeight) => ({
    schema_version: 1,
    id: "transcode",
    name: "Transcode",
    settings: {
      width: outputWidth,
      height: outputHeight,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48_000,
    },
    sources: [{ id: "video", locator: "transcode.mp4", kind: "video" }],
    timeline: {
      tracks: [
        {
          id: "v",
          name: "V",
          kind: "video",
          clips: [
            {
              id: "clip",
              source_id: "video",
              timeline_range: { start: 0, end: half },
              source_range: { start: 0, end: half },
            },
          ],
        },
      ],
    },
  });
  const exportAt = async (outputWidth, outputHeight, name) => {
    let result;
    await client.exportProject({
      project: transcodeProject(outputWidth, outputHeight),
      assets: {
        video: join(root, "tests", "corpus", "h264-bt709-transcode.mp4"),
      },
      startTicks: 0,
      endTicks: half,
      frameRate: { num: 30, den: 1 },
      includeAudio: false,
      output: join(scratch, name),
      onResult: (value) => {
        result = value;
      },
    });
    return result;
  };
  try {
    const transcoded = await exportAt(width, height, "transcoded.mp4");
    assert.equal(transcoded.videoOutputPath, "gpu-resident");
    assert.equal(transcoded.strategy, "render");
    assert.equal(transcoded.videoTranscode, "yuv-repack");
    assert.equal(transcoded.yuvTranscodeIneligible, null);
    assert.equal(transcoded.frames, 15);
    assert.equal(transcoded.performance.yuvRepacks, 15);
    assert.equal(transcoded.performance.graphPasses, 0);
    assert.equal(transcoded.performance.sourceConversions, 0);
    assertDeclaredPerformance(transcoded.performance, "gpu-resident");

    const rendered = await exportAt(width * 2, height * 2, "rendered.mp4");
    assert.equal(rendered.videoTranscode, null);
    assert.match(rendered.yuvTranscodeIneligible, /not the 512x288 output/);
    assert.equal(rendered.performance.yuvRepacks, 0);
    assert.ok(rendered.performance.graphPasses > 0);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

const heldImageProject = (frames) => {
  const held = structuredClone(project);
  const [clip] = held.timeline.tracks[0].clips;
  clip.timeline_range.end = frames * 5_000;
  clip.source_range.end = frames * 5_000;
  return held;
};
// Long enough that nothing below waits for it to finish: it is stopped first.
const LONG_FRAMES = 200_000;
const heldImageExport = (output, frames) => ({
  project: heldImageProject(frames),
  assets: { "image-source": assetPath },
  startTicks: 0,
  endTicks: frames * 5_000,
  frameRate: { num: 24, den: 1 },
  includeAudio: false,
  videoCodec: "mpeg4-part2",
  output,
});
const partialOf = (output) => output.replace(/\.mp4$/, ".alpha2-partial.mp4");

test("an export aborted mid-frame leaves neither its output nor its partial file, and the same path exports again", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-abort-"));
  const output = join(scratch, "aborted.mp4");
  const partial = partialOf(output);
  try {
    const controller = new AbortController();
    let writing = false;
    await assert.rejects(
      client.exportProject({
        ...heldImageExport(output, LONG_FRAMES),
        signal: controller.signal,
        onProgress: ({ phase, completed }) => {
          if (
            phase !== "frames" ||
            completed === 0 ||
            controller.signal.aborted
          )
            return;
          writing = existsSync(partial);
          controller.abort();
        },
      }),
      { name: "Alpha2AbortError" },
    );
    assert.ok(writing, "the export was writing its partial file when aborted");
    assert.equal(existsSync(output), false);
    // On Windows kill() is TerminateProcess, which leaves the partial file for the next export.
    assert.equal(
      existsSync(partial),
      process.platform === "win32",
      "the aborted export left its partial file",
    );
    assert.equal(
      await client.exportProject(heldImageExport(output, 24)),
      output,
    );
    assert.equal(existsSync(partial), false);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("a host killed mid-export leaves a partial file the next export to that path takes over", async () => {
  // SIGKILL, the OOM killer, or a crash removes nothing; the partial's lock goes with the process,
  // which is what marks the file stale.
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-kill-"));
  const output = join(scratch, "killed.mp4");
  const partial = partialOf(output);
  const projectFile = join(scratch, "held.json");
  try {
    await writeFile(projectFile, JSON.stringify(heldImageProject(LONG_FRAMES)));
    const child = spawn(
      hostPath,
      [
        "export",
        "--project",
        projectFile,
        "--asset",
        `image-source=${assetPath}`,
        "--out",
        output,
        "--end-ticks",
        String(LONG_FRAMES * 5_000),
        "--frame-rate",
        "24/1",
        "--no-audio",
        "--video-codec",
        "mpeg4-part2",
      ],
      { stdio: ["ignore", "ignore", "pipe"] },
    );
    const closed = once(child, "close");
    let stderr = "";
    await new Promise((resolveFrames, rejectFrames) => {
      child.stderr.setEncoding("utf8");
      child.stderr.on("data", (chunk) => {
        stderr += chunk;
        if (/alpha2-host export: frames [1-9]/.test(stderr)) resolveFrames();
      });
      child.on("close", () =>
        rejectFrames(new Error(`the host ended first: ${stderr.slice(-500)}`)),
      );
    });
    child.kill("SIGKILL");
    await closed;
    assert.ok(existsSync(partial), "SIGKILL leaves the partial file");
    assert.equal(
      await client.exportProject(heldImageExport(output, 24)),
      output,
    );
    assert.equal(existsSync(partial), false);
    const header = (await readFile(output)).subarray(4, 8).toString("ascii");
    assert.equal(header, "ftyp");
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

test("a second export to an output a running export is writing is refused, naming its partial file", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-node-e2e-in-use-"));
  const output = join(scratch, "shared.mp4");
  const partial = partialOf(output);
  const both = new Alpha2NodeClient({ hostPath, concurrency: 2 });
  const controller = new AbortController();
  try {
    let started;
    const running = new Promise((resolveStarted) => {
      started = resolveStarted;
    });
    const first = both
      .exportProject({
        ...heldImageExport(output, LONG_FRAMES),
        signal: controller.signal,
        onProgress: ({ phase, completed }) => {
          if (phase === "frames" && completed > 0) started();
        },
      })
      .catch((error) => error);
    await running;
    const refused = await both
      .exportProject(heldImageExport(output, 24))
      .catch((error) => error);
    assert.equal(refused.code, "OUTPUT_IN_USE", refused.message);
    assert.ok(
      refused.message.includes(partial),
      `the refusal names the partial file: ${refused.message}`,
    );
    // The same request meets the same running export: a retry policy does not spend attempts on it.
    assert.equal(refused.failure?.code, "INVALID_INPUT");
    assert.ok(
      existsSync(partial),
      "the refusal left the running export's file",
    );
    controller.abort();
    assert.equal((await first).name, "Alpha2AbortError");
    // Terminated where it stood on Windows, the first export leaves its partial file.
    assert.equal(existsSync(partial), process.platform === "win32");
    assert.equal(existsSync(output), false);
  } finally {
    controller.abort();
    await rm(scratch, { recursive: true, force: true });
  }
});

test("an idle host asked to terminate exits as the signal would have made it", async () => {
  // Its serving thread waits for a request line holding stdout; ending the process from the signal
  // thread must not wait for it.
  const child = spawn(hostPath, ["serve"], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  const closed = once(child, "close");
  const answered = new Promise((resolveAnswer) => {
    child.stdout.setEncoding("utf8");
    child.stdout.once("data", resolveAnswer);
  });
  child.stderr.resume();
  child.stdin.write(
    `${JSON.stringify({ protocolVersion: 1, requestId: "idle", operation: "capabilities" })}\n`,
  );
  await answered;
  const killer = setTimeout(() => child.kill("SIGKILL"), 5_000);
  child.kill("SIGTERM");
  const [code, signal] = await closed;
  clearTimeout(killer);
  // The request pipe stays open: only the signal ends it. On Windows kill() is TerminateProcess,
  // which Node reports as the signal it was asked to send.
  assert.deepEqual(
    { code, signal },
    process.platform === "win32"
      ? { code: null, signal: "SIGTERM" }
      : { code: 143, signal: null },
  );
});
