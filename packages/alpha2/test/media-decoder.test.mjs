import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import {
  DecoderRefillCoordinator,
  MAX_STALL_TIMEOUT_MS,
  PRESENTATION_SLACK_TICKS,
  WebCodecsVideoDecoder,
  presentationSlackTicks,
  roundingQuantum,
} from "../dist/media.js";
import { MediabunnyAudioDecoder } from "../dist/mediabunny.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");

// The packaged binding, whose resampler the audio adapter uses, or null before `npm run build` made
// it. ALPHA2_REQUIRE=wasm loads it regardless, so a missing package fails instead of skipping.
async function loadBinding() {
  if (!existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"))) return null;
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}
const binding = await loadBinding();
const needsWasm =
  binding === null &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

class FakeVideoFrame {
  constructor(timestamp) {
    this.timestamp = timestamp;
    this.closed = false;
  }

  close() {
    this.closed = true;
  }

  clone() {
    return new FakeVideoFrame(this.timestamp);
  }
}

class FakeVideoDecoder extends EventTarget {
  decodeQueueSize = 0;
  #output;
  #pending = [];

  constructor({ output }) {
    super();
    this.#output = output;
  }

  configure() {}

  decode(chunk) {
    this.decodeQueueSize += 1;
    const pending = Promise.resolve().then(() => {
      this.decodeQueueSize -= 1;
      this.#output(new FakeVideoFrame(chunk.timestamp));
      this.dispatchEvent(new Event("dequeue"));
    });
    this.#pending.push(pending);
  }

  async flush() {
    await Promise.all(this.#pending.splice(0));
  }

  close() {}
}

test("WebCodecs decoder keeps a sequential cursor and reuses a reverse window", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  let cursors = 0;
  const source = {
    config: { codec: "fake" },
    chunksFrom: (targetTicks) => {
      cursors += 1;
      // From the frame at or before the target, every frame a keyframe.
      const firstMicros =
        Math.floor(Number((targetTicks * 1_000_000n) / 120_000n) / 33_333) *
        33_333;
      return (async function* chunks() {
        for (let index = 0; index < 4; index += 1) {
          yield { timestamp: firstMicros + index * 33_333 };
        }
      })();
    },
  };
  const decoder = new WebCodecsVideoDecoder("source", source);
  const request = (mediaTimeTicks) => ({
    sourceId: "source",
    mediaTimeTicks,
    reason: "prefetch",
    generation: 1,
  });

  try {
    const first = await decoder.refill(request(0n));
    const second = await decoder.refill(request(3_938n));
    assert.equal(first.requestedTimeTicks, 0n);
    assert.equal(first.decodedTimeTicks, 0n);
    assert.equal(second.requestedTimeTicks, 3_938n);
    // 33333 us is 3999.96 ticks: frame one starts at 4000, past 3938's deadline (61 ticks on), so
    // frame zero is still presented at 3938.
    assert.equal(second.decodedTimeTicks, 0n);
    assert.equal(cursors, 1, "forward requests reuse one demux/decode cursor");
    first.frame.close();
    second.frame.close();

    const reversed = await decoder.refill(request(0n));
    assert.equal(cursors, 1, "reverse requests reuse a cached decoded frame");
    assert.equal(decoder.reverseCacheHits, 1);
    reversed.frame.close();
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("WebCodecs keeps target candidates decoded from one GOP for reverse use", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  let cursors = 0;
  const source = {
    config: { codec: "fake" },
    chunksFrom: () => {
      cursors += 1;
      return (async function* chunks() {
        for (const timestamp of [0, 33_333, 66_666, 99_999]) {
          yield { timestamp };
        }
      })();
    },
  };
  const decoder = new WebCodecsVideoDecoder("source", source);
  const request = (mediaTimeTicks) => ({
    sourceId: "source",
    mediaTimeTicks,
    reason: "prefetch",
    generation: 1,
  });
  try {
    // Each request gets the frame presented at it, the last at or before its deadline: 7938's is
    // before the frame at 8000.
    const high = await decoder.refill(request(7_938n));
    assert.equal(high.decodedTimeTicks, 4_000n);
    high.frame.close();
    const lower = await decoder.refill(request(3_938n));
    assert.equal(lower.decodedTimeTicks, 0n);
    lower.frame.close();
    assert.equal(cursors, 1);
    assert.equal(decoder.reverseCacheHits, 1);
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("a frame's microsecond timestamp rounds to its tick, so the frame at a tick is found", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  const source = {
    config: { codec: "fake" },
    chunksFrom: () =>
      (async function* chunks() {
        for (const timestamp of [0, 33_333, 66_666]) yield { timestamp };
      })(),
  };
  const decoder = new WebCodecsVideoDecoder("source", source);
  try {
    // 30 fps frame one is 33333 us, which floors to 3999 ticks: a decoder that floored would skip
    // it for a request at its exact tick, 4000, and return frame two.
    const exact = await decoder.refill({
      sourceId: "source",
      mediaTimeTicks: 4_000n,
      reason: "seek",
      generation: 1,
    });
    assert.equal(exact.decodedTimeTicks, 4_000n);
    exact.frame.close();
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("the presentation slack is the engine's", { skip: needsWasm }, () => {
  for (const resolution of [
    1_000, 90_000, 30_000, 15_360, 120_000, 600, 100, 1, 25, 7,
  ])
    for (const quantum of [0, 1, 16, 90])
      assert.equal(
        presentationSlackTicks(resolution, quantum),
        binding.presentationSlackTicks(1n, BigInt(resolution), BigInt(quantum)),
        `${resolution}, rounded to ${quantum}`,
      );
  assert.equal(presentationSlackTicks(), PRESENTATION_SLACK_TICKS);
  assert.equal(
    binding.presentationSlackTicks(0n, 0n, 0n),
    PRESENTATION_SLACK_TICKS,
  );
  const timestamps = [0, 528, 1072, 1600, 2128, 2672];
  assert.equal(
    roundingQuantum(timestamps),
    Number(binding.roundingQuantum(BigInt64Array.from(timestamps, BigInt))),
  );
});

// Timestamps a few ticks after the floored project frame times (90 fps at 2666.67 ticks, or
// Matroska's millisecond rounding) must not show a frame twice and skip another.
test("a source at the project's rate shows each of its frames once", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  const streams = {
    "90 fps": {
      timestamps: Array.from({ length: 30 }, (_, k) =>
        Math.round((k * 1_000_000) / 90),
      ),
      timeResolution: 1_000_000,
      start: (k) => (BigInt(k) * 120_000n) / 90n,
    },
    "29.97 fps in milliseconds": {
      timestamps: Array.from(
        { length: 30 },
        (_, k) => Math.round((k * 1_001) / 30) * 1_000,
      ),
      timeResolution: 1_000,
      start: (k) => (BigInt(k) * 120_000n * 1_001n) / 30_000n,
    },
  };
  // A keyframe every frame: a seek reads from one second before its target.
  const source = (timestamps, timeResolution) => ({
    config: { codec: "fake" },
    timeResolution,
    chunksFrom: (targetTicks) =>
      (async function* chunks() {
        for (const timestamp of timestamps)
          if (
            BigInt(timestamp) * 120_000n >=
            (targetTicks - 120_000n) * 1_000_000n
          )
            yield { timestamp };
      })(),
  });
  const ticksOf = (microseconds) =>
    (BigInt(microseconds) * 120_000n + 500_000n) / 1_000_000n;
  try {
    for (const [name, { timestamps, timeResolution, start }] of Object.entries(
      streams,
    )) {
      const shown = async (decoder, k, reason) => {
        const answer = await decoder.refill({
          sourceId: "source",
          mediaTimeTicks: start(k),
          reason,
          generation: 1,
        });
        answer.frame.close();
        return timestamps.findIndex(
          (timestamp) => ticksOf(timestamp) === answer.decodedTimeTicks,
        );
      };
      const sequential = new WebCodecsVideoDecoder(
        "source",
        source(timestamps),
      );
      const inOrder = [];
      for (let k = 0; k < 30; k += 1)
        inOrder.push(await shown(sequential, k, "prefetch"));
      sequential.close();
      assert.deepEqual(
        inOrder,
        timestamps.map((_, k) => k),
        `${name} in order`,
      );
      const sought = [];
      for (let k = 29; k >= 0; k -= 1) {
        const decoder = new WebCodecsVideoDecoder(
          "source",
          source(timestamps, timeResolution),
        );
        sought.push(await shown(decoder, k, "seek"));
        decoder.close();
      }
      assert.deepEqual(
        sought,
        timestamps.map((_, k) => 29 - k),
        `${name} sought`,
      );
    }
    // Only that far: a 29.97 fps stream in an exact 1/30000 time base starts its frame 3 at 12012
    // ticks, after a 30 fps project's frame 3 at 12000, which still shows frame 2.
    const exact = new WebCodecsVideoDecoder(
      "source",
      source(
        Array.from({ length: 6 }, (_, k) => Math.round((k * 1_001_000) / 30)),
        30_000,
      ),
    );
    const at12000 = await exact.refill({
      sourceId: "source",
      mediaTimeTicks: 12_000n,
      reason: "seek",
      generation: 1,
    });
    at12000.frame.close();
    exact.close();
    assert.equal(at12000.decodedTimeTicks, 8_008n);
  } finally {
    globalThis.VideoDecoder = original;
  }
});

test("each request gets the frame presented at it, as the native decoder does", async () => {
  // vfr.mp4's timestamps: frames at 0, 4000, 12000, 24000 and 40000 ticks.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  const timestamps = [0, 33_333, 100_000, 200_000, 333_333];
  const source = (first) => ({
    config: { codec: "fake" },
    chunksFrom: () =>
      (async function* chunks() {
        for (const timestamp of timestamps.slice(first)) yield { timestamp };
      })(),
  });
  const refill = async (decoder, mediaTimeTicks) => {
    const result = await decoder.refill({
      sourceId: "source",
      mediaTimeTicks,
      reason: "prefetch",
      generation: 1,
    });
    result.frame.close();
    return result.decodedTimeTicks;
  };
  try {
    const decoder = new WebCodecsVideoDecoder("source", source(0));
    const presented = [];
    for (const ticks of [2_000n, 6_000n, 12_000n, 20_000n, 30_000n, 43_000n])
      presented.push(await refill(decoder, ticks));
    assert.deepEqual(presented, [
      0n,
      4_000n,
      12_000n,
      12_000n,
      24_000n,
      40_000n,
    ]);
    decoder.close();
    const late = new WebCodecsVideoDecoder("source", source(2));
    assert.equal(await refill(late, 6_000n), 12_000n);
    late.close();
  } finally {
    globalThis.VideoDecoder = original;
  }
});

test("refill coordinator bounds independent sources and serializes each source", async () => {
  let active = 0;
  let peak = 0;
  const sourceActive = new Map();
  const delivered = [];
  const decoder = (sourceId) => ({
    async refill(request) {
      assert.equal(sourceActive.get(sourceId) ?? 0, 0);
      sourceActive.set(sourceId, 1);
      active += 1;
      peak = Math.max(peak, active);
      await new Promise((resolve) => setTimeout(resolve, 5));
      active -= 1;
      sourceActive.set(sourceId, 0);
      return {
        sourceId,
        requestedTimeTicks: request.mediaTimeTicks,
        decodedTimeTicks: request.mediaTimeTicks,
        generation: request.generation,
        frame: new FakeVideoFrame(Number(request.mediaTimeTicks)),
      };
    },
    async flush() {},
    close() {},
  });
  const coordinator = new DecoderRefillCoordinator(
    (sourceId) => decoder(sourceId),
    async (frame) => {
      delivered.push([frame.sourceId, frame.requestedTimeTicks]);
      frame.frame.close();
    },
    { maxConcurrency: 2 },
  );
  const request = (sourceId, mediaTimeTicks) => ({
    sourceId,
    mediaTimeTicks,
    reason: "prefetch",
    generation: 0,
  });
  try {
    await Promise.all([
      coordinator.refill(request("a", 0n)),
      coordinator.refill(request("b", 0n)),
      coordinator.refill(request("a", 1n)),
      coordinator.refill(request("c", 0n)),
    ]);
    assert.equal(peak, 2);
    assert.deepEqual(
      delivered.filter(([sourceId]) => sourceId === "a"),
      [
        ["a", 0n],
        ["a", 1n],
      ],
    );
  } finally {
    coordinator.close();
  }
});

test(
  "Mediabunny decodes a bounded project-rate audio interval",
  { skip: needsWasm },
  async () => {
    const decoder = await MediabunnyAudioDecoder.open(
      "tone",
      pcmWave(44_100, 44_100),
      binding,
    );
    try {
      const block = await decoder.refill(
        {
          sourceId: "tone",
          startMediaTimeTicks: 30_000n,
          endMediaTimeTicks: 42_000n,
          generation: 7,
        },
        48_000,
      );
      assert.equal(block.sampleRate, 48_000);
      assert.equal(block.channels.length, 1);
      assert.equal(block.channels[0].length, 4_804);
      assert.ok(block.channels[0].some((sample) => Math.abs(sample) > 0.1));
      assert.equal(block.generation, 7);
    } finally {
      decoder.close();
    }
  },
);

test(
  "Mediabunny keeps every channel of a surround source in WAVE order",
  { skip: needsWasm },
  async () => {
    const decoder = await MediabunnyAudioDecoder.open(
      "surround",
      new Uint8Array(
        await readFile(
          new URL(
            "../../../tests/corpus/audio-surround-offset.mov",
            import.meta.url,
          ),
        ),
      ),
      binding,
    );
    try {
      const block = await decoder.refill(
        {
          sourceId: "surround",
          startMediaTimeTicks: 7_500n,
          endMediaTimeTicks: 9_000n,
          generation: 1,
        },
        48_000,
      );
      assert.equal(block.channels.length, 6);
      // FL FR FC LFE BL BR at the levels the corpus wrote.
      block.channels.forEach((plane, index) => {
        const expected = Math.round((index + 1) * 0.1 * 32_768) / 32_768;
        assert.ok(
          plane.every((sample) => sample === expected),
          `plane ${index} holds ${expected}`,
        );
      });
    } finally {
      decoder.close();
    }
  },
);

// A stream whose packets jump over a second: the browser places the samples after the jump where
// their timestamps say, as a native host does, and the jump is silence.
test(
  "a clean audio timestamp gap stays silent, and the tone after it keeps its place",
  { skip: needsWasm },
  async () => {
    const decoder = await MediabunnyAudioDecoder.open(
      "gap",
      new Uint8Array(
        await readFile(
          new URL("../../../tests/corpus/audio-gap.mkv", import.meta.url),
        ),
      ),
      binding,
    );
    try {
      for (const [start, end] of [
        [0, 3],
        [1.5, 2.5],
        [2.05, 2.2],
      ]) {
        const block = await decoder.refill(
          {
            sourceId: "gap",
            startMediaTimeTicks: BigInt(Math.round(start * 120_000)),
            endMediaTimeTicks: BigInt(Math.round(end * 120_000)),
            generation: 1,
          },
          48_000,
        );
        const first = Number(block.startSample);
        const at = (sample) => block.channels[0][sample - first];
        for (
          let sample = Math.max(first, 49_200);
          sample < Math.min(first + block.channels[0].length, 94_800);
          sample += 1
        )
          assert.ok(
            Math.abs(at(sample)) < 1e-4,
            `${start}-${end} s: sample ${sample} is ${at(sample)} inside the gap`,
          );
        for (
          let sample = Math.max(first, 96_100);
          sample < Math.min(first + block.channels[0].length - 4, 98_000);
          sample += 1
        ) {
          const expected = Math.sin((2 * Math.PI * 440 * sample) / 48_000) / 8;
          assert.ok(
            Math.abs(at(sample) - expected) < 1e-3,
            `${start}-${end} s: sample ${sample} is ${at(sample)}, the tone ${expected}`,
          );
        }
      }
    } finally {
      decoder.close();
    }
  },
);

/**
 * Decodes `source` (a 44.1 kHz WAV) at 48 kHz in two-second windows from 0 to `seconds`, as a
 * host's frame supplier asks for it.
 */
async function decodeWindows(source, seconds) {
  const decoder = await MediabunnyAudioDecoder.open("wave", source, binding);
  const samples = new Float32Array(seconds * 48_000);
  try {
    for (let start = 0; start < seconds; start += 2) {
      const block = await decoder.refill(
        {
          sourceId: "wave",
          startMediaTimeTicks: BigInt(start * 120_000),
          endMediaTimeTicks: BigInt(Math.min(seconds, start + 2) * 120_000),
          generation: 1,
        },
        48_000,
      );
      const first = Number(block.startSample);
      const from = start * 48_000;
      const to = Math.min(seconds, start + 2) * 48_000;
      samples.set(block.channels[0].subarray(from - first, to - first), from);
    }
  } finally {
    decoder.close();
  }
  return samples;
}

// Resampling each packet alone, from separately truncated microseconds, leaves a sample on each
// join at 0: about one a second for 44.1 kHz into 48 kHz.
test(
  "a constant 44.1 kHz source resamples to a constant with no dropped samples",
  { skip: needsWasm },
  async () => {
    const seconds = 6;
    const samples = await decodeWindows(
      pcmWave(44_100, seconds * 44_100, () => 0.5),
      seconds,
    );
    const level = Math.round(0.5 * 20_000) / 32_768;
    const wrong = [];
    // The first and last samples ring, as any band-limited reading of a step does.
    for (let index = 100; index < samples.length - 100; index += 1)
      if (Math.abs(samples[index] - level) > 1e-5) wrong.push(index);
    assert.deepEqual(
      wrong.slice(0, 8),
      [],
      `${wrong.length} samples are off ${level}`,
    );
  },
);

// Linear interpolation between neighbours takes 1.5 dB off a 10 kHz tone and aliases it to
// 13.9 kHz at -23 dB.
test(
  "a 44.1 kHz tone resamples band-limited, as a native host resamples it",
  { skip: needsWasm },
  async () => {
    const seconds = 2;
    for (const hz of [1_000, 10_000, 18_000]) {
      const samples = await decodeWindows(
        pcmWave(44_100, seconds * 44_100, (index) =>
          Math.sin((index * 2 * Math.PI * hz) / 44_100),
        ),
        seconds,
      );
      let error = 0;
      for (let index = 2_000; index < samples.length - 2_000; index += 1) {
        const expected =
          (Math.sin((index * 2 * Math.PI * hz) / 48_000) * 20_000) / 32_768;
        error = Math.max(error, Math.abs(samples[index] - expected));
      }
      // 16-bit quantization and passband ripple; linear interpolation misses by 0.5% at 10 kHz.
      assert.ok(error < 2e-3, `${hz} Hz: error ${error}`);
    }
  },
);

function pcmWave(
  sampleRate,
  frames,
  wave = (index) => Math.sin((index * 2 * Math.PI * 440) / sampleRate),
) {
  const bytes = new ArrayBuffer(44 + frames * 2);
  const view = new DataView(bytes);
  const ascii = (offset, value) => {
    for (let index = 0; index < value.length; index += 1)
      view.setUint8(offset + index, value.charCodeAt(index));
  };
  ascii(0, "RIFF");
  view.setUint32(4, 36 + frames * 2, true);
  ascii(8, "WAVE");
  ascii(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  ascii(36, "data");
  view.setUint32(40, frames * 2, true);
  for (let index = 0; index < frames; index += 1)
    view.setInt16(44 + index * 2, Math.round(wave(index) * 20_000), true);
  return new Blob([bytes], { type: "audio/wav" });
}

/**
 * A decoder with a fixed surface pool, as hardware decoders have: a surface returns to the pool
 * only once every clone of its frame is closed, and the decoder produces nothing while the pool is
 * empty.
 */
function pooledDecoderClass(surfaces) {
  let free = surfaces;
  const release = () => {
    free += 1;
    pump();
  };
  let decoders = [];
  const pump = () => {
    for (const decoder of decoders) decoder.drain();
  };
  class SurfaceFrame {
    constructor(timestamp, surface) {
      this.timestamp = timestamp;
      this.surface = surface;
      surface.refs += 1;
      this.closed = false;
    }
    clone() {
      return new SurfaceFrame(this.timestamp, this.surface);
    }
    close() {
      if (this.closed) return;
      this.closed = true;
      this.surface.refs -= 1;
      if (this.surface.refs === 0) release();
    }
  }
  class PooledDecoder extends EventTarget {
    decodeQueueSize = 0;
    #output;
    #queue = [];
    constructor({ output }) {
      super();
      this.#output = output;
      decoders.push(this);
    }
    configure() {}
    decode(chunk) {
      this.decodeQueueSize += 1;
      this.#queue.push(chunk);
      queueMicrotask(() => this.drain());
    }
    drain() {
      while (free > 0 && this.#queue.length > 0) {
        const chunk = this.#queue.shift();
        free -= 1;
        this.decodeQueueSize -= 1;
        this.#output(new SurfaceFrame(chunk.timestamp, { refs: 0 }));
        this.dispatchEvent(new Event("dequeue"));
      }
    }
    async flush() {}
    close() {
      decoders = decoders.filter((decoder) => decoder !== this);
    }
  }
  return PooledDecoder;
}

/** One keyframe at zero, then frames forever, so a seek decodes from zero. */
function endlessSource() {
  return {
    config: { codec: "fake" },
    chunksFrom: () =>
      (async function* chunks() {
        for (let index = 0; ; index += 1) {
          yield { timestamp: index * 33_333 };
        }
      })(),
  };
}

for (const mode of ["abort", "timeout"]) {
  test(`EOF flush ${mode} releases the source and rejects late callbacks`, async () => {
    const original = globalThis.VideoDecoder;
    const instances = [];
    let started;
    const flushing = new Promise((resolve) => {
      started = resolve;
    });
    class EofDecoder extends EventTarget {
      decodeQueueSize = 0;
      constructor(callbacks) {
        super();
        this.callbacks = callbacks;
        this.closed = false;
        instances.push(this);
      }
      configure() {}
      decode() {}
      flush() {
        if (instances.length === 1) {
          started();
          return new Promise((_, reject) => {
            this.rejectFlush = reject;
          });
        }
        this.callbacks.output(new FakeVideoFrame(0));
        return Promise.resolve();
      }
      close() {
        this.closed = true;
      }
    }
    globalThis.VideoDecoder = EofDecoder;
    const decoder = new WebCodecsVideoDecoder(
      "source",
      {
        config: { codec: "fake" },
        chunksFrom: () =>
          (async function* () {
            yield { timestamp: 0 };
          })(),
      },
      { stallTimeoutMs: mode === "timeout" ? 20 : 1_000 },
    );
    const request = (generation) => ({
      sourceId: "source",
      mediaTimeTicks: 0n,
      reason: "seek",
      generation,
    });
    const delivered = [];
    const coordinator = new DecoderRefillCoordinator(
      () => decoder,
      async (frame) => {
        delivered.push(frame.generation);
        frame.frame.close();
      },
    );
    let guard;
    try {
      const pending = coordinator.refill(request(0));
      // Attach the rejection handler before triggering cancellation.
      const outcome = pending.then(
        () => "resolved",
        (error) => error,
      );
      await flushing;
      if (mode === "abort") coordinator.beginGeneration();
      const error = await Promise.race([
        outcome,
        new Promise((resolve) => {
          guard = setTimeout(
            () => resolve(new Error("unbounded EOF flush")),
            300,
          );
        }),
      ]);
      assert.match(
        String(error),
        mode === "abort" ? /AbortError/ : /stalled.*flush/,
      );
      assert.equal(instances[0].closed, true);
      const stale = new FakeVideoFrame(0);
      instances[0].callbacks.output(stale);
      instances[0].callbacks.error(new Error("late decoder error"));
      instances[0].rejectFlush(new Error("late flush rejection"));
      assert.equal(stale.closed, true);
      await coordinator.refill(request(1));
      assert.deepEqual(delivered, [1]);
    } finally {
      clearTimeout(guard);
      coordinator.close();
      globalThis.VideoDecoder = original;
    }
  });
}

test("an aborted iterator cannot block a replacement cursor through return", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  let cursors = 0;
  let started;
  const reading = new Promise((resolve) => {
    started = resolve;
  });
  const decoder = new WebCodecsVideoDecoder("source", {
    config: { codec: "fake" },
    chunksFrom: () =>
      ++cursors === 1
        ? {
            [Symbol.asyncIterator]() {
              return this;
            },
            next() {
              started();
              return new Promise(() => {});
            },
            return() {
              return new Promise(() => {});
            },
          }
        : (async function* () {
            yield { timestamp: 0 };
          })(),
  });
  const request = (generation) => ({
    sourceId: "source",
    mediaTimeTicks: 0n,
    reason: "seek",
    generation,
  });
  const controller = new AbortController();
  let guard;
  try {
    const first = decoder.refill(request(0), controller.signal);
    const rejected = assert.rejects(first, /AbortError/);
    await reading;
    controller.abort();
    await rejected;
    const frame = await Promise.race([
      decoder.refill(request(1)),
      new Promise((_, reject) => {
        guard = setTimeout(
          () => reject(new Error("return blocked replacement")),
          300,
        );
      }),
    ]);
    frame.frame.close();
  } finally {
    clearTimeout(guard);
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("frames kept for reverse playback leave the decoder surfaces to continue", async () => {
  // A cache bounded only by bytes would keep a clone of every small frame, each holding a decoder
  // surface, and stall a hardware decoder with 16 surfaces.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = pooledDecoderClass(16);
  try {
    const decoder = new WebCodecsVideoDecoder("source", endlessSource());
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), 3_000);
    const frame = await decoder.refill(
      {
        sourceId: "source",
        mediaTimeTicks: 64n * 4_000n,
        reason: "seek",
        generation: 1,
      },
      abort.signal,
    );
    clearTimeout(timer);
    // Frame 64 is at 2133312 us, 255997 ticks: the one presented at 256000.
    assert.equal(frame.decodedTimeTicks, 255_997n);
    frame.frame.close();
    decoder.close();
  } finally {
    globalThis.VideoDecoder = original;
  }
});

/**
 * Presents frames 0 to 59 in order, each closed at once as a host does, and returns the frame index
 * each request answered with.
 */
async function playSixtyFrames(decoder, signal) {
  const answered = [];
  for (let frame = 0; frame < 60; frame += 1) {
    const decoded = await decoder.refill(
      {
        sourceId: "source",
        mediaTimeTicks: BigInt(frame) * 4_000n,
        reason: frame === 0 ? "seek" : "prefetch",
        generation: 1,
      },
      signal,
    );
    answered.push(Math.round(Number(decoded.decodedTimeTicks) / 4_000));
    decoded.frame.close();
  }
  return answered;
}

const inOrder = Array.from({ length: 60 }, (_, frame) => frame);

test("a decoder with five free surfaces plays on without stalling", async () => {
  // Chromium's Windows decoder leaves about five spare surfaces; eight cached frames would stall
  // output until the timeout.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = pooledDecoderClass(5);
  const decoder = new WebCodecsVideoDecoder("source", endlessSource(), {
    stallTimeoutMs: 10_000,
  });
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 3_000);
  try {
    assert.deepEqual(await playSixtyFrames(decoder, abort.signal), inOrder);
  } finally {
    clearTimeout(timer);
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("recovering from a stall keeps the target instead of answering with a later frame", async () => {
  // A recovery that closed the queued frames, the target among them, and read on would answer a
  // request for frame 5 with frame 10.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = pooledDecoderClass(5);
  const decoder = new WebCodecsVideoDecoder("source", endlessSource(), {
    maxCachedFrames: 8,
    stallTimeoutMs: 20,
  });
  try {
    assert.deepEqual(await playSixtyFrames(decoder), inOrder);
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("a cache of fewer than two frames is refused", () => {
  assert.throws(
    () =>
      new WebCodecsVideoDecoder("source", endlessSource(), {
        maxCachedFrames: 1,
      }),
    /at least 2/,
  );
});

test("a decoder that stalls with nothing left to release fails instead of hanging", async () => {
  // Unclosed host frames retain surfaces; waiting cannot release them.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = pooledDecoderClass(2);
  try {
    const decoder = new WebCodecsVideoDecoder("source", endlessSource(), {
      stallTimeoutMs: 50,
    });
    const request = (frame) => ({
      sourceId: "source",
      mediaTimeTicks: BigInt(frame) * 4_000n,
      reason: "prefetch",
      generation: 1,
    });
    const held = [
      await decoder.refill(request(0)),
      await decoder.refill(request(1)),
    ];
    await assert.rejects(decoder.refill(request(2)), /stalled/);
    for (const frame of held) frame.frame.close();
    decoder.close();
  } finally {
    globalThis.VideoDecoder = original;
  }
});

/**
 * Requests `frames` (in tick multiples of `step`) in order, each closed at once, and returns the
 * tick each answered with.
 */
async function answersAt(decoder, ticks, signal) {
  const answered = [];
  for (const [index, mediaTimeTicks] of ticks.entries()) {
    const decoded = await decoder.refill(
      {
        sourceId: "source",
        mediaTimeTicks,
        reason: index === 0 ? "seek" : "prefetch",
        generation: 1,
      },
      signal,
    );
    answered.push(decoded.decodedTimeTicks);
    decoded.frame.close();
  }
  return answered;
}

/** A 30 fps stream whose frame `index` is at `index * 4000` ticks, starting at frame `first`. */
function lateSource(first, frames = 40) {
  return {
    config: { codec: "fake" },
    chunksFrom: () =>
      (async function* chunks() {
        for (let index = first; index < first + frames; index += 1)
          yield { timestamp: Math.round((index * 1_000_000) / 30) };
      })(),
  };
}

test("requests before a late stream's first frame all answer with it, and then play on in place", async () => {
  // A request before the first frame must not take the next decoded frame, or every later answer
  // would run frames ahead.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  try {
    const frameRate = new WebCodecsVideoDecoder("source", lateSource(3));
    assert.deepEqual(
      await answersAt(
        frameRate,
        Array.from({ length: 12 }, (_, frame) => BigInt(frame) * 4_000n),
      ),
      [3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11].map(
        (frame) => BigInt(frame) * 4_000n,
      ),
    );
    frameRate.close();
    // A 60 fps timeline over the same stream: two requests per source frame, which must not run
    // away without bound.
    const doubled = new WebCodecsVideoDecoder("source", lateSource(3));
    assert.deepEqual(
      await answersAt(
        doubled,
        Array.from({ length: 16 }, (_, step) => BigInt(step) * 2_000n),
      ),
      [3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7].map(
        (frame) => BigInt(frame) * 4_000n,
      ),
    );
    doubled.close();
  } finally {
    globalThis.VideoDecoder = original;
  }
});

test("a read aborted mid-chunk does not leave the next request to decode past it", async () => {
  // The aborted read still consumed a chunk; resuming from the same cursor would skip it and decode
  // the frames after it without their reference.
  const original = globalThis.VideoDecoder;
  const fed = [];
  globalThis.VideoDecoder = class extends FakeVideoDecoder {
    decode(chunk) {
      fed.push(Math.round((chunk.timestamp * 30) / 1_000_000));
      super.decode(chunk);
    }
  };
  let cursors = 0;
  const source = {
    config: { codec: "fake" },
    chunksFrom: () => {
      cursors += 1;
      return (async function* chunks() {
        for (let index = 0; index < 30; index += 1) {
          await new Promise((resolve) => setTimeout(resolve, 20));
          yield { timestamp: Math.round((index * 1_000_000) / 30) };
        }
      })();
    },
  };
  const decoder = new WebCodecsVideoDecoder("source", source);
  const request = (frame) => ({
    sourceId: "source",
    mediaTimeTicks: BigInt(frame) * 4_000n,
    reason: frame === 0 ? "seek" : "prefetch",
    generation: 1,
  });
  try {
    for (const frame of [0, 1, 2]) {
      const decoded = await decoder.refill(request(frame));
      decoded.frame.close();
    }
    const aborted = decoder.refill(request(4), AbortSignal.timeout(5));
    await assert.rejects(aborted, /TimeoutError|AbortError|aborted/i);
    const after = await decoder.refill(request(5));
    assert.equal(after.decodedTimeTicks, 20_000n);
    after.frame.close();
    const sinceLastCursor = fed.slice(fed.lastIndexOf(0));
    assert.deepEqual(
      sinceLastCursor.slice(0, 6),
      [0, 1, 2, 3, 4, 5],
      `chunks fed ${fed}`,
    );
    assert.equal(cursors, 2, "the aborted cursor is not reused");
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("a decoder that outputs in decode order still answers each B-frame time with its own frame", async () => {
  // Safari 16.4 to 26.3 output H.264 B-frames in decode order: answering on the next reference
  // frame would map I0 P3 B1 B2 P6 B4 B5 to 0, 0, 0, 3, 3, 3.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  const decodeOrder = [];
  for (let reference = 0; reference < 60; reference += 3) {
    decodeOrder.push(reference);
    if (reference > 0) decodeOrder.push(reference - 2, reference - 1);
  }
  decodeOrder.push(58, 59);
  const source = {
    config: { codec: "avc1.64001f" },
    chunksFrom: () =>
      (async function* chunks() {
        for (const frame of decodeOrder)
          yield { timestamp: Math.round((frame * 1_000_000) / 30) };
      })(),
  };
  try {
    const decoder = new WebCodecsVideoDecoder("source", source);
    const frames = Array.from({ length: 24 }, (_, frame) => frame);
    assert.deepEqual(
      await answersAt(
        decoder,
        frames.map((frame) => BigInt(frame) * 4_000n),
      ),
      frames.map((frame) => BigInt(frame) * 4_000n),
    );
    decoder.close();
  } finally {
    globalThis.VideoDecoder = original;
  }
});

test("a codec with B-frames holds one frame back and still plays on five free surfaces", async () => {
  // Holding frames until a later one arrives costs one decoder surface; the Windows decoder's five
  // spare ones still leave room.
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = pooledDecoderClass(5);
  const decoder = new WebCodecsVideoDecoder(
    "source",
    { ...endlessSource(), config: { codec: "avc1.64001f" } },
    { stallTimeoutMs: 10_000 },
  );
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 3_000);
  try {
    assert.deepEqual(await playSixtyFrames(decoder, abort.signal), inOrder);
  } finally {
    clearTimeout(timer);
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("a stall timeout longer than a timer can hold is refused", () => {
  const source = { config: { codec: "fake" }, chunksFrom: () => [] };
  for (const stallTimeoutMs of [
    2 ** 31,
    2 ** 32 + 5_000,
    Number.MAX_SAFE_INTEGER,
  ])
    assert.throws(
      () => new WebCodecsVideoDecoder("source", source, { stallTimeoutMs }),
      (error) =>
        error instanceof RangeError &&
        /stallTimeoutMs .*2147483647/.test(error.message),
      String(stallTimeoutMs),
    );
  new WebCodecsVideoDecoder("source", source, {
    stallTimeoutMs: MAX_STALL_TIMEOUT_MS,
  }).close();
});

test("a picture held past the stream's end decodes nothing more once the end was reached", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = FakeVideoDecoder;
  // 90 frames at 30 fps in GOPs of 30: a seek decodes from the keyframe before the time.
  const frames = 90;
  const micros = (index) => Math.round((index * 1_000_000) / 30);
  let decoded = 0;
  const source = {
    config: { codec: "fake" },
    chunksFrom: (ticks) => {
      const index = Math.min(frames - 1, Number((ticks * 30n) / 120_000n));
      const key = index - (index % 30);
      return (async function* chunks() {
        for (let at = key; at < frames; at += 1) {
          decoded += 1;
          yield {
            timestamp: micros(at),
            type: at % 30 === 0 ? "key" : "delta",
          };
        }
      })();
    },
  };
  const decoder = new WebCodecsVideoDecoder("source", source);
  const request = (index) => ({
    sourceId: "source",
    mediaTimeTicks: BigInt(index) * 4_000n,
    reason: "prefetch",
    generation: 1,
  });
  try {
    for (let index = 0; index < frames; index += 1)
      (await decoder.refill(request(index))).frame.close();
    const decodes = [];
    for (let index = frames; index < frames + 10; index += 1) {
      const before = decoded;
      const answer = await decoder.refill(request(index));
      assert.equal(answer.decodedTimeTicks, BigInt(frames - 1) * 4_000n);
      answer.frame.close();
      decodes.push(decoded - before);
    }
    assert.deepEqual(decodes, new Array(10).fill(0));
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

test("each consumer of a source decodes forward through its own cursor", async () => {
  // Two clips read one source 30 s apart; each decoder records the times it is asked for.
  const opened = [];
  const open = (sourceId) => {
    const decoder = {
      times: [],
      closed: false,
      async refill(request) {
        decoder.times.push(request.mediaTimeTicks);
        return {
          sourceId,
          requestedTimeTicks: request.mediaTimeTicks,
          decodedTimeTicks: request.mediaTimeTicks,
          generation: request.generation,
          frame: new FakeVideoFrame(0),
        };
      },
      async flush() {},
      close() {
        decoder.closed = true;
      },
    };
    opened.push(decoder);
    return decoder;
  };
  const coordinator = new DecoderRefillCoordinator(open, async (frame) =>
    frame.frame.close(),
  );
  const request = (mediaTimeTicks, consumerId) => ({
    sourceId: "source",
    mediaTimeTicks,
    reason: "prefetch",
    generation: 0,
    consumerId,
  });
  try {
    for (let frame = 0n; frame < 4n; frame += 1n)
      await Promise.all([
        coordinator.refill(request(frame * 4_000n, "main")),
        coordinator.refill(request(3_600_000n + frame * 4_000n, "pip")),
      ]);
    assert.equal(opened.length, 2);
    assert.deepEqual(opened[0].times, [0n, 4_000n, 8_000n, 12_000n]);
    assert.deepEqual(opened[1].times, [
      3_600_000n,
      3_604_000n,
      3_608_000n,
      3_612_000n,
    ]);
  } finally {
    coordinator.close();
  }
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(
    opened.map(({ closed }) => closed),
    [true, true],
  );
});

test("a source keeps at most its cursor limit open, closing the one used least recently", async () => {
  const opened = [];
  const open = () => {
    const decoder = {
      closed: false,
      async refill(request) {
        return {
          sourceId: request.sourceId,
          requestedTimeTicks: request.mediaTimeTicks,
          decodedTimeTicks: request.mediaTimeTicks,
          generation: request.generation,
          frame: new FakeVideoFrame(0),
        };
      },
      async flush() {},
      close() {
        decoder.closed = true;
      },
    };
    opened.push(decoder);
    return decoder;
  };
  const coordinator = new DecoderRefillCoordinator(
    open,
    async (frame) => frame.frame.close(),
    { maxCursorsPerSource: 2 },
  );
  const request = (consumerId) => ({
    sourceId: "source",
    mediaTimeTicks: 0n,
    reason: "prefetch",
    generation: 0,
    consumerId,
  });
  try {
    await coordinator.refill(request("a"));
    await coordinator.refill(request("b"));
    await coordinator.refill(request("a"));
    await coordinator.refill(request("c"));
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.deepEqual(
      opened.map(({ closed }) => closed),
      [false, true, false],
      "b was used least recently",
    );
  } finally {
    coordinator.close();
  }
  assert.throws(
    () =>
      new DecoderRefillCoordinator(open, async () => {}, {
        maxCursorsPerSource: 0,
      }),
    RangeError,
  );
});

test("after a decoder error the next forward request decodes with a new decoder", async () => {
  // A transient EncodingError, or a reclaimed codec, closes the VideoDecoder and calls its error
  // callback; the request that met it fails, and play continues from the next one.
  const original = globalThis.VideoDecoder;
  let failAt = 4 * 33_333;
  let created = 0;
  globalThis.VideoDecoder = class extends EventTarget {
    decodeQueueSize = 0;
    state = "configured";
    #output;
    #error;
    #pending = [];
    constructor({ output, error }) {
      super();
      this.#output = output;
      this.#error = error;
      created += 1;
    }
    configure() {}
    decode(chunk) {
      if (chunk.timestamp === failAt) {
        failAt = -1;
        this.state = "closed";
        queueMicrotask(() =>
          this.#error(new DOMException("Decoding error.", "EncodingError")),
        );
        return;
      }
      this.decodeQueueSize += 1;
      this.#pending.push(
        Promise.resolve().then(() => {
          this.decodeQueueSize -= 1;
          this.#output(new FakeVideoFrame(chunk.timestamp));
          this.dispatchEvent(new Event("dequeue"));
        }),
      );
    }
    async flush() {
      await Promise.all(this.#pending.splice(0));
    }
    close() {
      this.state = "closed";
    }
  };
  const source = {
    config: { codec: "fake" },
    chunksFrom: (ticks) => {
      const first = Number((ticks * 30n) / 120_000n);
      return (async function* chunks() {
        for (let index = first; index < 30; index += 1)
          yield { timestamp: index * 33_333, type: "key" };
      })();
    },
  };
  const decoder = new WebCodecsVideoDecoder("source", source);
  const outcomes = [];
  try {
    for (let index = 0; index < 10; index += 1) {
      try {
        const answer = await decoder.refill({
          sourceId: "source",
          mediaTimeTicks: BigInt(index) * 4_000n,
          reason: "prefetch",
          generation: 1,
        });
        answer.frame.close();
        outcomes.push("ok");
      } catch (error) {
        outcomes.push(error.name);
      }
    }
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
  assert.deepEqual(outcomes, [
    "ok",
    "ok",
    "ok",
    "ok",
    "EncodingError",
    "ok",
    "ok",
    "ok",
    "ok",
    "ok",
  ]);
  assert.equal(created, 2);
});
