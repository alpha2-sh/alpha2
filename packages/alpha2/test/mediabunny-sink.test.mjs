// MediabunnySink as the SDK's runners use it: a stream destination is committed only when the
// file finished, and two runners can share one file.
import assert from "node:assert/strict";
import test from "node:test";

import {
  ALL_FORMATS,
  BufferSource,
  CustomAudioEncoder,
  CustomVideoEncoder,
  EncodedPacket,
  Input,
  registerEncoder,
} from "mediabunny";

import { renderOfflineAudio } from "../dist/audio.js";
import { WritableFileDestination } from "../dist/export-destination.js";
import { Alpha2Exporter, exportToSink } from "../dist/exporter.js";
import { MediabunnySink } from "../dist/mediabunny.js";

/** Enough of a `VideoFrame` for mediabunny to wrap it in a sample. */
class TestVideoFrame {
  format = "RGBX";
  codedWidth = 2;
  codedHeight = 2;
  displayWidth = 2;
  displayHeight = 2;
  timestamp = 0;
  duration = null;
  colorSpace = {};
  visibleRect = { x: 0, y: 0, width: 2, height: 2 };
  allocationSize() {
    return 16;
  }
  async copyTo() {
    return [{ offset: 0, stride: 8 }];
  }
  clone() {
    return new TestVideoFrame();
  }
  close() {}
}

/**
 * How the next video encoder misbehaves: `failFlush` fails its flush; `stall` puts nothing out and
 * holds its encodes or its flush for `STALL_MS`; `delayMs` delays each frame. `configs` records
 * each encoder's configuration.
 */
const encoders = { failFlush: false, stall: null, delayMs: 0, configs: [] };
const STALL_MS = 500;

const AVC_CONFIG = new Uint8Array([
  1, 0x64, 0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1e, 1, 0, 2, 0x68, 0xce,
]);

/** Encodes each sample as a three-byte keyframe at the sample's own time. */
class StampEncoder extends CustomVideoEncoder {
  static supports() {
    return true;
  }
  init() {
    this.configured = false;
    encoders.configs.push(this.config);
  }
  encode(sample) {
    if (encoders.stall === "encode")
      return new Promise((resolve) => setTimeout(resolve, STALL_MS));
    if (encoders.stall === "flush") return;
    if (encoders.delayMs > 0) {
      const timestamp = sample.timestamp;
      const duration = sample.duration;
      return new Promise((resolve) =>
        setTimeout(() => {
          this.put(timestamp, duration);
          resolve();
        }, encoders.delayMs),
      );
    }
    this.put(sample.timestamp, sample.duration);
  }
  put(timestamp, duration) {
    const meta = this.configured
      ? undefined
      : {
          decoderConfig: {
            codec: this.config.codec,
            codedWidth: 2,
            codedHeight: 2,
            // Without an AVCDecoderConfigurationRecord the muxer takes an AVC track to be Annex B,
            // so give it one with a stub SPS and PPS.
            ...(this.codec === "avc" ? { description: AVC_CONFIG } : {}),
          },
        };
    this.configured = true;
    this.onPacket(
      new EncodedPacket(new Uint8Array([1, 2, 3]), "key", timestamp, duration),
      meta,
    );
  }
  flush() {
    if (encoders.failFlush) throw new Error("encoder flush failed");
    if (encoders.stall === "flush")
      return new Promise((resolve) => setTimeout(resolve, STALL_MS));
  }
  close() {}
}

/** Encodes each audio sample as one three-byte packet spanning it. */
class StampAudioEncoder extends CustomAudioEncoder {
  static supports() {
    return true;
  }
  init() {
    this.configured = false;
  }
  encode(sample) {
    const meta = this.configured
      ? undefined
      : {
          decoderConfig: {
            codec: this.config.codec,
            numberOfChannels: this.config.numberOfChannels,
            sampleRate: this.config.sampleRate,
            // Without an AudioSpecificConfig the muxer takes an AAC track to be ADTS, so give it
            // one: AAC-LC, 48 kHz, stereo.
            ...(this.codec === "aac"
              ? { description: new Uint8Array([0x11, 0x90]) }
              : {}),
          },
        };
    this.configured = true;
    this.onPacket(
      new EncodedPacket(
        new Uint8Array([4, 5, 6]),
        "key",
        sample.timestamp,
        sample.duration,
      ),
      meta,
    );
  }
  flush() {}
  close() {}
}

