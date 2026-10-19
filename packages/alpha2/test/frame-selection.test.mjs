// The browser decoders pick the source frames a native export does, for rounded and exact time
// bases from 23.976 to 60 fps: `tests/golden/frame-selection-corpus.json`, written by
// `bindings/alpha2-host/src/tests.rs` from the native decode of the same files.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import {
  ALL_FORMATS,
  BufferSource,
  BufferTarget,
  CustomVideoDecoder,
  EncodedPacket,
  EncodedPacketSink,
  EncodedVideoPacketSource,
  Input,
  Mp4OutputFormat,
  Output,
  VideoSample,
  registerDecoder,
} from "mediabunny";

import {
  ROUNDING_SCAN_BYTES,
  ROUNDING_SCAN_FRAMES,
  WebCodecsVideoDecoder,
  roundingQuantum,
} from "../dist/media.js";
import { MediabunnyVideoDecoder } from "../dist/mediabunny.js";

const corpusUrl = new URL("../../../tests/corpus/", import.meta.url);
const table = JSON.parse(
  await readFile(
    new URL(
      "../../../tests/golden/frame-selection-corpus.json",
      import.meta.url,
    ),
    "utf8",
  ),
);

const WIDTH = 4;
const HEIGHT = 4;
const counts = { decoded: 0 };

/** Node has no WebCodecs: a frame that carries its time and a few bytes. */
class TestVideoFrame {
  constructor(source, init = {}) {
    this.bytes =
      source instanceof TestVideoFrame
        ? source.bytes
        : new Uint8Array(WIDTH * HEIGHT * 4);
    this.timestamp = init.timestamp ?? source.timestamp;
    this.duration = init.duration ?? source.duration ?? null;
    this.format = "RGBA";
    this.codedWidth = WIDTH;
    this.codedHeight = HEIGHT;
    this.displayWidth = WIDTH;
    this.displayHeight = HEIGHT;
    this.visibleRect = { x: 0, y: 0, width: WIDTH, height: HEIGHT };
    this.colorSpace = {
      primaries: "bt709",
      transfer: "bt709",
      matrix: "bt709",
      fullRange: false,
      toJSON() {
        return { ...this };
      },
    };
  }

  clone() {
    return new TestVideoFrame(this);
  }

  allocationSize() {
    return this.bytes.byteLength;
  }

  async copyTo(destination) {
    new Uint8Array(destination).set(this.bytes);
    return [{ offset: 0, stride: WIDTH * 4 }];
  }

  close() {}
}

/** Decodes an H.264 or VP9 packet into a frame at the packet's time. */
class TimestampDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return codec === "avc" || codec === "vp9";
  }
  init() {}
  decode(packet) {
    counts.decoded += 1;
    this.onSample(
      new VideoSample(
        new TestVideoFrame(
          { timestamp: 0 },
          {
            timestamp: Math.round(packet.timestamp * 1e6),
            duration: Math.round(packet.duration * 1e6),
          },
        ),
      ),
    );
  }
  flush() {}
  close() {}
}

/** A WebCodecs `VideoDecoder` that outputs each chunk as a frame at its time. */
class TimestampVideoDecoder extends EventTarget {
  decodeQueueSize = 0;
  #output;
  #pending = [];
  constructor({ output }) {
    super();
    this.#output = output;
  }
  configure() {}
  decode(chunk) {
    counts.decoded += 1;
    this.decodeQueueSize += 1;
    this.#pending.push(
      Promise.resolve().then(() => {
        this.decodeQueueSize -= 1;
        this.#output(new TestVideoFrame({ timestamp: chunk.timestamp }));
        this.dispatchEvent(new Event("dequeue"));
      }),
    );
  }
  async flush() {
    await Promise.all(this.#pending.splice(0));
  }
  close() {}
}

globalThis.VideoFrame = TestVideoFrame;
registerDecoder(TimestampDecoder);

/** The tick a frame time in microseconds rounds to, as the adapters read it. */
const ticks = (microseconds) =>
  (BigInt(microseconds) * 120_000n + 500_000n) / 1_000_000n;

/**
 * A source for the WebCodecs adapter that serves `file`'s packets as a demuxer would: from the
 * keyframe at or before a time, with the time resolution and the rounding its first packets show.
 */
async function encodedSource(bytes) {
  const input = new Input({
    source: new BufferSource(bytes),
    formats: ALL_FORMATS,
  });
  const track = await input.getPrimaryVideoTrack();
  const sink = new EncodedPacketSink(track);
  const packets = [];
  for (
    let packet = await sink.getFirstPacket({ metadataOnly: true });
    packet !== null;
    packet = await sink.getNextPacket(packet, { metadataOnly: true })
  )
    packets.push({
      timestamp: Math.round(packet.timestamp * 1e6),
      key: packet.type === "key",
      seconds: packet.timestamp,
    });
  const timeResolution = await track.getTimeResolution();
  input.dispose();
  return {
    config: { codec: "fake" },
    timeResolution,
    roundingQuantum: roundingQuantum(
      packets
        .slice(0, ROUNDING_SCAN_FRAMES)
        .map((packet) => Math.round(packet.seconds * timeResolution)),
    ),
    chunksFrom: (targetTicks) =>
      (async function* chunks() {
        let start = 0;
        packets.forEach((packet, index) => {
          if (packet.key && ticks(packet.timestamp) <= targetTicks)
            start = index;
        });
        for (const packet of packets.slice(start))
          yield {
            timestamp: packet.timestamp,
            type: packet.key ? "key" : "delta",
          };
      })(),
  };
}

const opens = {
  mediabunny: (bytes) => MediabunnyVideoDecoder.open("clip", bytes),
  webcodecs: async (bytes) =>
    new WebCodecsVideoDecoder("clip", await encodedSource(bytes)),
};

