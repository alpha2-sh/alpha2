import test from "node:test";
import assert from "node:assert/strict";

import {
  AUDIO_RING_CONTROL_BYTES,
  AudioPlaybackClock,
  AudioPump,
  AudioRingBuffer,
  encodeMasterAudio,
} from "../dist/audio-timeline.js";
import { configureEngineMixedAudio } from "../dist/audio-worklet.js";

/**
 * Attaches a ring without the cross-origin-isolation check, which Node cannot satisfy. The shared
 * buffers themselves behave identically.
 */
function ring(channels, capacity) {
  return AudioRingBuffer.attach({
    control: new SharedArrayBuffer(AUDIO_RING_CONTROL_BYTES),
    samples: new SharedArrayBuffer(
      channels * capacity * Float32Array.BYTES_PER_ELEMENT,
    ),
    channels,
    capacity,
  });
}

function planes(channels, frames, fill) {
  return Array.from({ length: channels }, (_, channel) =>
    Float32Array.from({ length: frames }, (_, index) => fill(channel, index)),
  );
}

test("a ring round trips planar samples in order", () => {
  const buffer = ring(2, 16);
  assert.equal(buffer.available, 0);
  assert.equal(buffer.writable, 16);

  const written = buffer.write(
    planes(2, 8, (channel, index) => channel * 100 + index),
    8,
  );
  assert.equal(written, 8);
  assert.equal(buffer.available, 8);

  const out = planes(2, 8, () => -1);
  assert.equal(buffer.read(out, 8), 8);
  assert.deepEqual([...out[0]], [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.deepEqual([...out[1]], [100, 101, 102, 103, 104, 105, 106, 107]);
  assert.equal(buffer.available, 0);
});

test("a ring wraps around its capacity without losing samples", () => {
  const buffer = ring(1, 8);
  buffer.write(
    planes(1, 6, (_, index) => index),
    6,
  );
  const first = planes(1, 5, () => -1);
  buffer.read(first, 5);

  // Five samples are free at the start of the storage, so this write wraps.
  buffer.write(
    planes(1, 5, (_, index) => 100 + index),
    5,
  );
  const rest = planes(1, 6, () => -1);
  assert.equal(buffer.read(rest, 6), 6);
  assert.deepEqual([...rest[0]], [5, 100, 101, 102, 103, 104]);
});

test("a ring refuses to overrun and reports what it accepted", () => {
  const buffer = ring(1, 4);
  assert.equal(
    buffer.write(
      planes(1, 10, () => 1),
      10,
    ),
    4,
  );
  assert.equal(buffer.writable, 0);
  assert.equal(
    buffer.write(
      planes(1, 1, () => 1),
      1,
    ),
    0,
  );
});

test("an underrun produces silence rather than stale audio", () => {
  const buffer = ring(1, 8);
  buffer.write(
    planes(1, 2, () => 0.5),
    2,
  );
  const out = planes(1, 4, () => -1);
  assert.equal(buffer.read(out, 4), 2);
  assert.deepEqual([...out[0]], [0.5, 0.5, 0, 0]);
});

test("clearing drops buffered audio for both sides", () => {
  const buffer = ring(1, 8);
  buffer.write(
    planes(1, 6, () => 1),
    6,
  );
  buffer.clear();
  assert.equal(buffer.available, 0);
  // The discarded samples hold their slots until the reader moves past them, but audio for the new
  // position can be written at once.
  assert.equal(buffer.writable, 2);
  buffer.write(
    planes(1, 2, () => 2),
    2,
  );
  const out = planes(1, 4, () => -1);
  assert.equal(buffer.read(out, 4), 2);
  assert.deepEqual([...out[0]], [2, 2, 0, 0]);
  assert.equal(buffer.writable, 8);
});

test("a ring's capacity is a power of two", () => {
  assert.throws(() => ring(1, 24), RangeError);
});

test("a mono ring feeds every requested output channel", () => {
  const buffer = ring(1, 8);
  buffer.write(
    planes(1, 4, (_, index) => index),
    4,
  );
  const out = planes(2, 4, () => -1);
  assert.equal(buffer.read(out, 4), 4);
  assert.deepEqual([...out[0]], [...out[1]]);
});

function source(channels, onMix) {
  return {
    sampleRate: 48_000,
    channels,
    mix: async (startSample, frames) => onMix(startSample, frames),
  };
}

test("a pump fills the ring to its target and advances its position", async () => {
  const buffer = ring(2, 64);
  const calls = [];
  const pump = new AudioPump(
    source(2, (startSample, frames) => {
      calls.push([startSample, frames]);
      return { kind: "mixed", planes: planes(2, frames, () => 0.25) };
    }),
    buffer,
    0,
    { blockFrames: 16, targetFrames: 32 },
  );

  const written = await pump.fill();
  assert.equal(written, 32);
  assert.equal(pump.position, 32);
  assert.deepEqual(calls, [
    [0, 16],
    [16, 16],
  ]);
  assert.equal(buffer.available, 32);

  // A second fill tops the ring back up only after the consumer drains it.
  assert.equal(await pump.fill(), 0);
  buffer.read(
    planes(2, 16, () => 0),
    16,
  );
  assert.equal(await pump.fill(), 16);
});

test("a pump stops at missing media and reports it once", async () => {
  const buffer = ring(1, 32);
  const missing = [];
  const pump = new AudioPump(
    source(1, (startSample) =>
      startSample === 0
        ? { kind: "mixed", planes: planes(1, 8, () => 1) }
        : { kind: "waiting", missingSources: ["music"] },
    ),
    buffer,
    0,
    {
      blockFrames: 8,
      targetFrames: 24,
      onRefill: (sources) => {
        missing.push(...sources);
      },
    },
  );

  assert.equal(await pump.fill(), 8);
  assert.deepEqual(missing, ["music"]);
  // The position never advances past audio that was not mixed.
  assert.equal(pump.position, 8);
});

test("seeking moves the pump and discards buffered audio", async () => {
  const buffer = ring(1, 32);
  const pump = new AudioPump(
    source(1, (_, frames) => ({
      kind: "mixed",
      planes: planes(1, frames, () => 1),
    })),
    buffer,
    0,
    { blockFrames: 8, targetFrames: 16 },
  );
  await pump.fill();
  assert.ok(buffer.available > 0);

  pump.seek(48_000);
  assert.equal(pump.position, 48_000);
  assert.equal(buffer.available, 0);
  assert.throws(() => pump.seek(-1), RangeError);
});

test("a seek while a block is mixing drops that block and plays the target first", async () => {
  const buffer = ring(1, 64);
  const calls = [];
  const pending = [];
  // Every sample is its absolute index, so a read shows where the audio came from.
  const pump = new AudioPump(
    {
      sampleRate: 48_000,
      channels: 1,
      mix: (startSample, frames, signal) => {
        calls.push([startSample, frames, signal]);
        return new Promise((resolve) => {
          pending.push(() =>
            resolve({
              kind: "mixed",
              planes: planes(1, frames, (_, index) => startSample + index),
            }),
          );
        });
      },
    },
    buffer,
    0,
    { blockFrames: 8, targetFrames: 16 },
  );
  const filling = pump.fill();
  await Promise.resolve();
  assert.equal(calls.length, 1);
  pump.seek(1_000);
  assert.equal(calls[0][2]?.aborted, true, "the seek aborts the stale mix");
  // A fill asked for now finds the running one, which carries on from the target.
  assert.equal(await pump.fill(), 0);
  pending.shift()();
  for (let block = 0; block < 2; block += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
    pending.shift()();
  }
  await filling;
  assert.deepEqual(
    calls.map(([start, frames]) => [start, frames]),
    [
      [0, 8],
      [1_000, 8],
      [1_008, 8],
    ],
  );
  assert.equal(pump.position, 1_016);
  const out = planes(1, 16, () => -1);
  assert.equal(buffer.read(out, 16), 16);
  assert.equal(out[0][0], 1_000, "the first sample heard is the seek target");
  assert.equal(out[0][15], 1_015);
});

test("a seek while the pump waits for media mixes the target, not the old position", async () => {
  const buffer = ring(1, 64);
  const calls = [];
  let pump;
  pump = new AudioPump(
    source(1, (startSample, frames) => {
      calls.push(startSample);
      return startSample === 0
        ? { kind: "waiting", missingSources: ["music"] }
        : {
            kind: "mixed",
            planes: planes(1, frames, (_, index) => startSample + index),
          };
    }),
    buffer,
    0,
    {
      blockFrames: 8,
      targetFrames: 8,
      onRefill: () => pump.seek(500),
    },
  );
  assert.equal(await pump.fill(), 8);
  assert.deepEqual(calls, [0, 500]);
  assert.equal(pump.position, 508);
});

test("a rejected mix that a seek made stale does not fail the fill", async () => {
  const buffer = ring(1, 64);
  let pump;
  pump = new AudioPump(
    {
      sampleRate: 48_000,
      channels: 1,
      mix: async (startSample, frames, signal) => {
        if (startSample === 0) {
          pump.seek(200);
          throw signal.reason;
        }
        return { kind: "mixed", planes: planes(1, frames, () => 1) };
      },
    },
    buffer,
    0,
    { blockFrames: 8, targetFrames: 8 },
  );
  assert.equal(await pump.fill(), 8);
  assert.equal(pump.position, 208);
});

test("a ring sized from an odd sample rate rounds its capacity up", () => {
  const isolated = globalThis.crossOriginIsolated;
  globalThis.crossOriginIsolated = true;
  try {
    // 11 025 Hz / 2: half a second at a supported odd rate.
    assert.equal(AudioRingBuffer.create(2, 11_025 / 2).capacity, 8_192);
    assert.equal(AudioRingBuffer.create(1, 2.5).capacity, 4);
    for (const capacity of [Number.NaN, Infinity, 1.5, 2 ** 30 + 1])
      assert.throws(() => AudioRingBuffer.create(1, capacity), RangeError);
  } finally {
    globalThis.crossOriginIsolated = isolated;
  }
});

test("a frame count that is not a whole number is refused before it moves an index", () => {
  const buffer = ring(1, 16);
  buffer.write(
    planes(1, 8, () => 1),
    8,
  );
  buffer.read(
    planes(1, 8, () => 0),
    8,
  );
  for (const frames of [Number.NaN, 2.5, -1, Infinity]) {
    assert.throws(
      () =>
        buffer.write(
          planes(1, 8, () => 1),
          frames,
        ),
      RangeError,
    );
    assert.throws(
      () =>
        buffer.read(
          planes(1, 8, () => 0),
          frames,
        ),
      RangeError,
    );
  }
  assert.equal(
    buffer.write(
      planes(1, 4, () => 2),
      4,
    ),
    4,
  );
  const out = planes(1, 4, () => 0);
  assert.equal(buffer.read(out, 4), 4);
  assert.deepEqual([...out[0]], [2, 2, 2, 2]);
});

test("a pump refuses a block or target that is not a positive whole number", () => {
  const mixer = source(1, (_, frames) => ({
    kind: "mixed",
    planes: planes(1, frames, () => 0),
  }));
  for (const blockFrames of [220.5, Number.NaN, 0, -8, Infinity])
    assert.throws(
      () => new AudioPump(mixer, ring(1, 64), 0, { blockFrames }),
      RangeError,
    );
  for (const targetFrames of [Number.NaN, 0.5, 0, -1])
    assert.throws(
      () => new AudioPump(mixer, ring(1, 64), 0, { targetFrames }),
      RangeError,
    );
});

test("the master record matches the layout the worklet decodes", () => {
  const bytes = encodeMasterAudio({
    gain: { mode: "constant", value: 0.5 },
    limiter: { threshold: 1, release_ms: 50 },
    effects: [
      { type: "reverb", room_size: 0.5, damping: 0.25, mix: 0.125 },
      {
        type: "eq",
        bands: [{ kind: "high_pass", frequency_hz: 120, q: 0.7 }],
      },
    ],
  });

  const view = new DataView(bytes.buffer);
  assert.equal(view.getUint32(0, true), 1, "version");
  assert.equal(view.getFloat32(4, true), 0.5, "master gain");
  assert.equal(bytes[8], 1, "limiter present");
  assert.equal(view.getFloat32(9, true), 1, "limiter threshold");
  assert.equal(view.getFloat32(13, true), 50, "limiter release");
  assert.equal(view.getUint32(17, true), 2, "node count");

  // Node 0: reverb.
  assert.equal(bytes[21], 2);
  assert.equal(view.getFloat32(22, true), 0.5);
  assert.equal(view.getFloat32(26, true), 0.25);
  assert.equal(view.getFloat32(30, true), 0.125);
  // Node 1: one EQ band, expanded from the band list.
  assert.equal(bytes[34], 0);
  assert.equal(bytes[35], 2, "high_pass");
  assert.equal(view.getFloat32(36, true), 120);
  assert.equal(view.getFloat32(40, true), Math.fround(0.7));
  assert.equal(view.getFloat32(44, true), 0, "default band gain");
  assert.equal(bytes.length, 48);
});

test("a master bus without a limiter keeps the record fixed width", () => {
  const bytes = encodeMasterAudio({ limiter: null });
  assert.equal(bytes[8], 0);
  assert.equal(
    new DataView(bytes.buffer).getFloat32(4, true),
    1,
    "default gain",
  );
  assert.equal(bytes.length, 21);
  assert.equal(encodeMasterAudio(undefined).length, 21);
});

test("a master that leaves the limiter out keeps the schema's default limiter", () => {
  // Only `null` removes the limiter; an omitted one is the 0 dBFS, 50 ms default the engine
  // applies.
  for (const master of [undefined, {}, { effects: [] }]) {
    const bytes = encodeMasterAudio(master);
    const view = new DataView(bytes.buffer);
    assert.equal(bytes[8], 1, `${JSON.stringify(master)}: limiter present`);
    assert.equal(view.getFloat32(9, true), 1, "default threshold");
    assert.equal(view.getFloat32(13, true), 50, "default release");
    assert.equal(view.getFloat32(4, true), 1, "default gain");
  }
  assert.equal(encodeMasterAudio({ limiter: null })[8], 0);
});

test("a keyframed master gain is refused, not played at unity", () => {
  assert.throws(
    () =>
      encodeMasterAudio({
        gain: {
          mode: "keyframes",
          keyframes: [
            { time: 0, value: 1 },
            { time: 120_000, value: 0 },
          ],
        },
      }),
    (error) =>
      error instanceof RangeError &&
      /keyframed master gain/.test(error.message),
  );
});

test("every node kind keeps its layout", () => {
  // The same bytes `EVERY_NODE_RECORD` in bindings/alpha2-audio-worklet/src/tests.rs decodes field
  // by field. Every node and band kind, no field at a default.
  const bytes = encodeMasterAudio({
    gain: { mode: "constant", value: 0.75 },
    limiter: { threshold: 0.875, release_ms: 80 },
    effects: [
      {
        type: "compressor",
        threshold_db: -18,
        ratio: 3,
        attack_ms: 5,
        release_ms: 120,
        lookahead_ms: 2.5,
      },
      {
        type: "eq",
        bands: [
          { kind: "peaking", frequency_hz: 1000, q: 1.5, gain_db: 6 },
          { kind: "low_pass", frequency_hz: 8192, q: 0.5, gain_db: -3 },
        ],
      },
      { type: "reverb", room_size: 0.5, damping: 0.25, mix: 0.125 },
      {
        type: "eq",
        bands: [{ kind: "high_pass", frequency_hz: 120, q: 0.75 }],
      },
    ],
  });
  assert.equal(
    Buffer.from(bytes).toString("hex"),
    [
      "01000000",
      "0000403f",
      "01",
      "0000603f",
      "0000a042",
      "05000000",
      "01",
      "000090c1",
      "00004040",
      "0000a040",
      "0000f042",
      "00002040",
      "00",
      "00",
      "00007a44",
      "0000c03f",
      "0000c040",
      "00",
      "01",
      "00000046",
      "0000003f",
      "000040c0",
      "02",
      "0000003f",
      "0000803e",
      "0000003e",
      "00",
      "02",
      "0000f042",
      "0000403f",
      "00000000",
    ].join(""),
  );
});

test("engine-mixed preview configures a transparent worklet chain", async () => {
  let message;
  const node = new EventTarget();
  node.port = new EventTarget();
  node.port.start = () => {};
  node.port.postMessage = (value) => {
    message = value;
    node.port.dispatchEvent(
      new MessageEvent("message", {
        data: { type: "alpha2-applied", id: value.id },
      }),
    );
  };
  await configureEngineMixedAudio(node);
  assert.equal(message.type, "alpha2-config");
  const bytes = message.config;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  assert.equal(view.getFloat32(4, true), 1, "unity gain");
  assert.equal(bytes[8], 0, "limiter disabled");
  assert.equal(view.getUint32(17, true), 0, "no effects");
});

test("the playback clock is the audio the listener hears, not what was mixed", async () => {
  const buffer = ring(1, 4096);
  const pump = new AudioPump(
    source(1, (_, frames) => ({
      kind: "mixed",
      planes: planes(1, frames, () => 0),
    })),
    buffer,
    48_000,
    { blockFrames: 512, targetFrames: 2048 },
  );
  const clock = new AudioPlaybackClock(pump, buffer, {
    sampleRate: 48_000,
    outputLatency: 0.01,
    baseLatency: 0.0,
  });
  await pump.fill();
  // Mixed ahead but nothing handed to the audio thread: the start holds.
  assert.equal(pump.position, 48_000 + 2048);
  assert.equal(clock.sample, 48_000);
  assert.equal(clock.timelineTicks, 120_000n);
  // The audio thread takes 1024 samples; 480 of them are still on their way to the speaker.
  buffer.read(
    planes(1, 1024, () => 0),
    1024,
  );
  assert.equal(clock.sample, 48_000 + 1024 - 480);
  pump.seek(96_000);
  assert.equal(clock.sample, 96_000);
});

test("the playback clock counts the project's samples when the context runs at another rate", async () => {
  // A 48 kHz project in a 44.1 kHz context: the pump counts project samples, so 10 ms of output is
  // 480 of them, while the node's delay is in context samples.
  const buffer = ring(1, 4096);
  const pump = new AudioPump(
    source(1, (_, frames) => ({
      kind: "mixed",
      planes: planes(1, frames, () => 0),
    })),
    buffer,
    48_000,
    { blockFrames: 512, targetFrames: 2048 },
  );
  const processing = { latencySamples: 0 };
  const clock = new AudioPlaybackClock(
    pump,
    buffer,
    { sampleRate: 44_100, outputLatency: 0.01 },
    processing,
  );
  await pump.fill();
  assert.equal(pump.sampleRate, 48_000);
  assert.equal(clock.sample, 48_000);
  assert.equal(clock.timelineTicks, 120_000n);
  buffer.read(
    planes(1, 2048, () => 0),
    2048,
  );
  assert.equal(clock.sample, 48_000 + 2048 - 480);
  // 441 samples of the context's are 480 of the project's.
  processing.latencySamples = 441;
  assert.equal(clock.sample, 48_000 + 2048 - 480 - 480);
});

test("the playback clock subtracts the delay of what plays the ring", async () => {
  const buffer = ring(1, 4096);
  const pump = new AudioPump(
    source(1, (_, frames) => ({
      kind: "mixed",
      planes: planes(1, frames, () => 0),
    })),
    buffer,
    0,
    { blockFrames: 512, targetFrames: 2048 },
  );
  // A worklet master with 10 ms of compressor lookahead, whose delay changes with its master.
  const processing = { latencySamples: 480 };
  const clock = new AudioPlaybackClock(
    pump,
    buffer,
    { sampleRate: 48_000, outputLatency: 0.01 },
    processing,
  );
  await pump.fill();
  buffer.read(
    planes(1, 2048, () => 0),
    2048,
  );
  // 2048 handed over, 480 on the way to the speaker and 480 held in the lookahead.
  assert.equal(clock.sample, 2048 - 480 - 480);
  processing.latencySamples = 0;
  assert.equal(clock.sample, 2048 - 480);
});

test("video driven by the playback clock does not drift from the audio device", async () => {
  // Ten minutes of 30 fps playback with the audio device's crystal running 100 ppm fast against the
  // system clock. Video timed by the wall clock (open loop) falls behind the sound by 60 ms, almost
  // two frames; timed by the clock, it stays with what the device has played.
  const rate = 48_000;
  const buffer = ring(2, 16_384);
  const pump = new AudioPump(
    source(2, (_, frames) => ({
      kind: "mixed",
      planes: planes(2, frames, () => 0),
    })),
    buffer,
    0,
    { blockFrames: 1024, targetFrames: 8192 },
  );
  const latency = 0.02;
  const clock = new AudioPlaybackClock(pump, buffer, {
    sampleRate: rate,
    outputLatency: latency,
  });
  const deviceRate = rate * (1 + 100e-6);
  const quantum = planes(2, 128, () => 0);
  let played = 0;
  let worstClock = 0;
  let worstWall = 0;
  for (let frame = 1; frame <= 30 * 600; frame += 1) {
    const wallSeconds = frame / 30;
    await pump.fill();
    // The device has taken whole render quanta up to its own time.
    while (played + 128 <= Math.floor(wallSeconds * deviceRate)) {
      buffer.read(quantum, 128);
      played += 128;
    }
    const heard = played - latency * rate;
    worstClock = Math.max(worstClock, Math.abs(clock.sample - heard));
    worstWall = Math.max(
      worstWall,
      Math.abs((wallSeconds - latency) * rate - heard),
    );
  }
  const frameSamples = rate / 30;
  assert.ok(
    worstClock <= 1,
    `clock drifted ${worstClock} samples from the device`,
  );
  assert.ok(
    worstWall > frameSamples,
    `wall clock drifted only ${worstWall} samples`,
  );
});