globalThis.VideoFrame = TestVideoFrame;
registerEncoder(StampEncoder);
registerEncoder(StampAudioEncoder);

/**
 * A file like a `FileSystemWritableFileStream`: writes go to a swap copy, `close` commits it over
 * the file, and `abort` discards it.
 */
function swapFile() {
  const file = {
    committed: null,
    aborted: false,
    swap: new Uint8Array(0),
    async write({ data, position }) {
      const end = position + data.byteLength;
      if (end > file.swap.byteLength) {
        const grown = new Uint8Array(end);
        grown.set(file.swap);
        file.swap = grown;
      }
      file.swap.set(data, position);
    },
    async close() {
      file.committed = file.swap;
    },
    async abort() {
      file.aborted = true;
    },
  };
  return file;
}

/**
 * Renders `frames` frames at 30 fps; frame `failAt` fails its readback, and `onBegin` sees each
 * frame's index as it starts.
 */
function exporter(frames, failAt, onBegin = () => {}) {
  let index = 0;
  return new Alpha2Exporter(
    {
      begin: async () => {
        const current = index++;
        onBegin(current);
        return {
          kind: "rendered",
          retainedBytes: 16,
          readback:
            current === failAt
              ? Promise.reject(new Error("GPU readback map failed"))
              : Promise.resolve(new TestVideoFrame()),
        };
      },
      isDeviceLost: () => false,
      abort() {},
    },
    { num: 30n, den: 1n },
    0n,
    BigInt(frames) * 4_000n,
  );
}

function streamSink(file, tracks = { video: true }, options = {}) {
  return new MediabunnySink({
    container: "mp4",
    ...options,
    ...(tracks.video
      ? {
          videoCodec: "vp9",
          videoRateControl: { bitrate: 1_000_000, bitrateMode: "variable" },
          frameRate: { num: 30, den: 1 },
        }
      : {}),
    ...(tracks.audio ? { audioCodec: "opus", audioBitrate: 128_000 } : {}),
    destination: {
      kind: "stream",
      writable: new WritableFileDestination(file).writable,
    },
  });
}

async function tracksOf(bytes) {
  const input = new Input({
    source: new BufferSource(bytes),
    formats: ALL_FORMATS,
  });
  try {
    const tracks = await input.getTracks();
    return Promise.all(
      tracks.map(async (track) => [
        track.type,
        Math.round((await track.computeDuration()) * 10) / 10,
      ]),
    );
  } finally {
    input.dispose();
  }
}

test("a finished export commits the file", async () => {
  const file = swapFile();
  const sink = streamSink(file);
  await exportToSink(exporter(30), sink, async () => {});
  assert.equal(file.aborted, false);
  assert.ok(file.committed !== null);
  assert.deepEqual(await tracksOf(file.committed), [["video", 1]]);
  assert.deepEqual(sink.output, {
    kind: "stream",
    bytes: file.committed.byteLength,
  });
});

test("an export that fails discards the file it was writing", async () => {
  const file = swapFile();
  await assert.rejects(
    exportToSink(exporter(30, 20), streamSink(file), async () => {}),
    /GPU readback map failed/,
  );
  assert.equal(file.committed, null, "nothing was committed");
  assert.equal(file.aborted, true);
});

test("a cancelled export discards the file it was writing", async () => {
  const file = swapFile();
  const controller = new AbortController();
  const cancelAt = (index) => {
    if (index === 10) controller.abort();
  };
  await assert.rejects(
    exportToSink(exporter(30, -1, cancelAt), streamSink(file), async () => {}, {
      signal: controller.signal,
    }),
    (error) => error.name === "ExportAbortedError",
  );
  assert.equal(file.committed, null);
  assert.equal(file.aborted, true);
});

