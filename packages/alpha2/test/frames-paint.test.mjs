import assert from "node:assert/strict";
import test from "node:test";

import { frames } from "../dist/frames.js";
import { linearGradient, radialGradient } from "../dist/paint.js";
import {
  EASING_PRESETS,
  secondsToTicks,
  validateProject,
} from "../dist/project.js";

test("frames snap to the engine's floored frame starts", () => {
  const ntsc = frames({ num: 30000, den: 1001 });
  assert.equal(ntsc.startTicks(1), 4004);
  assert.equal(ntsc.index(0.05), 1);
  assert.equal(secondsToTicks(ntsc.snap(0.05)), 4004);
  assert.equal(secondsToTicks(ntsc.last(1)), ntsc.startTicks(29));
  assert.ok(Math.abs(frames(30).last(6) - (6 - 1 / 30)) < 1e-9);
  assert.throws(() => frames(0), RangeError);
});

test("gradients are plain paints the engine accepts", () => {
  const paint = linearGradient(
    ["#000", [0.8, "#fff"]],
    { x: 0, y: 0 },
    { x: 1, y: 0 },
  );
  assert.deepEqual(paint, {
    type: "linear_gradient",
    start: { x: 0, y: 0 },
    end: { x: 1, y: 0 },
    stops: [
      { offset: 0, color: "#000" },
      { offset: 0.8, color: "#fff" },
    ],
  });
  const radial = radialGradient(["#f00", "#00f", "#0f0"], undefined, 0.7);
  assert.deepEqual(
    radial.stops.map((stop) => stop.offset),
    [0, 0.5, 1],
  );
  assert.throws(() => linearGradient(["#fff"]), RangeError);
  const project = {
    schema_version: 1,
    id: "p",
    name: "P",
    settings: {
      width: 16,
      height: 16,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      {
        id: "g",
        kind: "generated",
        generator: { type: "fill", paint: radial },
      },
    ],
    timeline: { tracks: [] },
  };
  assert.deepEqual(validateProject(project), []);
  assert.equal(EASING_PRESETS.easeOut.x2, 0.58);
});
