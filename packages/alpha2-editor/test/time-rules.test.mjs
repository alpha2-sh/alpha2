// The editor's ports of the engine's time rules run the engine's own table,
// `tests/golden/time-rules-corpus.json`, which `crates/alpha2-engine/tests/time_rules.rs` computes through
// the engine. An application shows keyframed values, and keys a clip's source composition, through these.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  clipSourceTime,
  clipTimelineTime,
  cubicBezierAt,
  frameAt,
  frameStart,
} from "../dist/index.js";
import { exportAudioWriter } from "../dist/browser/runtime/export-audio.js";

const corpus = JSON.parse(
  await readFile(
    new URL("../../../tests/golden/time-rules-corpus.json", import.meta.url),
    "utf8",
  ),
);

test("a tick falls in the frame the engine says, and that frame starts where the engine's does", () => {
  for (const { rate, cases } of corpus.frames)
    for (const { tick, frame, frameStart: start } of cases) {
      const label = `${rate.num}/${rate.den} at tick ${tick}`;
      assert.equal(frameAt(tick, rate), frame, label);
      assert.equal(frameStart(frame, rate), start, label);
    }
});

test("a curve's value is the engine's, bit for bit, steep curves included", () => {
  for (const { curve, cases } of corpus.curves)
    for (const [progress, value] of cases)
      assert.equal(
        cubicBezierAt(curve, progress),
        value,
        `cubic-bezier(${curve.x1}, ${curve.y1}, ${curve.x2}, ${curve.y2}) at ${progress}`,
      );
});

const clipOf = ({ timeline, source, timeMap }) => ({
  id: "c",
  source_id: "s",
  timeline_range: timeline,
  source_range: source,
  time_map: timeMap,
});
const describe = ({ rate, timeline, source, timeMap }) =>
  `${timeMap.type}${timeMap.mode ? ` ${timeMap.mode}` : ""} [${timeline.start}, ${timeline.end}) over [${source.start}, ${source.end}) at ${rate.num}/${rate.den}`;

test("a clip shows the source instant the engine draws, reversed and ping-pong passes mirrored by a frame", () => {
  for (const c of corpus.sourceTimes) {
    const clip = clipOf(c);
    for (const [tick, shown] of c.cases)
      assert.equal(
        clipSourceTime(clip, tick, c.rate),
        shown,
        `${describe(c)} at ${tick}`,
      );
  }
});

test("a source time is first shown at the tick the timeline clock seeks to", () => {
  for (const c of corpus.sourceTimes) {
    const clip = clipOf(c);
    const { type } = c.timeMap;
    if (type === "freeze") continue;
    const at = (tick) => clipSourceTime(clip, tick, c.rate);
    // Forward and the first pass of a loop advance; reverse retreats.
    const reached =
      type === "reverse" ? (shown, k) => shown <= k : (shown, k) => shown >= k;
    const firstPass =
      type === "loop"
        ? Math.min(
            c.timeline.end,
            c.timeline.start + c.source.end - c.source.start,
          )
        : c.timeline.end;
    for (const [tick] of c.cases) {
      if (tick >= firstPass) continue;
      const k = at(tick);
      const first = clipTimelineTime(clip, k, c.rate);
      const label = `${describe(c)} source ${k}`;
      assert.ok(reached(at(first), k), `${label}: ${first} shows ${at(first)}`);
      if (first > c.timeline.start)
        assert.ok(
          !reached(at(first - 1), k),
          `${label}: ${first - 1} shows it too`,
        );
    }
  }
});

test("export audio is mixed from the samples the engine's export covers, not the request", async () => {
  // A 30 fps in-point at 1 s, exported after the sequence became 29.97 fps: the export starts a
  // frame earlier, at tick 116116.
  const window = corpus.exports.find(
    (c) =>
      c.rate.num === 30000 &&
      c.sampleRate === 48000 &&
      c.start === 120000 &&
      c.end === 240000,
  );
  assert.equal(window.effectiveStart, 116116);
  const mixed = [];
  const written = [];
  const engine = {
    exportProgress: () =>
      JSON.stringify({
        completed: 0,
        total: window.total,
        effectiveRange: {
          startTicks: String(window.effectiveStart),
          endTicks: String(window.effectiveEnd),
        },
        audioWindow: {
          startSample: String(window.audioStartSample),
          frames: window.audioFrames,
        },
      }),
    audioMaxBlock: 4096,
    renderAudio(start, frames) {
      mixed.push([Number(start), frames]);
      return new Float32Array(frames * 2);
    },
    audioChannels: () => 2,
    audioMissingSources: () => "[]",
  };
  const writeThrough = exportAudioWriter({
    engine,
    audioSupplier: { supply: async () => {} },
    sink: {
      writeAudio: async ({ startSample, channels }) =>
        written.push([Number(startSample), channels[0].length]),
    },
    sampleRate: 48000,
    metrics: {
      audioDecodeMs: 0,
      audioMixMs: 0,
      audioEncodeMs: 0,
      audioBlocksInterleaved: 0,
    },
    enter: () => {},
  });
  await writeThrough(Infinity);
  // The sample nearest tick 116116, not the request's 48000.
  assert.equal(mixed[0][0], 46446);
  assert.equal(mixed[0][0], window.audioStartSample);
  const frames = mixed.reduce((sum, [, count]) => sum + count, 0);
  assert.equal(frames, window.audioFrames);
  assert.equal(
    mixed.at(-1)[0] + mixed.at(-1)[1],
    window.audioStartSample + window.audioFrames,
  );
  // Written from the output's zero, where the first frame is.
  assert.equal(written[0][0], 0);
});