/** The project frame times of `rate` up to the source's last frame. */
function frameTimes({ num, den }, last) {
  const times = [];
  for (let frame = 0n; ; frame += 1n) {
    // A frame starts at the tick at or before its exact time, as the engine floors it.
    const start = (frame * 120_000n * BigInt(den)) / BigInt(num);
    if (start > last) return times;
    times.push(start);
  }
}

for (const [adapter, open] of Object.entries(opens))
  test(`the ${adapter} adapter shows the frames a native export does`, async () => {
    const original = globalThis.VideoDecoder;
    globalThis.VideoDecoder = TimestampVideoDecoder;
    try {
      for (const source of table.sources) {
        const bytes = new Uint8Array(
          await readFile(new URL(source.file, corpusUrl)),
        );
        const frameTicks = source.frameTicks.map(BigInt);
        const index = (decoded) => {
          const found = frameTicks.indexOf(decoded);
          assert.notEqual(found, -1, `${source.file}: no frame at ${decoded}`);
          return found;
        };
        for (const { rate, frames } of source.rates) {
          const label = `${source.file} at ${rate.num}/${rate.den}`;
          const times = frameTimes(rate, frameTicks.at(-1));
          assert.equal(times.length, frames.length, label);
          const shown = async (decoder, time, reason) => {
            const answer = await decoder.refill({
              sourceId: "clip",
              mediaTimeTicks: time,
              reason,
              generation: 0,
            });
            answer.frame.close();
            return index(answer.decodedTimeTicks);
          };
          const sequential = await open(bytes);
          const inOrder = [];
          for (const time of times)
            inOrder.push(await shown(sequential, time, "prefetch"));
          sequential.close();
          assert.deepEqual(inOrder, frames, `${label} in order`);
          const seeking = await open(bytes);
          const sought = [];
          for (const time of [...times].reverse())
            sought.unshift(await shown(seeking, time, "seek"));
          seeking.close();
          assert.deepEqual(sought, frames, `${label} sought`);
        }
      }
    } finally {
      globalThis.VideoDecoder = original;
    }
  });

// A keyframe stored a few ticks after its frame time is sought directly, not decoded to from the
// keyframe before, which would take a GOP of 33 frames for one.
for (const [adapter, open] of Object.entries(opens))
  test(`the ${adapter} adapter seeks onto a keyframe stored up to the slack after its time`, async () => {
    const original = globalThis.VideoDecoder;
    globalThis.VideoDecoder = TimestampVideoDecoder;
    try {
      const bytes = new Uint8Array(
        await readFile(new URL("frame-rounding-30-gop32.mkv", corpusUrl)),
      );
      const decoder = await open(bytes);
      counts.decoded = 0;
      const answer = await decoder.refill({
        sourceId: "clip",
        mediaTimeTicks: 128_000n,
        reason: "seek",
        generation: 0,
      });
      answer.frame.close();
      decoder.close();
      assert.equal(answer.decodedTimeTicks, 128_040n);
      assert.ok(
        counts.decoded <= 2,
        `${counts.decoded} frames decoded for a seek onto a keyframe`,
      );
    } finally {
      globalThis.VideoDecoder = original;
    }
  });

/**
 * An MP4 whose VP9 track (packets of `bytes` each, which the timestamp decoder never reads) has a
 * 1/16000 time base and a frame at each of `units` of it.
 */
async function mp4WithPackets(units, bytes) {
  const target = new BufferTarget();
  const output = new Output({ format: new Mp4OutputFormat(), target });
  const source = new EncodedVideoPacketSource("vp9");
  output.addVideoTrack(source, { frameRate: 16_000 });
  await output.start();
  const data = new Uint8Array(bytes);
  for (const [index, unit] of units.entries()) {
    const next = units[index + 1] ?? unit + 640;
    await source.add(
      new EncodedPacket(
        data,
        index === 0 ? "key" : "delta",
        unit / 16_000,
        (next - unit) / 16_000,
      ),
      index === 0
        ? {
            decoderConfig: {
              codec: "vp09.00.10.08",
              codedWidth: 64,
              codedHeight: 64,
            },
          }
        : undefined,
    );
  }
  await output.finalize();
  return new Uint8Array(target.buffer);
}

// The slack is read from a track's first 64 packets, at most 16 MiB, as a native host reads it:
// a scan past 16 MiB would see the later millisecond rounding and show the ninth frame.
test("the mediabunny adapter reads the slack from no more than 16 MiB of first packets", async () => {
  const frameBytes = 2 * 1024 * 1024;
  const exact = ROUNDING_SCAN_BYTES / frameBytes;
  assert.equal(exact, 8);
  const units = Array.from({ length: exact }, (_, index) => index * 640);
  for (const step of [528, 544, 528, 544]) units.push(units.at(-1) + step);
  assert.ok(units.length < ROUNDING_SCAN_FRAMES);
  assert.notEqual(
    roundingQuantum(units.slice(0, exact)),
    roundingQuantum(units),
    "the frames past 16 MiB show a rounding",
  );
  const decoder = await MediabunnyVideoDecoder.open(
    "clip",
    await mp4WithPackets(units, frameBytes),
  );
  try {
    const ninth = (BigInt(units[exact]) * 120_000n) / 16_000n;
    const answer = await decoder.refill({
      sourceId: "clip",
      mediaTimeTicks: ninth - 30n,
      reason: "seek",
      generation: 0,
    });
    answer.frame.close();
    assert.equal(
      answer.decodedTimeTicks,
      (BigInt(units[exact - 1]) * 120_000n) / 16_000n,
    );
  } finally {
    decoder.close();
  }
});
