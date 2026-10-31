import test from "node:test";
import assert from "node:assert/strict";

import {
  BufferTarget,
  CustomVideoDecoder,
  EncodedPacket,
  EncodedVideoPacketSource,
  Mp4OutputFormat,
  Output,
  VideoSample,
  WebMOutputFormat,
  registerDecoder,
} from "mediabunny";

import { MediabunnyVideoDecoder } from "../dist/mediabunny.js";
import { PRESENTATION_SLACK_TICKS } from "../dist/media.js";

// 25 fps keeps every frame time on a whole millisecond, as Matroska stores it.
const FPS = 25;
const FRAMES = 90;
const GOP = 30;
const TICKS_PER_FRAME = 120_000n / BigInt(FPS);
const WIDTH = 16;
const HEIGHT = 8;
const FRAME_BYTES = WIDTH * HEIGHT * 4;

const decoder = { surfacesHeld: 0, decoded: 0, live: new Set() };

/** A frame built from another shares its surface; one built from bytes owns it. */
class TestVideoFrame {
  #closed = false;

  constructor(source, init = {}) {
    if (source instanceof TestVideoFrame) {
      this.surface = source.surface;
      this.format = source.format;
      this.codedWidth = source.codedWidth;
      this.codedHeight = source.codedHeight;
    } else {
      const bytes = new Uint8Array(
        source instanceof ArrayBuffer ? source : source.buffer,
      ).slice();
      this.surface = { fromDecoder: false, refs: 0, bytes };
      this.format = init.format;
      this.codedWidth = init.codedWidth;
      this.codedHeight = init.codedHeight;
    }
    this.timestamp = init.timestamp ?? source.timestamp;
    this.duration = init.duration ?? source.duration ?? null;
    this.displayWidth = this.codedWidth;
    this.displayHeight = this.codedHeight;
    this.visibleRect = {
      x: 0,
      y: 0,
      width: this.codedWidth,
      height: this.codedHeight,
    };
    this.colorSpace = {
      primaries: "bt709",
      transfer: "iec61966-2-1",
      matrix: "rgb",
      fullRange: true,
      toJSON() {
        return { ...this };
      },
    };
    this.surface.refs += 1;
    if (this.surface.fromDecoder && this.surface.refs === 1)
      decoder.surfacesHeld += 1;
  }

  get index() {
    return this.surface.bytes[0] | (this.surface.bytes[1] << 8);
  }

  clone() {
    return new TestVideoFrame(this);
  }

  allocationSize() {
    return this.surface.bytes.byteLength;
  }

  async copyTo(destination) {
    new Uint8Array(destination).set(this.surface.bytes);
    return [{ offset: 0, stride: this.codedWidth * 4 }];
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.surface.refs -= 1;
    if (this.surface.fromDecoder && this.surface.refs === 0) {
      decoder.surfacesHeld -= 1;
      decoder.live.delete(this.surface);
      this.surface.owner?.release();
    }
  }
}

/** Like Chromium's Windows hardware pool, a decoder with no free surface waits silently. */
const pool = { size: undefined };

/** Chromium's VA-API decoders can fail their first packet on Linux. */
const hardware = { fails: false, software: false, configs: [] };

class CountingDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return codec === "vp8";
  }
  #held = 0;
  #waiting = [];
  #drained = [];
  init() {}
  decode(packet) {
    hardware.configs.push(this.config.hardwareAcceleration);
    if (
      hardware.fails &&
      (hardware.software ||
        this.config.hardwareAcceleration !== "prefer-software")
    ) {
      this.onError(new DOMException("Decoding error.", "EncodingError"));
      return;
    }
    this.#waiting.push(packet);
    this.#output();
  }
  release() {
    this.#held -= 1;
    queueMicrotask(() => this.#output());
  }
  #output() {
    while (
      this.#waiting.length > 0 &&
      (pool.size === undefined || this.#held < pool.size)
    )
      this.#emit(this.#waiting.shift());
    if (this.#waiting.length === 0)
      for (const resolve of this.#drained.splice(0)) resolve();
  }
  #emit(packet) {
    decoder.decoded += 1;
    const bytes = new Uint8Array(FRAME_BYTES);
    bytes.set(packet.data.subarray(1, 3));
    const frame = new TestVideoFrame(
      { buffer: bytes.buffer, timestamp: 0 },
      {
        format: "RGBA",
        codedWidth: WIDTH,
        codedHeight: HEIGHT,
        timestamp: Math.round(packet.timestamp * 1e6),
        duration: Math.round(packet.duration * 1e6),
      },
    );
    frame.surface.fromDecoder = true;
    frame.surface.owner = this;
    this.#held += 1;
    decoder.surfacesHeld += 1;
    decoder.live.add(frame.surface);
    this.onSample(new VideoSample(frame));
  }
  flush() {
    if (this.#waiting.length === 0) return;
    return new Promise((resolve) => this.#drained.push(resolve));
  }
  close() {
    this.#waiting = [];
    for (const resolve of this.#drained.splice(0)) resolve();
  }
}

globalThis.VideoFrame = TestVideoFrame;
registerDecoder(CountingDecoder);

async function stream(start = 0, fps = FPS) {
  const output = new Output({
    format: new WebMOutputFormat(),
    target: new BufferTarget(),
  });
  const source = new EncodedVideoPacketSource("vp8");
  output.addVideoTrack(source, { frameRate: fps });
  await output.start();
  for (let index = start; index < FRAMES; index += 1) {
    const key = (index - start) % GOP === 0;
    // A VP8 frame header's low bit is zero for a keyframe.
    const data = new Uint8Array([key ? 0 : 1, index & 0xff, index >> 8, 0]);
    await source.add(
      new EncodedPacket(data, key ? "key" : "delta", index / fps, 1 / fps),
      index === start
        ? {
            decoderConfig: {
              codec: "vp8",
              codedWidth: WIDTH,
              codedHeight: HEIGHT,
            },
          }
        : undefined,
    );
  }
  await output.finalize();
  return new Blob([output.target.buffer], { type: "video/webm" });
}

const blob = await stream();

async function open(options, resource = blob) {
  decoder.decoded = 0;
  return MediabunnyVideoDecoder.open("clip", resource, options);
}

async function presented(video, frame) {
  const decoded = await video.refill({
    sourceId: "clip",
    mediaTimeTicks: BigInt(frame) * TICKS_PER_FRAME,
    reason: "prefetch",
    generation: 0,
  });
  const index = decoded.frame.index;
  assert.equal(decoded.decodedTimeTicks, BigInt(index) * TICKS_PER_FRAME);
  decoded.frame.close();
  return index;
}

async function mirroredMp4() {
  const output = new Output({
    format: new Mp4OutputFormat(),
    target: new BufferTarget(),
  });
  const source = new EncodedVideoPacketSource("vp8");
  output.addVideoTrack(source, { frameRate: FPS });
  await output.start();
  for (let index = 0; index < 3; index += 1)
    await source.add(
      new EncodedPacket(
        new Uint8Array([index === 0 ? 0 : 1, index, 0, 0]),
        index === 0 ? "key" : "delta",
        index / FPS,
        1 / FPS,
      ),
      index === 0
        ? {
            decoderConfig: {
              codec: "vp8",
              codedWidth: WIDTH,
              codedHeight: HEIGHT,
            },
          }
        : undefined,
    );
  await output.finalize();
  const bytes = new Uint8Array(output.target.buffer);
  const tkhd = Buffer.from(bytes).indexOf("tkhd");
  assert.ok(tkhd > 0);
  // Version 0: the matrix follows 40 bytes of the box's fields.
  const view = new DataView(bytes.buffer);
  assert.equal(view.getInt32(tkhd + 4 + 40), 0x10000);
  view.setInt32(tkhd + 4 + 40, -0x10000);
  return new Blob([bytes], { type: "video/mp4" });
}

test("a track whose matrix mirrors is answered mirrored, not turned upside down", async () => {
  const video = await open({}, await mirroredMp4());
  try {
    const decoded = await video.refill({
      sourceId: "clip",
      mediaTimeTicks: 0n,
      reason: "prefetch",
      generation: 0,
    });
    decoded.frame.close();
    assert.equal(decoded.rotationDegrees, 0);
    assert.equal(decoded.mirror, true);
  } finally {
    video.close();
  }
});

test("a reverse run plays from windows of copies, decoding each interval a few times", async () => {
  const video = await open({ maxReverseFrameBytes: 10 * FRAME_BYTES });
  try {
    for (let frame = FRAMES - 1; frame >= 0; frame -= 1) {
      assert.equal(await presented(video, frame), frame);
      assert.ok(
        decoder.surfacesHeld <= 16,
        `${decoder.surfacesHeld} at ${frame}`,
      );
    }
    assert.ok(decoder.decoded < 300, `${decoder.decoded} packets decoded`);
    assert.equal(
      video.reverseCacheHits + video.reverseWindowsBuilt,
      FRAMES - 1,
      `${video.reverseCacheHits} hits, ${video.reverseWindowsBuilt} windows`,
    );
    assert.ok(video.reverseWindowsBuilt <= 10, `${video.reverseWindowsBuilt}`);
    assert.ok(
      decoder.surfacesHeld <= 9,
      `${decoder.surfacesHeld} decoder surfaces held: ${[...decoder.live].map((surface) => surface.bytes[0])}`,
    );
  } finally {
    video.close();
  }
  assert.equal(decoder.surfacesHeld, 0, "closing releases every surface");
});

test("a loop's wrap is a seek, and forward play continues from it", async () => {
  const video = await open();
  try {
    for (let frame = 50; frame < 60; frame += 1)
      assert.equal(await presented(video, frame), frame);
    for (const frame of [0, 1, 2, 3])
      assert.equal(await presented(video, frame), frame);
    assert.equal(video.reverseWindowsBuilt, 0);
  } finally {
    video.close();
  }
});

test("a ping-pong turn plays back from the window it has", async () => {
  const video = await open();
  try {
    for (let frame = 40; frame >= 20; frame -= 1)
      assert.equal(await presented(video, frame), frame);
    const built = video.reverseWindowsBuilt;
    const hits = video.reverseCacheHits;
    for (let frame = 21; frame <= 25; frame += 1)
      assert.equal(await presented(video, frame), frame);
    assert.equal(video.reverseWindowsBuilt, built);
    assert.equal(video.reverseCacheHits, hits + 5);
    assert.equal(await presented(video, 45), 45);
  } finally {
    video.close();
  }
});

test("scrubbing back in long jumps seeks instead of filling reverse windows", async () => {
  const video = await open();
  try {
    for (const frame of [89, 30, 29, 28])
      assert.equal(await presented(video, frame), frame);
    assert.equal(
      video.reverseWindowsBuilt,
      0,
      "one step in, and then the frame before it from the forward window",
    );
    for (const frame of [80, 20])
      assert.equal(await presented(video, frame), frame);
    assert.equal(video.reverseWindowsBuilt, 0);
  } finally {
    video.close();
  }
});

test("past the stream's end the last frame answers, forward and backward", async () => {
  const video = await open();
  try {
    assert.equal(await presented(video, 120), FRAMES - 1);
    for (let frame = 95; frame >= 80; frame -= 1)
      assert.equal(await presented(video, frame), Math.min(frame, FRAMES - 1));
  } finally {
    video.close();
  }
});

test("a picture held past the stream's end decodes nothing more once the end was reached", async () => {
  const video = await open();
  try {
    for (let frame = 0; frame < FRAMES; frame += 1)
      assert.equal(await presented(video, frame), frame);
    const decodes = [];
    for (let frame = FRAMES; frame < FRAMES + 10; frame += 1) {
      const before = decoder.decoded;
      assert.equal(await presented(video, frame), FRAMES - 1);
      decodes.push(decoder.decoded - before);
    }
    assert.deepEqual(decodes, new Array(10).fill(0));
  } finally {
    video.close();
  }
});

test("a decoder with a fixed pool of surfaces plays forward and seeks without running dry", async () => {
  // Chromium's Windows decoder leaves about five spare surfaces.
  pool.size = 5;
  const video = await open({ stallTimeoutMs: 1_000 });
  try {
    for (let frame = 0; frame < FRAMES; frame += 1)
      assert.equal(await presented(video, frame), frame);
    for (const frame of [10, 70, 45, 44])
      assert.equal(await presented(video, frame), frame);
  } finally {
    video.close();
    pool.size = undefined;
  }
  assert.equal(decoder.surfacesHeld, 0);
});

test("a decoder that runs out of surfaces fails the request, and a seek after it decodes", async () => {
  pool.size = 3;
  const video = await open({ stallTimeoutMs: 50 });
  const held = [];
  try {
    const refill = (frame) =>
      video.refill({
        sourceId: "clip",
        mediaTimeTicks: BigInt(frame) * TICKS_PER_FRAME,
        reason: "prefetch",
        generation: 0,
      });
    await assert.rejects(async () => {
      for (let frame = 0; frame < FRAMES; frame += 1)
        held.push((await refill(frame)).frame);
    }, /stalled/);
    for (const frame of held.splice(0)) frame.close();
    assert.equal(await presented(video, 40), 40);
    assert.equal(await presented(video, 41), 41);
  } finally {
    for (const frame of held) frame.close();
    video.close();
    pool.size = undefined;
  }
  assert.equal(decoder.surfacesHeld, 0, "a stalled read releases what it held");
});

test("a decoder that fails in hardware is replaced once by a software decoder", async () => {
  hardware.fails = true;
  hardware.configs = [];
  const video = await open();
  try {
    assert.equal(await presented(video, 40), 40);
    assert.equal(await presented(video, 41), 41);
    assert.equal(video.softwareFallback, "EncodingError: Decoding error.");
    assert.equal(hardware.configs[0], "no-preference");
    assert.ok(hardware.configs.slice(1).every((c) => c === "prefer-software"));
  } finally {
    video.close();
    hardware.fails = false;
  }
  assert.equal(decoder.surfacesHeld, 0);
});

test("a decoder that fails in software too rejects the request", async () => {
  hardware.fails = true;
  hardware.software = true;
  hardware.configs = [];
  const video = await open();
  try {
    await assert.rejects(
      video.refill({
        sourceId: "clip",
        mediaTimeTicks: 0n,
        reason: "prefetch",
        generation: 0,
      }),
      (error) => {
        assert.equal(error.code, "DECODE_UNAVAILABLE");
        assert.equal(error.cause.name, "EncodingError");
        assert.equal(error.facts.kind, "video");
        return true;
      },
    );
    assert.deepEqual(
      [...new Set(hardware.configs)],
      ["no-preference", "prefer-software"],
    );
  } finally {
    video.close();
    hardware.fails = false;
    hardware.software = false;
  }
});

async function presentedAtTicks(video, ticks) {
  const decoded = await video.refill({
    sourceId: "clip",
    mediaTimeTicks: ticks,
    reason: "prefetch",
    generation: 0,
  });
  const index = decoded.frame.index;
  assert.equal(decoded.decodedTimeTicks, BigInt(index) * TICKS_PER_FRAME);
  decoded.frame.close();
  return index;
}

test("requests before a late stream's first frame all answer with it, and then play on in place", async () => {
  const late = await stream(3);
  const video = await open({}, late);
  try {
    const answered = [];
    for (let frame = 0; frame < 12; frame += 1)
      answered.push(await presented(video, frame));
    assert.deepEqual(answered, [3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
  } finally {
    video.close();
  }
  const doubled = await open({}, late);
  try {
    const answered = [];
    for (let step = 0; step < 16; step += 1)
      answered.push(
        await presentedAtTicks(doubled, (BigInt(step) * TICKS_PER_FRAME) / 2n),
      );
    assert.deepEqual(
      answered,
      [3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7],
    );
  } finally {
    doubled.close();
  }
});

test("forward play after a jump back into a reverse window continues from there, not from where play stopped", async () => {
  const video = await open();
  try {
    for (const frame of [60, 59, 58, 57])
      assert.equal(await presented(video, frame), frame);
    for (let frame = 58; frame <= 80; frame += 1)
      assert.equal(await presented(video, frame), frame);
    for (let frame = 50; frame <= 57; frame += 1)
      assert.equal(await presented(video, frame), frame);
    for (let frame = 58; frame <= 61; frame += 1)
      assert.equal(await presented(video, frame), frame);
  } finally {
    video.close();
  }
});

test("a frame up to the presentation slack after a request is the one it shows, forward and in a reverse window", async () => {
  // WebM's millisecond time base takes the most slack.
  const video = await open();
  try {
    for (let frame = 20; frame < 26; frame += 1) {
      const at = BigInt(frame) * TICKS_PER_FRAME;
      assert.equal(
        await presentedAtTicks(video, at - PRESENTATION_SLACK_TICKS),
        frame,
      );
    }
    for (let frame = 30; frame > 24; frame -= 1) {
      const at = BigInt(frame) * TICKS_PER_FRAME;
      assert.equal(
        await presentedAtTicks(video, at - PRESENTATION_SLACK_TICKS),
        frame,
      );
      assert.equal(
        await presentedAtTicks(video, at - PRESENTATION_SLACK_TICKS - 1n),
        frame - 1,
      );
    }
  } finally {
    video.close();
  }
});

test("a reverse window keeps a frame stored up to the presentation slack after its request, so a backward run decodes each interval once per window", async () => {
  // Matroska's milliseconds put two of every three 30 fps frames a tick late.
  for (const fps of [30, 25, 24, 60]) {
    const ticksPerFrame = 120_000 / fps;
    const video = await open(
      { maxReverseFrameBytes: 6 * FRAME_BYTES },
      await stream(0, fps),
    );
    try {
      const at = async (frame) => {
        const decoded = await video.refill({
          sourceId: "clip",
          mediaTimeTicks: BigInt(Math.round(frame * ticksPerFrame)),
          reason: "prefetch",
          generation: 0,
        });
        const index = decoded.frame.index;
        decoded.frame.close();
        return index;
      };
      assert.equal(await at(FRAMES - 1), FRAMES - 1);
      decoder.decoded = 0;
      const wrong = [];
      for (let frame = FRAMES - 2; frame >= 0; frame -= 1) {
        const index = await at(frame);
        if (index !== frame) wrong.push(`${frame}->${index}`);
      }
      assert.deepEqual(wrong, [], `${fps} fps`);
      assert.equal(decoder.decoded, 275, `${fps} fps packets decoded`);
    } finally {
      video.close();
    }
  }
});

test("frames too large for two in the byte limit still answer between frames with the frame at or before", async () => {
  const video = await open({ maxCachedFrameBytes: (FRAME_BYTES * 3) / 2 });
  try {
    const half = TICKS_PER_FRAME / 2n;
    for (let frame = 10; frame < 16; frame += 1)
      assert.equal(
        await presentedAtTicks(video, BigInt(frame) * TICKS_PER_FRAME + half),
        frame,
      );
    for (let step = 0; step < 8; step += 1)
      assert.equal(
        await presentedAtTicks(
          video,
          16n * TICKS_PER_FRAME + (BigInt(step) * TICKS_PER_FRAME) / 4n,
        ),
        16 + Math.floor(step / 4),
      );
  } finally {
    video.close();
  }
});

test("a stall timeout longer than a timer can hold is refused before the input is read", async () => {
  await assert.rejects(
    MediabunnyVideoDecoder.open("video", new ArrayBuffer(0), {
      stallTimeoutMs: Number.MAX_SAFE_INTEGER,
    }),
    (error) =>
      error instanceof RangeError &&
      /stallTimeoutMs .*2147483647/.test(error.message),
  );
});

test("a software decoder asked for is checked when the track opens", async (t) => {
  const asked = [];
  let supported = false;
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = {
    async isConfigSupported(config) {
      asked.push(config);
      return { supported, config };
    },
  };
  t.after(() => {
    globalThis.VideoDecoder = original;
  });
  await assert.rejects(open({ hardwareAcceleration: "prefer-software" }), {
    code: "software_decode_unavailable",
    message: /no software decoder for vp8/,
  });
  assert.equal(asked.length, 1);
  assert.equal(asked[0].codec, "vp8");
  assert.equal(asked[0].hardwareAcceleration, "prefer-software");
  supported = true;
  (await open({ hardwareAcceleration: "prefer-software" })).close();
  (await open()).close();
  assert.equal(asked.length, 2);
});
