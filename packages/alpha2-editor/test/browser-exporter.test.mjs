// The browser exporter's pipeline: frames in flight within a byte budget, a stalled source, a
// frame no budget holds, an I420 frame the encoder cannot read, and a cancel that discards the
// output before the muxer can commit it. The engine's export schedule, the readbacks and the
// muxer are scripted here; the real encode and mux run in Chrome (`npm run test:editor:package`).
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { decideRetry, isRetryableFailure } from "alpha2/export-retry";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmDirectory = join(root, "packages", "alpha2", "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

/** A WebCodecs frame as the exporter hands it to the muxer: its pixels and how they lie. */
globalThis.VideoFrame ??= class VideoFrame {
  constructor(data, init) {
    this.data = data;
    Object.assign(this, init);
    this.closed = false;
  }
  close() {
    this.closed = true;
  }
};

/**
 * The page's `VideoEncoder`: by default one that encodes every configuration, so the pipeline
 * tests are about the pipeline; `encodes` narrows it to codec strings with these prefixes.
 */
let encodes = null;
globalThis.VideoEncoder = class VideoEncoder {
  static async isConfigSupported(config) {
    return {
      supported:
        encodes === null || encodes.some((p) => config.codec.startsWith(p)),
      config,
    };
  }
};

let exportTimeline;
let estimatedStagingBytes;
let probeEncoders;
if (!skip) {
  // The rate control the exporter resolves is the engine's own.
  const wasm = await import("alpha2/wasm");
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  ({ exportTimeline, probeEncoders } =
    await import("../dist/browser/runtime/exporter.js"));
  ({ estimatedStagingBytes } =
    await import("../dist/browser/runtime/staging.js"));
}

const WIDTH = 64;
const HEIGHT = 36;
const settings = {
  width: WIDTH,
  height: HEIGHT,
  fpsNum: 30,
  fpsDen: 1,
  sampleRate: 48_000,
};

/** An I420 readback of frame `index`: tight planes, unless `layout` says otherwise. */
function step(index, layout) {
  return {
    index,
    timeTicks: BigInt(index * 4_000),
    format: "I420",
    width: WIDTH,
    height: HEIGHT,
    pixels: new Uint8Array((WIDTH * HEIGHT * 3) / 2),
    layout: layout ?? [
      { offset: 0, stride: WIDTH },
      { offset: WIDTH * HEIGHT, stride: WIDTH / 2 },
      { offset: (WIDTH * HEIGHT * 5) / 4, stride: WIDTH / 2 },
    ],
    colorSpace: { matrix: "bt709", primaries: "bt709", transfer: "bt709" },
  };
}

/**
 * An export of `frames` frames that each hold `frameBytes` in flight: the engine's schedule, the
 * readbacks (each resolves a task later), and a muxer whose video writes take `writeMs`. `script`
 * may replace a ticket (`ticket(index, defaultTicket)`) or a readback step. Records what happened
 * in `log`.
 */
function scripted({
  frames = 12,
  frameBytes = estimatedStagingBytes(WIDTH, HEIGHT),
  writeMs = 1,
  ticket = (_index, made) => made,
  readback = (index) => step(index),
  onWrite = () => {},
  holdWrites = false,
} = {}) {
  const log = {
    order: [],
    written: [],
    supplied: [],
    released: 0,
    submitted: 0,
  };
  let next = 0;
  const engine = {
    beginExport() {
      log.order.push("beginExport");
    },
    exportProgress: () =>
      JSON.stringify({
        total: frames,
        audioWindow: { startSample: "0", frames: 0 },
      }),
    endExport() {
      log.order.push("endExport");
    },
    releaseIdleMemory() {
      log.released += 1;
      return 1_000;
    },
  };
  const renderFrame = () => {
    if (next >= frames) return { kind: "complete" };
    const index = next;
    const made = {
      kind: "frame",
      retainedBytes: frameBytes,
      readback: new Promise((done) => setTimeout(() => done(readback(index)))),
    };
    const answer = ticket(index, made);
    if (answer.kind === "frame") {
      next += 1;
      log.submitted += 1;
    }
    return answer;
  };
  const sink = {
    appliedVideoRateControl: null,
    bytesWritten: 0,
    output: null,
    async writeVideo(output) {
      // An encoder that holds its frames and never answers.
      if (holdWrites) return new Promise(() => {});
      await new Promise((done) => setTimeout(done, writeMs));
      log.written.push(Number(output.index));
      output.frame.close();
      onWrite(log);
    },
    async writeAudio() {},
    async finish() {
      this.bytesWritten = 1_000;
      this.output = { kind: "stream", bytes: 1_000 };
      log.order.push("sink.finish");
    },
    async abort() {
      log.order.push("sink.abort");
    },
  };
  const openOutput = async (request) => {
    log.outputRequest = request;
    return opened();
  };
  const opened = () => ({
    mode: "memory",
    estimatedBytes: 1_000,
    storageEstimate: null,
    destination: { kind: "memory", maxBytes: 1e9 },
    stats: () => null,
    abort: async () => {
      log.order.push("output.abort");
    },
  });
  const run = ({ inFlightBytes, signal, options = {} } = {}) =>
    exportTimeline({
      supplyFrame: async (seconds, missing) => {
        log.supplied.push([seconds, missing]);
      },
      renderFrame,
      engine,
      project: null,
      doc: { settings },
      library: null,
      audioSupplier: null,
      range: { start: 0, end: frames / 30 },
      options: {
        includeAudio: false,
        ...(inFlightBytes ? { memory: { inFlightBytes } } : {}),
        ...options,
      },
      openOutput,
      createSink: (sinkOptions) => {
        log.sinkOptions = sinkOptions;
        return sink;
      },
      signal,
    });
  return { log, run };
}

const quiet = async (body) => {
  const log = console.log;
  console.log = () => {};
  try {
    return await body();
  } finally {
    console.log = log;
  }
};

test(
  "frames in flight stay within the budget, and the renderer's idle memory is given back once per pressure episode",
  { skip },
  async () => {
    const frameBytes = estimatedStagingBytes(WIDTH, HEIGHT);
    // Room for two frames and a half: one readback and one encoder copy, rarely a third.
    const { log, run } = scripted({ frames: 40, frameBytes, writeMs: 2 });
    const { result } = await quiet(() =>
      run({ inFlightBytes: Math.floor(frameBytes * 2.5) }),
    );
    assert.deepEqual(
      log.written,
      Array.from({ length: 40 }, (_, index) => index),
    );
    const memory = result.performance.memory;
    assert.ok(memory.budgetWaits > 10, `${memory.budgetWaits} budget waits`);
    assert.ok(
      memory.peakInFlightBytes <= Math.floor(frameBytes * 2.5),
      `${memory.peakInFlightBytes} bytes in flight at most`,
    );
    // Bound by its budget the whole way, the export gave back the idle memory once, not once a
    // frame: what is idle after that is the next frame's working set.
    assert.equal(log.released, 1);
    assert.equal(memory.idleBytesReleased, 1_000);
    assert.deepEqual(log.order, ["beginExport", "endExport", "sink.finish"]);
  },
);

test(
  "a container's defaults fill the codecs left out, and the result names the container",
  { skip },
  async () => {
    for (const [container, videoCodec] of [
      [undefined, "avc"],
      ["mov", "avc"],
      ["mkv", "avc"],
      ["webm", "vp9"],
    ]) {
      const { log, run } = scripted({ frames: 2 });
      const { result } = await quiet(() =>
        run({ options: container === undefined ? {} : { container } }),
      );
      const expected = container ?? "mp4";
      assert.equal(log.sinkOptions.container, expected);
      assert.equal(log.sinkOptions.videoCodec, videoCodec);
      assert.equal(log.outputRequest.container, expected);
      assert.equal(result.container, expected);
      assert.equal(result.videoCodec, videoCodec);
    }
  },
);

test(
  "a codec the container does not offer is refused before the output opens",
  { skip },
  async () => {
    for (const options of [
      { container: "webm", videoCodec: "avc" },
      { container: "mov", videoCodec: "av1" },
      { container: "mov", audioCodec: "opus" },
      { container: "avi" },
    ]) {
      const { log, run } = scripted({ frames: 2 });
      await assert.rejects(
        run({ options }),
        (error) =>
          error.failure?.code === "INVALID_INPUT" &&
          error.failure.phase === "prepare" &&
          !isRetryableFailure(error.failure),
        JSON.stringify(options),
      );
      assert.equal(log.outputRequest, undefined);
      assert.deepEqual(log.order, []);
    }
  },
);

test(
  "a frame no budget holds is refused before anything renders, and its output discarded",
  { skip },
  async () => {
    const { log, run } = scripted();
    await assert.rejects(
      run({ inFlightBytes: estimatedStagingBytes(WIDTH, HEIGHT) }),
      (error) =>
        error.failure?.code === "RESOURCE_LIMIT" &&
        error.failure.phase === "prepare",
    );
    assert.equal(log.submitted, 0);
    assert.deepEqual(log.order, ["output.abort"]);
  },
);

test(
  "a source that is still missing after four uploads stalls the export at its time",
  { skip },
  async () => {
    const { log, run } = scripted({
      ticket: (index, made) =>
        index === 3
          ? { kind: "waiting", timeTicks: 12_000n, missingSources: ["clip-a"] }
          : made,
    });
    let failure;
    await assert.rejects(quiet(run), (error) => {
      failure = error.failure;
      return /export stalled at 0\.100s waiting for clip-a/.test(
        failure?.message,
      );
    });
    // Classified as the SDK's runner classifies it, so the job does not retry it.
    assert.equal(failure.code, "RESOURCE_LIMIT");
    assert.equal(failure.phase, "upload");
    assert.equal(failure.stage, "sources-not-held");
    assert.equal(failure.timeTicks, "12000");
    assert.equal(
      decideRetry([
        {
          failure,
          profile: { name: "default", memoryScale: 1 },
          renderer: "gpu",
          videoEncoder: "default",
        },
      ]).retry,
      false,
    );
    assert.equal(log.supplied.length, 4);
    assert.deepEqual(log.supplied[0], [0.1, ["clip-a"]]);
  },
);

test(
  "an I420 frame whose planes do not fit its buffer fails the export before it is encoded",
  { skip },
  async () => {
    const { log, run } = scripted({
      readback: (index) =>
        step(
          index,
          index === 2
            ? [
                { offset: 0, stride: WIDTH },
                { offset: WIDTH * HEIGHT, stride: WIDTH },
                { offset: (WIDTH * HEIGHT * 5) / 4, stride: WIDTH },
              ]
            : undefined,
        ),
    });
    await assert.rejects(quiet(run), (error) =>
      /invalid I420 plane layout/.test(error.failure?.message),
    );
    assert.deepEqual(log.written, [0, 1]);
    // The file is discarded before the muxer is aborted, so it cannot commit a partial file.
    assert.deepEqual(log.order, [
      "beginExport",
      "endExport",
      "output.abort",
      "sink.abort",
    ]);
  },
);

test("abort during a pending write cancels", { skip }, async () => {
  const controller = new AbortController();
  const { log, run } = scripted({ frames: 30, holdWrites: true });
  const done = quiet(() => run({ signal: controller.signal }));
  await new Promise((resolve) => setTimeout(resolve, 20));
  const started = performance.now();
  controller.abort(new Error("user"));
  await assert.rejects(done, { message: "user" });
  assert.ok(
    performance.now() - started < 1_000,
    "the cancel did not wait for the held write",
  );
  assert.deepEqual(log.order, [
    "beginExport",
    "endExport",
    "output.abort",
    "sink.abort",
  ]);
});

test(
  "a cancel rejects with its reason and discards the output before the muxer",
  { skip },
  async () => {
    const controller = new AbortController();
    const { log, run } = scripted({
      frames: 30,
      onWrite: (log) => {
        if (log.written.length === 5) controller.abort(new Error("user"));
      },
    });
    await assert.rejects(
      quiet(() => run({ signal: controller.signal })),
      {
        message: "user",
      },
    );
    assert.ok(log.written.length < 30);
    assert.deepEqual(log.order, [
      "beginExport",
      "endExport",
      "output.abort",
      "sink.abort",
    ]);
  },
);

test(
  "an H.264 export where the browser refuses High opens Main and reports High as a fallback",
  { skip },
  async () => {
    encodes = ["avc1.4d", "vp09"];
    try {
      // An explicit rate of its own: mediabunny remembers each answer by its configuration.
      const { log, run } = scripted({ frames: 2 });
      const { result } = await quiet(() =>
        run({ options: { videoQuality: { bitrate: 777_000 } } }),
      );
      assert.equal(log.sinkOptions.videoCodecString, "avc1.4d0014");
      assert.equal(result.videoCodec, "avc");
      assert.equal(result.videoCodecString, "avc1.4d0014");
      assert.equal(result.videoCodecProfile, "Main");
      assert.deepEqual(result.videoEncoderFallbacks, [
        {
          encoder: "avc1.640014",
          profile: "High",
          reason: "unsupported",
          detail: `this browser does not encode H.264 High profile at ${WIDTH}x${HEIGHT}`,
        },
      ]);

      // VP9 has one profile: nothing to report, and the sink opens mediabunny's own string.
      const webm = scripted({ frames: 2 });
      const { result: vp9 } = await quiet(() =>
        webm.run({
          options: { container: "webm", videoQuality: { bitrate: 777_000 } },
        }),
      );
      assert.equal("videoCodecString" in webm.log.sinkOptions, false);
      assert.equal(vp9.videoCodecProfile, null);
      assert.deepEqual(vp9.videoEncoderFallbacks, []);
    } finally {
      encodes = null;
    }
  },
);

test(
  "an H.264 export the browser encodes in no profile is refused with the profiles it tried",
  { skip },
  async () => {
    encodes = ["vp09", "av01"];
    try {
      assert.equal(
        (
          await probeEncoders(WIDTH * 3, HEIGHT * 3, {
            frameRate: { num: 30, den: 1 },
          })
        ).avc,
        false,
      );
      const { run } = scripted({ frames: 2 });
      await assert.rejects(
        quiet(() => run({ options: { videoQuality: { bitrate: 555_000 } } })),
        (error) =>
          error.failure?.code === "INVALID_INPUT" &&
          error.failure.phase === "prepare" &&
          error.failure.message ===
            `This browser cannot encode H.264 (AVC) video at ${WIDTH}x${HEIGHT} in any profile (tried High avc1.64000d, Main avc1.4d000d, Constrained Baseline avc1.42e00d). Choose another video codec, or use the native exporter.` &&
          !isRetryableFailure(error.failure),
      );
    } finally {
      encodes = null;
    }
  },
);

test(
  "an export whose video codec the browser does not encode is refused before it renders, and not retried",
  {
    skip,
  },
  async () => {
    // Chrome on Linux: H.264, VP9 and AV1, no HEVC. Each case asks at its own size: mediabunny
    // remembers each answer by its configuration.
    encodes = ["avc1", "vp09", "av01"];
    try {
      assert.deepEqual(
        await probeEncoders(WIDTH * 2, HEIGHT * 2, {
          frameRate: { num: 30, den: 1 },
        }),
        { avc: true, hevc: false, vp9: true, av1: true },
      );
      let opened = false;
      let rendered = false;
      await assert.rejects(
        exportTimeline({
          supplyFrame: async () => {},
          renderFrame: () => {
            rendered = true;
            return { kind: "complete" };
          },
          engine: {},
          project: null,
          doc: {
            settings: { ...settings, width: WIDTH * 4, height: HEIGHT * 4 },
          },
          library: null,
          audioSupplier: null,
          range: { start: 0, end: 1 },
          options: { includeAudio: false, videoCodec: "hevc" },
          openOutput: async () => {
            opened = true;
            throw new Error("opened the output");
          },
        }),
        (error) =>
          error.failure?.code === "INVALID_INPUT" &&
          error.failure.phase === "prepare" &&
          /cannot encode H\.265 \(HEVC\) video at 256x144/.test(
            error.failure.message,
          ) &&
          !isRetryableFailure(error.failure),
      );
      assert.equal(opened, false);
      assert.equal(rendered, false);
    } finally {
      encodes = null;
    }
  },
);