test("a file whose finishing fails is discarded", async () => {
  const file = swapFile();
  encoders.failFlush = true;
  try {
    await assert.rejects(
      exportToSink(exporter(10), streamSink(file), async () => {}),
      /encoder flush failed/,
    );
  } finally {
    encoders.failFlush = false;
  }
  assert.equal(file.committed, null);
  assert.equal(file.aborted, true);
});

/** One second of stereo silence at 48 kHz, rendered in blocks of 4800 frames. */
function audioRun(track, render = undefined) {
  return renderOfflineAudio(
    48_000n,
    [4_800],
    render ??
      (async (startSample, frames) => ({
        sourceId: "mix",
        startSample,
        generation: 0,
        sampleRate: 48_000,
        channels: [new Float32Array(frames), new Float32Array(frames)],
      })),
    track,
  );
}

test("two runners share one file through its tracks, at once or in turn", async () => {
  for (const order of ["together", "video first", "audio first"]) {
    const file = swapFile();
    const sink = streamSink(file, { video: true, audio: true });
    const video = () =>
      exportToSink(exporter(30), sink.videoTrack, async () => {});
    const audio = () => audioRun(sink.audioTrack);
    if (order === "together") await Promise.all([video(), audio()]);
    else if (order === "video first") {
      await video();
      assert.equal(file.committed, null, "the file waits for the audio");
      await audio();
    } else {
      await audio();
      await video();
    }
    assert.ok(file.committed !== null, order);
    assert.deepEqual(
      (await tracksOf(file.committed)).sort(),
      [
        ["audio", 1],
        ["video", 1],
      ],
      order,
    );
  }
});

test("a runner that fails discards the file the other runner shares", async () => {
  const file = swapFile();
  const sink = streamSink(file, { video: true, audio: true });
  let release;
  const held = new Promise((resolve) => (release = resolve));
  const results = await Promise.allSettled([
    exportToSink(exporter(30, 5), sink.videoTrack, async () => {}).finally(
      release,
    ),
    audioRun(sink.audioTrack, async (startSample, frames) => {
      if (startSample > 0n) await held;
      return {
        sourceId: "mix",
        startSample,
        generation: 0,
        sampleRate: 48_000,
        channels: [new Float32Array(frames), new Float32Array(frames)],
      };
    }),
  ]);
  assert.deepEqual(
    results.map(({ status }) => status),
    ["rejected", "rejected"],
  );
  assert.match(String(results[0].reason), /GPU readback map failed/);
  assert.equal(file.committed, null);
  assert.equal(file.aborted, true);
});

test("each container writes its own file through a stream destination", async () => {
  const cases = [
    { container: "mp4", video: "vp9", audio: "opus", format: "MP4" },
    {
      container: "mov",
      video: "avc",
      audio: "aac",
      format: "QuickTime File Format",
    },
    { container: "mkv", video: "avc", audio: "aac", format: "Matroska" },
    { container: "webm", video: "vp9", audio: "opus", format: "WebM" },
  ];
  for (const { container, video, audio, format } of cases) {
    const file = swapFile();
    const sink = new MediabunnySink({
      container,
      videoCodec: video,
      videoRateControl: { bitrate: 1_000_000, bitrateMode: "variable" },
      frameRate: { num: 30, den: 1 },
      audioCodec: audio,
      audioBitrate: 128_000,
      destination: {
        kind: "stream",
        writable: new WritableFileDestination(file).writable,
      },
    });
    await Promise.all([
      exportToSink(exporter(30), sink.videoTrack, async () => {}),
      audioRun(sink.audioTrack),
    ]);
    assert.ok(file.committed !== null, container);
    const input = new Input({
      source: new BufferSource(file.committed),
      formats: ALL_FORMATS,
    });
    try {
      assert.equal((await input.getFormat()).name, format, container);
      const codecs = (await input.getTracks()).map((track) => track.codec);
      assert.deepEqual(codecs.sort(), [audio, video].sort(), container);
    } finally {
      input.dispose();
    }
  }
});

