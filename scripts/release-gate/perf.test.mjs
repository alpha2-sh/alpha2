import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";

import {
  aggregate,
  grainedProject,
  pairs,
  roundOrder,
  tiledProject,
  validity,
} from "./perf.mjs";

const axes = {
  project: "p1",
  renderer: "gpu",
  pipeline: "readback",
  encoder: "h264_nvenc",
  encclass: "hw",
  decode: "sw",
};

function run(steady, identity = {}) {
  return {
    ok: true,
    steadyMsPerFrame: steady,
    exportFps: 1000 / steady,
    cpuMsPerFrame: 2,
    startOverheadMs: 300,
    shares: { frameSubmitMs: 0.5 },
    identity: {
      renderer: "gpu",
      adapter: "GPU",
      pipeline: "readback",
      encoder: "h264_nvenc",
      encoderHardware: true,
      hardwareDecoded: 0,
      softwareDecoded: 360,
      frames: 360,
      ...identity,
    },
  };
}

test("rounds rotate and reverse", () => {
  assert.deepEqual(roundOrder(4, 0), [0, 1, 2, 3]);
  assert.deepEqual(roundOrder(4, 1), [0, 3, 2, 1]);
  assert.deepEqual(roundOrder(4, 2), [2, 3, 0, 1]);
  assert.deepEqual(roundOrder(4, 3), [2, 1, 0, 3]);
  // Over enough rounds no configuration always follows the same one.
  const followers = new Map();
  for (let round = 0; round < 4; round++) {
    const order = roundOrder(4, round);
    for (let i = 1; i < order.length; i++)
      followers.set(
        order[i],
        (followers.get(order[i]) ?? new Set()).add(order[i - 1]),
      );
  }
  for (const seen of followers.values()) assert.ok(seen.size > 1);
});

test("a run whose encoder changed between rounds is invalid", () => {
  assert.deepEqual(validity(axes, [run(10), run(10.2)]), []);
  const problems = validity(axes, [
    run(10),
    run(10.2, { encoder: "h264_vulkan" }),
  ]);
  assert.ok(
    problems.some((problem) =>
      problem.startsWith("encoder differs between rounds"),
    ),
  );
  assert.deepEqual(validity({ ...axes, pipeline: "resident" }, [run(10)]), [
    "videoPipeline is readback, configured gpu-resident",
  ]);
  assert.deepEqual(validity(axes, [{ ok: false, error: "x" }]), [
    "no successful run",
  ]);
});

test("a spread over 1.15 asks for more rounds", () => {
  const configuration = { name: "gpu-readback-nvenc", axes };
  const steady = aggregate(configuration, [run(10), run(10.5), run(11)]);
  assert.equal(steady.addRounds, false);
  assert.equal(steady.steadyMsPerFrame.median, 10.5);
  const spread = aggregate(configuration, [run(10), run(10.5), run(11.6)]);
  assert.equal(spread.addRounds, true);
  assert.ok(spread.spread > 1.15);
});

test("configurations that differ in one axis are paired", () => {
  const a = aggregate({ name: "gpu-readback-nvenc", axes }, [run(10)]);
  const b = aggregate(
    { name: "cpu-nvenc", axes: { ...axes, renderer: "cpu" } },
    [run(20, { renderer: "cpu" })],
  );
  const c = aggregate(
    { name: "default-config", axes: { project: "p1", default: "1" } },
    [run(5)],
  );
  assert.deepEqual(
    pairs([a, b, c]).map(({ axis, a: x, b: y, steadyRatio }) => [
      axis,
      x,
      y,
      steadyRatio,
    ]),
    [["renderer", "cpu-nvenc", "gpu-readback-nvenc", 0.5]],
  );
});

test("the tiled project repeats the parity timeline and grain goes on both looks", () => {
  const base = JSON.parse(
    readFileSync(
      join(
        import.meta.dirname,
        "..",
        "..",
        "tests",
        "cases",
        "host-parity-complex-project.json",
      ),
      "utf8",
    ),
  );
  const tiled = tiledProject(base, 8);
  const extent = (project) =>
    Math.max(
      ...project.timeline.tracks.flatMap((t) =>
        t.clips.map((c) => c.timeline_range.end),
      ),
    );
  assert.equal(extent(tiled), 8 * extent(base));
  assert.deepEqual(tiled.settings.frame_rate, { num: 30, den: 1 });
  const clips = (project) =>
    project.timeline.tracks.reduce((n, t) => n + t.clips.length, 0);
  assert.equal(clips(tiled), 8 * clips(base));
  const grained = grainedProject(tiled);
  assert.equal(
    grained.compositions.filter((c) => c.output === "perf-grain").length,
    2,
  );
});
