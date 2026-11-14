import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import { applyEdit } from "../dist/apply-edit.js";
import { insert, retime, rippleDelete, rippleTrim } from "../dist/edit.js";
import { validateProject } from "../dist/project.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// Skipped before `npm run build`; ALPHA2_REQUIRE=wasm makes that a failure.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

async function binding() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const T = 4000; // one frame at 30 fps
const at = (frames) => frames * T;
const plain = (value) => JSON.parse(JSON.stringify(value));
/** The engine's canonical form of `project`, which writes every default. */
const canonical = (wasm, project) =>
  applyEdit(wasm, plain(project), { op: "bake", targets: [] }).project;

const curves = {
  hold: { type: "hold" },
  linear: { type: "linear" },
  eased: { type: "cubic_bezier", x1: 0.42, y1: 0, x2: 0.58, y2: 1 },
  overshoot: { type: "cubic_bezier", x1: 0.3, y1: -0.4, x2: 0.6, y2: 1.3 },
};
const keys = (curve, points) => ({
  mode: "keyframes",
  keyframes: points.map(([time, value]) => ({
    time: at(time),
    value,
    interpolation: curves[curve],
  })),
});
const clip = (id, source, start, end) => ({
  id,
  source_id: source,
  timeline_range: { start: at(start), end: at(end) },
  source_range: { start: at(start), end: at(end) },
  time_map: { type: "forward" },
});

/** A video track and an audio track (both reached by a ripple), with keyed buses and master. */
function project(curve) {
  return {
    schema_version: 1,
    id: "p",
    name: "Ripple keys",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "s", kind: "video", locator: "test://s" },
      { id: "au", kind: "audio", locator: "test://au" },
    ],
    timeline: {
      tracks: [
        {
          id: "v1",
          name: "V1",
          kind: "video",
          clips: [clip("A", "s", 0, 100), clip("B", "s", 100, 160)],
          audio: {
            gain: keys(curve, [
              [0, 0.2],
              [130, 1],
              [200, 0.4],
            ]),
          },
        },
        {
          id: "a1",
          name: "A1",
          kind: "audio",
          clips: [clip("Z", "au", 160, 200)],
          audio: {
            gain: keys(curve, [
              [10, 1],
              [70, 0.3],
              [150, 0.9],
            ]),
            pan: keys(curve, [
              [0, -0.5],
              [120, 0.5],
            ]),
          },
        },
      ],
    },
    audio: {
      gain: keys(curve, [
        [0, 0.5],
        [90, 1],
        [180, 0.25],
      ]),
    },
  };
}

const removeRange = (start, end, clipId) => ({
  op: "remove_ranges",
  ranges: { list: [{ start, end }], anchor: { type: "timeline" } },
  target: { clips: [clipId] },
  ripple: "timeline",
});

for (const curve of Object.keys(curves))
  test(
    `a ripple that reaches every track moves ${curve} bus keys as remove_ranges does`,
    { skip },
    async () => {
      const wasm = await binding();
      const source = project(curve);
      assert.deepEqual(validateProject(source), []);
      const sync = { syncTracks: ["a1"] };
      const cases = [
        [rippleDelete(source, "A", { syncTracks: ["a1"] }), [0, at(100)], "A"],
        [rippleTrim(source, "A", "end", at(60), sync), [at(60), at(100)], "A"],
        [rippleTrim(source, "A", "start", at(25), sync), [0, at(25)], "A"],
        [
          retime(source, "B", at(30), { ripple: true, ...sync }),
          [at(130), at(160)],
          "B",
        ],
      ];
      for (const [result, [start, end], target] of cases) {
        assert.ok(result.ok, JSON.stringify(result.error));
        assert.deepEqual(result.timeline, {
          removed: { start, end },
          everyTrack: true,
        });
        const engine = applyEdit(
          wasm,
          plain(source),
          removeRange(start, end, target),
        ).project;
        const mine = canonical(wasm, result.project);
        const buses = (project) => [
          project.timeline.tracks.map((track) => track.audio),
          project.audio,
        ];
        assert.deepEqual(
          buses(mine),
          buses(engine),
          `${curve} ${start}..${end}`,
        );
      }
    },
  );

test("a ripple that reaches only some tracks moves their bus keys and not the master's", () => {
  const source = project("eased");
  const result = rippleDelete(source, "A");
  assert.ok(result.ok, JSON.stringify(result.error));
  assert.deepEqual(result.timeline, {
    removed: { start: 0, end: at(100) },
    everyTrack: false,
  });
  assert.notDeepEqual(
    result.project.timeline.tracks[0].audio,
    source.timeline.tracks[0].audio,
  );
  assert.deepEqual(
    result.project.timeline.tracks[1].audio,
    source.timeline.tracks[1].audio,
  );
  assert.deepEqual(result.project.audio, source.audio);
});

/** A bus curve's value at `tick`, as the engine samples keyframes. */
function value(animation, tick) {
  const frames = animation.keyframes;
  let next = frames.findIndex((key) => key.time > tick);
  if (next === -1) next = frames.length;
  if (next === 0) return Math.fround(frames[0].value);
  const left = frames[next - 1];
  const right = frames[next];
  if (!right || tick === left.time || left.interpolation.type === "hold")
    return Math.fround(left.value);
  const progress = (tick - left.time) / (right.time - left.time);
  assert.equal(left.interpolation.type, "linear", "only linear curves here");
  return Math.fround(left.value + (right.value - left.value) * progress);
}

test("an insert opens bus keys: the value at the edit point holds across the opening", () => {
  for (const curve of ["hold", "linear"]) {
    const source = project(curve);
    const length = at(20);
    const result = insert(source, "v1", clip("N", "s", 0, 20), at(100), {
      syncTracks: ["a1"],
    });
    assert.ok(result.ok, JSON.stringify(result.error));
    assert.deepEqual(result.timeline, {
      opened: { at: at(100), length },
      everyTrack: true,
    });
    const before = [
      source.timeline.tracks[0].audio.gain,
      source.timeline.tracks[1].audio.gain,
      source.timeline.tracks[1].audio.pan,
      source.audio.gain,
    ];
    const after = [
      result.project.timeline.tracks[0].audio.gain,
      result.project.timeline.tracks[1].audio.gain,
      result.project.timeline.tracks[1].audio.pan,
      result.project.audio.gain,
    ];
    for (let tick = 0; tick < at(220); tick += 997) {
      before.forEach((animation, index) => {
        const moved =
          tick < at(100)
            ? value(animation, tick)
            : tick < at(100) + length
              ? value(animation, at(100))
              : value(animation, tick - length);
        assert.ok(
          Math.abs(value(after[index], tick) - moved) < 1e-6,
          `${curve} curve ${index} at ${tick}`,
        );
      });
    }
  }
});
