// A custom decoder registered with mediabunny stands for `VideoDecoder` and counts live instances
// and decoded surfaces not yet closed.
import test from "node:test";
import assert from "node:assert/strict";

import {
  BufferTarget,
  CustomVideoDecoder,
  EncodedPacket,
  EncodedVideoPacketSource,
  Output,
  VideoSample,
  WebMOutputFormat,
  registerDecoder,
} from "mediabunny";

import { MediabunnyVideoDecoder } from "../dist/mediabunny.js";
import {
  DEFAULT_MAX_DECODER_CURSORS,
  DecoderRefillCoordinator,
} from "../dist/media.js";

const FPS = 25;
const FRAMES = 90;
const GOP = 30;
const TICKS_PER_FRAME = 120_000n / BigInt(FPS);
const WIDTH = 16;
const HEIGHT = 8;
const FRAME_BYTES = WIDTH * HEIGHT * 4;

const counters = { surfacesHeld: 0, decodersLive: 0, decodersMade: 0 };

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
      counters.surfacesHeld += 1;
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
    if (this.surface.fromDecoder && this.surface.refs === 0)
      counters.surfacesHeld -= 1;
  }
}

/** Stands for a browser `VideoDecoder`: counted from `init` to `close`. */
class CountingDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return codec === "vp8";
  }
  init() {
    counters.decodersMade += 1;
    counters.decodersLive += 1;
  }
  decode(packet) {
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
    counters.surfacesHeld += 1;
    this.onSample(new VideoSample(frame));
  }
  flush() {}
  close() {
    counters.decodersLive -= 1;
  }
}

globalThis.VideoFrame = TestVideoFrame;
registerDecoder(CountingDecoder);