test("a codec the container does not offer is refused before the destination is touched", () => {
  for (const [container, tracks, message] of [
    ["webm", { videoCodec: "avc" }, /webm does not offer video codec h264/],
    ["mov", { audioCodec: "opus" }, /mov does not offer audio codec opus/],
    ["mov", { videoCodec: "vp9" }, /mov does not offer video codec vp9/],
  ]) {
    const writable = new WritableFileDestination(swapFile()).writable;
    assert.throws(
      () =>
        new MediabunnySink({
          container,
          ...tracks,
          videoRateControl: { bitrate: 1_000_000, bitrateMode: "variable" },
          audioBitrate: 128_000,
          destination: { kind: "stream", writable },
        }),
      (error) =>
        error instanceof RangeError &&
        error.name === "ExportFormatError" &&
        message.test(error.message),
    );
    assert.equal(writable.locked, false, container);
  }
  assert.throws(
    () =>
      new MediabunnySink({
        container: "avi",
        audioCodec: "aac",
        audioBitrate: 128_000,
        destination: { kind: "memory", maxBytes: 1 << 20 },
      }),
    /unknown container "avi"/,
  );
});

test("a track that is not configured has no sink", () => {
  const sink = streamSink(swapFile(), { video: true });
  assert.throws(() => sink.audioTrack, RangeError);
  assert.equal(sink.videoTrack, sink.videoTrack);
});

/** The child boxes of an ISO BMFF box body `bytes[start, end)`, by type. */
function childBoxes(bytes, start = 0, end = bytes.byteLength) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const boxes = new Map();
  for (let at = start; at + 8 <= end;) {
    const size = view.getUint32(at);
    const type = String.fromCharCode(...bytes.subarray(at + 4, at + 8));
    assert.ok(size >= 8 && at + size <= end, `box ${type} fits its parent`);
    boxes.set(type, { start: at + 8, end: at + size });
    at += size;
  }
  return boxes;
}

/** The video track's media timescale and its stts entries `[sampleCount, sampleDelta]`. */
function videoTiming(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const inside = (box, type) => {
    const child = childBoxes(bytes, box.start, box.end).get(type);
    assert.ok(child, `${type} is present`);
    return child;
  };
  const moov = childBoxes(bytes).get("moov");
  const mdia = inside(inside(moov, "trak"), "mdia");
  const mdhd = inside(mdia, "mdhd");
  // Version 1 widens the creation and modification times to 64 bits.
  const timescale = view.getUint32(
    mdhd.start + (bytes[mdhd.start] === 1 ? 20 : 12),
  );
  const stts = inside(inside(inside(mdia, "minf"), "stbl"), "stts");
  const entries = [];
  const count = view.getUint32(stts.start + 4);
  for (let index = 0; index < count; index += 1) {
    const at = stts.start + 8 + index * 8;
    entries.push([view.getUint32(at), view.getUint32(at + 4)]);
  }
  return { timescale, entries };
}

test("the sink's video track keeps one sample duration at every rate it declares, NTSC included", async () => {
  // Without the declared rate the muxer derives durations on a 1/57600 s grid, where 29.97 fps
  // alternates 1922 and 1921 units and judders; 30 fps fits that grid.
  for (const { num, den } of [
    { num: 30000, den: 1001 },
    { num: 24000, den: 1001 },
    { num: 60000, den: 1001 },
    { num: 30, den: 1 },
  ]) {
    const frames = 600;
    const sink = new MediabunnySink({
      container: "mp4",
      videoCodec: "vp9",
      videoRateControl: { bitrate: 1_000_000, bitrateMode: "variable" },
      frameRate: { num, den },
      destination: { kind: "memory", maxBytes: 1 << 20 },
    });
    const exporter = new Alpha2Exporter(
      {
        begin: async () => ({
          kind: "rendered",
          retainedBytes: 16,
          readback: Promise.resolve(new TestVideoFrame()),
        }),
        isDeviceLost: () => false,
        abort() {},
      },
      { num: BigInt(num), den: BigInt(den) },
      0n,
      (BigInt(frames) * 120_000n * BigInt(den)) / BigInt(num),
    );
    await exportToSink(exporter, sink, async () => {});
    const { timescale, entries } = videoTiming(
      new Uint8Array(sink.output.buffer),
    );
    assert.equal(
      entries.length,
      1,
      `${num}/${den}: ${JSON.stringify(entries)}`,
    );
    const [[samples, delta]] = entries;
    assert.equal(samples, frames, `${num}/${den} holds every frame`);
    assert.equal(
      delta * num,
      timescale * den,
      `${num}/${den}: each sample lasts one frame (${delta}/${timescale} s)`,
    );
  }
});

