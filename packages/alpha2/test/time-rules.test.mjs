// The TypeScript ports of the engine time rules run `tests/golden/time-rules-corpus.json`,
// which `crates/alpha2-engine/tests/time_rules.rs` computes through the engine.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { Alpha2Exporter } from "../dist/exporter.js";
import { samplesToTicks, secondsToTicks } from "../dist/project.js";
import {
  audioFrameGrid,
  placeOnAudioFrameGrid,
  presentationSlackTicks,
  roundingQuantum,
} from "../dist/media.js";

const corpus = JSON.parse(
  await readFile(
    new URL("../../../tests/golden/time-rules-corpus.json", import.meta.url),
    "utf8",
  ),
);

const backend = {
  begin: async () => ({
    kind: "waiting",
    missingSources: [],
    missingFrames: [],
  }),
  isDeviceLost: () => false,
  abort: () => {},
};
const rational = ({ num, den }) => ({ num: BigInt(num), den: BigInt(den) });

test("an export starts at the frame the engine says holds its start", () => {
  for (const { rate, cases } of corpus.frames)
    for (const { tick, frameStart } of cases) {
      const exporter = new Alpha2Exporter(
        backend,
        rational(rate),
        BigInt(tick),
        BigInt(tick) + 1n,
      );
      assert.equal(
        exporter.effectiveRange.startTicks,
        BigInt(frameStart),
        `${rate.num}/${rate.den} at tick ${tick}`,
      );
    }
});

test("an export covers the range, frames, and audio samples the engine's does", () => {
  for (const c of corpus.exports) {
    const exporter = new Alpha2Exporter(
      backend,
      rational(c.rate),
      BigInt(c.start),
      BigInt(c.end),
    );
    const label = `${c.rate.num}/${c.rate.den} [${c.start}, ${c.end}) at ${c.sampleRate} Hz`;
    assert.deepEqual(
      exporter.effectiveRange,
      {
        startTicks: BigInt(c.effectiveStart),
        endTicks: BigInt(c.effectiveEnd),
      },
      label,
    );
    assert.equal(exporter.progress.total, BigInt(c.total), label);
    assert.deepEqual(
      exporter.audioWindow(c.sampleRate),
      {
        startSample: BigInt(c.audioStartSample),
        frames: BigInt(c.audioFrames),
      },
      label,
    );
  }
});

test("a sample is the tick at or before it, negative samples included", () => {
  for (const [sample, sampleRate, ticks] of corpus.samplesToTicks)
    assert.equal(
      samplesToTicks(sample, sampleRate),
      ticks,
      `${sample} at ${sampleRate} Hz`,
    );
});

test("seconds read as the tick nearest them, so an exact time keeps its own tick", () => {
  for (const [seconds, ticks] of corpus.secondsToTicks)
    assert.equal(secondsToTicks(seconds), ticks, `${seconds} s`);
});

test("a stream's first timestamps show the rounding, and give the slack, the engine's do", () => {
  for (const {
    timeBase,
    timestamps,
    quantum,
    slackTicks,
  } of corpus.presentationSlack) {
    const label = `${timeBase.num}/${timeBase.den} [${timestamps.slice(0, 4)}...]`;
    assert.equal(roundingQuantum(timestamps), quantum, label);
    // The browser knows a time base as a resolution, units per second.
    const resolution =
      timeBase.num > 0 && timeBase.den > 0
        ? timeBase.den / timeBase.num
        : undefined;
    assert.equal(
      presentationSlackTicks(resolution, quantum),
      BigInt(slackTicks),
      label,
    );
  }
});

test("an audio stream's first timestamps give the frame grid the engine's do", () => {
  for (const {
    timeResolution,
    sampleRate,
    timestamps,
    grid,
    placements,
  } of corpus.audioFrameGrid) {
    const label = `1/${timeResolution} at ${sampleRate} Hz [${timestamps.slice(0, 4)}...]`;
    const found = audioFrameGrid(timestamps, timeResolution, sampleRate);
    assert.deepEqual(found ?? null, grid, label);
    for (const [at, frames, placed] of placements)
      assert.equal(
        placeOnAudioFrameGrid(found, at, frames),
        placed,
        `${label}: ${frames} samples at ${at}`,
      );
  }
});
