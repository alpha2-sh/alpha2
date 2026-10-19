// A browser audio window puts its samples where a decode from the stream's start does, in Matroska
// and WebM (millisecond timestamps) as in MP4; the native host's equivalent test in
// `bindings/alpha2-native/src/tests.rs` reads the same files.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { AudioSample, CustomAudioDecoder, registerDecoder } from "mediabunny";

import { MediabunnyAudioDecoder } from "../dist/mediabunny.js";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const binding = existsSync(join(wasmDirectory, "alpha2_wasm.js"))
  ? await (async () => {
      const wasm = await import(
        pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
      );
      await wasm.default({
        module_or_path: await readFile(
          join(wasmDirectory, "alpha2_wasm_bg.wasm"),
        ),
      });
      return wasm;
    })()
  : null;
const needsWasm = binding === null && "the wasm package is not built";

const RATE = 48_000;
const FRAMES = { aac: 1_024, opus: 960 };

/**
 * Node has no WebCodecs: decodes each packet into a frame of its codec's length whose samples name
 * the packet (its timestamp) and their place in it, so two windows agree only where they put the
 * same packet's samples at the same source sample. As a WebCodecs Opus decoder does, it drops the
 * `OpusHead` pre-skip from the first frame it decodes after it is configured, and times that
 * frame at its packet.
 */
class PacketNamingDecoder extends CustomAudioDecoder {
  static supports(codec) {
    return codec in FRAMES;
  }
  init() {
    const description = this.config.description;
    const head =
      this.codec !== "opus" || description === undefined
        ? null
        : ArrayBuffer.isView(description)
          ? new Uint8Array(
              description.buffer,
              description.byteOffset,
              description.byteLength,
            )
          : new Uint8Array(description);
    this.skip = head === null ? 0 : head[10] | (head[11] << 8);
  }
  decode(packet) {
    const frames = FRAMES[this.codec];
    const name = (Math.round(packet.timestamp * 1e6) % 7_919) / 7_919;
    const data = new Float32Array(frames - this.skip);
    for (let index = this.skip; index < frames; index += 1)
      data[index - this.skip] = (name + index / frames) / 2;
    this.skip = 0;
    this.onSample(
      new AudioSample({
        data,
        format: "f32-planar",
        numberOfChannels: 1,
        sampleRate: RATE,
        timestamp: packet.timestamp,
      }),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(PacketNamingDecoder);

const corpus = (name) =>
  readFile(new URL(`../../../tests/corpus/${name}`, import.meta.url)).then(
    (bytes) => new Uint8Array(bytes),
  );

/** Samples `startMs` to `endMs` of `decoder`'s source at 48 kHz, as one window. */
async function window(decoder, startMs, endMs) {
  const block = await decoder.refill(
    {
      sourceId: "audio",
      startMediaTimeTicks: BigInt(startMs * 120),
      endMediaTimeTicks: BigInt(endMs * 120),
      generation: 1,
    },
    RATE,
  );
  return { start: Number(block.startSample), samples: block.channels[0] };
}

for (const name of [
  "audio-seek-aac.mkv",
  "audio-seek-aac.mp4",
  "audio-seek-aac-remux.mp4",
  "audio-seek-opus.webm",
  "audio-seek-opus.mp4",
  "audio-seek-libopus.webm",
]) {
  test(
    `${name}: a window holds the samples a window from the start does`,
    { skip: needsWasm },
    async () => {
      const decoder = await MediabunnyAudioDecoder.open(
        "audio",
        await corpus(name),
        binding,
      );
      try {
        const whole = await window(decoder, 0, 1_500);
        for (const ms of [150, 250, 333, 500, 777, 1_000, 1_234]) {
          const part = await window(decoder, ms, ms + 100);
          const from = part.start - whole.start;
          // The two edge samples each side are the resampler's margin; compare inside it.
          for (let index = 4; index < part.samples.length - 4; index += 1)
            assert.equal(
              part.samples[index],
              whole.samples[from + index],
              `${name} window at ${ms} ms, sample ${part.start + index}`,
            );
        }
      } finally {
        decoder.close();
      }
    },
  );
}

// A Matroska block is timed its track's `CodecDelay` (FFmpeg's 21.33 ms AAC priming, rounded to
// 21 ms) after its samples. Subtracted as FFmpeg does, the first packet's 1024 samples sit before
// 0 as in MP4; mediabunny's own timestamps put that packet at 0, 1024 samples late.
test(
  "a Matroska track's samples are presented its CodecDelay before their block times",
  { skip: needsWasm },
  async () => {
    // The packet the second is named for: timed 21 ms in Matroska, 0 in MP4.
    for (const [name, timestamp] of [
      ["audio-seek-aac.mkv", 21_000],
      ["audio-seek-aac.mp4", 0],
    ]) {
      const decoder = await MediabunnyAudioDecoder.open(
        "audio",
        await corpus(name),
        binding,
      );
      try {
        const { start, samples } = await window(decoder, 0, 100);
        const named = (timestamp % 7_919) / 7_919;
        for (const sample of [0, 1, 500, 1_023])
          assert.equal(
            samples[sample - start],
            Math.fround((named + sample / 1_024) / 2),
            `${name}, sample ${sample}`,
          );
      } finally {
        decoder.close();
      }
    }
  },
);

test("the CodecDelay a Matroska or WebM track declares", async () => {
  const { readMatroskaCodecDelay } = await import("../dist/media.js");
  const delays = {};
  for (const name of [
    "audio-seek-aac.mkv",
    "audio-seek-aac-44100.mkv",
    "audio-seek-opus.webm",
    "audio-seek-libopus.webm",
    "audio-seek-aac.mp4",
  ])
    delays[name] = await readMatroskaCodecDelay(await corpus(name), 1);
  assert.deepEqual(delays, {
    "audio-seek-aac.mkv": 21_333_333,
    "audio-seek-aac-44100.mkv": 23_219_955,
    "audio-seek-opus.webm": 2_500_000,
    "audio-seek-libopus.webm": 6_500_000,
    "audio-seek-aac.mp4": 0,
  });
  assert.equal(
    await readMatroskaCodecDelay(await corpus("audio-seek-aac.mkv"), 2),
    0,
  );
});