/** Runs `run` with the encoders set as `settings` say. */
async function withEncoders(settings, run) {
  Object.assign(encoders, settings);
  try {
    return await run();
  } finally {
    Object.assign(encoders, { failFlush: false, stall: null, delayMs: 0 });
  }
}

test("an encoder that holds its frames without putting one out fails the write, not the export forever", async () => {
  const file = swapFile();
  const sink = streamSink(file, { video: true }, { stallTimeoutMs: 50 });
  const failure = await withEncoders({ stall: "encode" }, () =>
    exportToSink(exporter(30), sink, async () => {}).then(
      () => assert.fail("the export finished"),
      (error) => error,
    ),
  );
  assert.equal(
    failure.cause?.code ?? failure.code,
    "ENCODE_STALLED",
    String(failure),
  );
  assert.match(
    String(failure.cause ?? failure),
    /video encoder stalled: no output for \d+ ms with 4 frames queued/,
  );
  assert.equal(file.committed, null);
  assert.equal(file.aborted, true);
});

test("an encoder whose flush puts nothing out fails the finish", async () => {
  const file = swapFile();
  const sink = streamSink(file, { video: true }, { stallTimeoutMs: 50 });
  const failure = await withEncoders({ stall: "flush" }, () =>
    exportToSink(exporter(3), sink, async () => {}).then(
      () => assert.fail("the export finished"),
      (error) => error,
    ),
  );
  const stalled = failure.cause ?? failure;
  assert.equal(stalled.code, "ENCODE_STALLED", String(failure));
  assert.deepEqual(stalled.failure, {
    code: "GPU_TIMEOUT",
    phase: "encode",
    stage: "encoder-stalled",
    resource: "unknown",
  });
  assert.equal(file.committed, null);
  assert.equal(file.aborted, true);
});

test("a slow encoder that keeps putting frames out is not taken to be stalled", async () => {
  const file = swapFile();
  // The whole export (12 x 30 ms) outlasts the timeout, while each output lands well inside it,
  // even on a loaded machine's coarse timers.
  const sink = streamSink(file, { video: true }, { stallTimeoutMs: 150 });
  await withEncoders({ delayMs: 30 }, () =>
    exportToSink(exporter(12), sink, async () => {}),
  );
  assert.ok(file.committed !== null);
  assert.deepEqual(await tracksOf(file.committed), [["video", 0.4]]);
});

test("the video encoder opens in the latency mode the sink is given, else in the measured one", async () => {
  for (const given of [undefined, "realtime", "quality"]) {
    encoders.configs = [];
    const sink = streamSink(
      swapFile(),
      { video: true },
      given === undefined ? {} : { videoLatencyMode: given },
    );
    await exportToSink(exporter(2), sink, async () => {});
    const mode = given ?? "quality";
    assert.equal(sink.videoLatencyMode, mode);
    assert.deepEqual(
      encoders.configs.map((config) => config.latencyMode),
      [mode],
    );
  }
});

test("a stall timeout out of range is refused at construction", () => {
  for (const stallTimeoutMs of [0, -1, 1.5, 2 ** 31])
    assert.throws(
      () => streamSink(swapFile(), { video: true }, { stallTimeoutMs }),
      RangeError,
    );
});