async function stream() {
  const output = new Output({
    format: new WebMOutputFormat(),
    target: new BufferTarget(),
  });
  const source = new EncodedVideoPacketSource("vp8");
  output.addVideoTrack(source, { frameRate: FPS });
  await output.start();
  for (let index = 0; index < FRAMES; index += 1) {
    const key = index % GOP === 0;
    const data = new Uint8Array([key ? 0 : 1, index & 0xff, index >> 8, 0]);
    await source.add(
      new EncodedPacket(data, key ? "key" : "delta", index / FPS, 1 / FPS),
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
  }
  await output.finalize();
  return new Blob([output.target.buffer], { type: "video/webm" });
}
const blob = await stream();
const settle = (ms = 0) => new Promise((r) => setTimeout(r, ms));

function countingCoordinator(options = {}) {
  const opened = [];
  const delivered = [];
  const coordinator = new DecoderRefillCoordinator(
    async (sourceId) => {
      const video = await MediabunnyVideoDecoder.open(sourceId, blob);
      const entry = { sourceId, closed: false, video };
      const close = video.close.bind(video);
      video.close = () => {
        entry.closed = true;
        return close();
      };
      opened.push(entry);
      return video;
    },
    async (frame) => {
      delivered.push(frame.sourceId);
      frame.frame.close();
    },
    options,
  );
  const open = () => opened.filter((entry) => !entry.closed).length;
  return { coordinator, opened, delivered, open };
}

test("clips of more files than the budget, played in turn, keep the open cursors within it and release idle surfaces", async () => {
  const SOURCES = DEFAULT_MAX_DECODER_CURSORS + 4;
  // Each clip shows frames 0 to 9 of its 90-frame file, so no cursor reaches the end of its stream.
  const CLIP_FRAMES = 10;
  const { coordinator, delivered, open } = countingCoordinator({
    idleMs: 300,
  });
  let mostOpen = 0;
  try {
    for (let source = 0; source < SOURCES; source += 1) {
      for (let frame = 0; frame < CLIP_FRAMES; frame += 1)
        await coordinator.refill({
          sourceId: `file-${source}`,
          consumerId: `clip-${source}`,
          mediaTimeTicks: BigInt(frame) * TICKS_PER_FRAME,
          reason: frame === 0 ? "seek" : "prefetch",
          generation: 0,
        });
      await settle(20);
      mostOpen = Math.max(mostOpen, open(), counters.decodersLive);
    }
    assert.equal(delivered.length, SOURCES * CLIP_FRAMES);
    assert.ok(mostOpen <= DEFAULT_MAX_DECODER_CURSORS, `${mostOpen} open`);
    assert.equal(coordinator.diagnostics.evictions, 4);
    assert.equal(coordinator.diagnostics.overBudget, 0);
    // The playhead is past every clip; nothing asks any of these cursors again.
    await settle(1_000);
    assert.equal(
      counters.surfacesHeld,
      0,
      "an idle cursor holds no decoder surface",
    );
    // Under load an early cursor can idle out before it is evicted too.
    assert.ok(
      coordinator.diagnostics.idleFlushes >= DEFAULT_MAX_DECODER_CURSORS,
    );
    assert.equal(
      coordinator.diagnostics.openCursors,
      DEFAULT_MAX_DECODER_CURSORS,
    );
  } finally {
    coordinator.close();
    await settle(50);
  }
  assert.equal(counters.decodersLive, 0);
  assert.equal(counters.surfacesHeld, 0);
});

test("five consumers of one source each keep their cursor across frames", async () => {
  const { coordinator, opened } = countingCoordinator();
  try {
    for (let frame = 0; frame < 10; frame += 1)
      await Promise.all(
        Array.from({ length: 5 }, (_, consumer) =>
          coordinator.refill({
            sourceId: "file",
            consumerId: `clip-${consumer}`,
            mediaTimeTicks: BigInt(frame) * TICKS_PER_FRAME,
            reason: "prefetch",
            generation: 0,
          }),
        ),
      );
    assert.equal(opened.length, 5);
  } finally {
    coordinator.close();
    await settle(50);
  }
});

function stubCoordinator(options) {
  const opened = [];
  const gates = new Map();
  const coordinator = new DecoderRefillCoordinator(
    (sourceId) => {
      const decoder = {
        sourceId,
        closed: false,
        flushed: 0,
        async refill(request) {
          await gates.get(request.consumerId ?? sourceId);
          return {
            sourceId,
            requestedTimeTicks: request.mediaTimeTicks,
            decodedTimeTicks: request.mediaTimeTicks,
            generation: request.generation,
            frame: { close() {} },
          };
        },
        async flush() {
          decoder.flushed += 1;
        },
        close() {
          decoder.closed = true;
        },
      };
      opened.push(decoder);
      return decoder;
    },
    async () => {},
    options,
  );
  const request = (sourceId) => ({
    sourceId,
    mediaTimeTicks: 0n,
    reason: "prefetch",
    generation: 0,
  });
  return { coordinator, opened, gates, request };
}

test("a cursor with a refill running is never closed for the budget; the budget is exceeded instead", async () => {
  const { coordinator, opened, gates, request } = stubCoordinator({
    maxCursors: 1,
  });
  let release;
  gates.set("a", new Promise((resolve) => (release = resolve)));
  try {
    const slow = coordinator.refill(request("a"));
    await settle(0);
    await coordinator.refill(request("b"));
    await settle(0);
    assert.deepEqual(
      opened.map(({ sourceId, closed }) => [sourceId, closed]),
      [
        ["a", false],
        ["b", false],
      ],
    );
    assert.equal(coordinator.diagnostics.overBudget, 1);
    release();
    await slow;
    await coordinator.refill(request("c"));
    await settle(0);
    assert.deepEqual(
      opened.map(({ closed }) => closed),
      [true, true, false],
      "idle cursors close, least recently used first",
    );
    assert.equal(coordinator.diagnostics.evictions, 2);
  } finally {
    coordinator.close();
  }
});

test("retained cursors stay open past the budget and others close", async () => {
  const { coordinator, opened, request } = stubCoordinator({ maxCursors: 2 });
  try {
    coordinator.retain([{ sourceId: "a" }, { sourceId: "b" }]);
    for (const source of ["a", "b", "c", "d"])
      await coordinator.refill(request(source));
    await settle(0);
    assert.deepEqual(
      opened.map(({ sourceId, closed }) => [sourceId, closed]),
      [
        ["a", false],
        ["b", false],
        ["c", true],
        ["d", false],
      ],
    );
    coordinator.retain([{ sourceId: "d" }]);
    await settle(0);
    assert.deepEqual(
      opened.map(({ closed }) => closed),
      [true, false, true, false],
    );
  } finally {
    coordinator.close();
  }
  assert.throws(
    () =>
      new DecoderRefillCoordinator(
        () => {},
        async () => {},
        { maxCursors: 0 },
      ),
    RangeError,
  );
  assert.throws(
    () =>
      new DecoderRefillCoordinator(
        () => {},
        async () => {},
        { idleMs: 1.5 },
      ),
    RangeError,
  );
});
