// The audio clock never reports a tick before the one Play was pressed at, so the first frame shown
// is not the one before it.
import test from "node:test";
import assert from "node:assert/strict";

import {
  AudioPlaybackClock,
  AudioPump,
  AudioRingBuffer,
} from "alpha2/audio-timeline";
import { frameAt, frameStart, sampleStart } from "../dist/index.js";
import { playbackStartSample } from "../dist/browser/runtime/audio.js";

const RATES = [
  { num: 24_000, den: 1_001 },
  { num: 30_000, den: 1_001 },
  { num: 60_000, den: 1_001 },
  { num: 25, den: 1 },
  { num: 30, den: 1 },
  { num: 50, den: 1 },
];

test("audio playback from a frame start is heard in that frame until it plays on", () => {
  const source = {
    sampleRate: 48_000,
    channels: 2,
    mix: async () => ({ kind: "mixed", planes: [] }),
  };
  // Node has shared memory but is not a cross-origin isolated page, which the ring requires.
  const isolated = globalThis.crossOriginIsolated;
  globalThis.crossOriginIsolated = true;
  const ring = AudioRingBuffer.create(2, 4_096);
  globalThis.crossOriginIsolated = isolated;
  for (const sampleRate of [44_100, 48_000]) {
    for (const fps of RATES) {
      for (let frame = 0; frame < 1_000; frame += 1) {
        const tick = frameStart(frame, fps);
        const start = playbackStartSample(tick, sampleRate);
        const where = `${fps.num}/${fps.den} fps at ${sampleRate} Hz, frame ${frame} (tick ${tick})`;
        assert.ok(sampleStart(start, sampleRate) >= tick, where);
        assert.ok(
          start === 0 || sampleStart(start - 1, sampleRate) < tick,
          where,
        );
        const pump = new AudioPump({ ...source, sampleRate }, ring, start);
        const clock = new AudioPlaybackClock(pump, ring, {
          sampleRate,
          outputLatency: 0.02,
          baseLatency: 0.005,
        });
        const heard = Number(clock.timelineTicks);
        assert.ok(heard >= tick, `${where}: heard tick ${heard}`);
        assert.equal(frameAt(heard, fps), frame, where);
      }
    }
  }
});

test("a start before the timeline is its first sample", () => {
  assert.equal(playbackStartSample(0, 48_000), 0);
  assert.equal(playbackStartSample(-5, 48_000), 0);
  assert.equal(playbackStartSample(1, 48_000), 1);
  assert.equal(playbackStartSample(120_000, 44_100), 44_100);
});
