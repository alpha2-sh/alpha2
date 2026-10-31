// The benchmark comparison flags only times that grew by more than 5 %.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { compare, measurements, report } from "./bench-compare.mjs";

const script = resolve(import.meta.dirname, "bench-compare.mjs");

const baseline = [
  "running 3 tests",
  "test tests::a_case ... ignored",
  "timeline_evaluation tracks=32 iterations=20000 ns_per_evaluation=1000",
  "dsp processor=compressor fresh_silent_ns_per_block=200 decayed_silent_ns_per_block=100 ratio=0.50",
  "dsp processor=compressor loud_ns_per_block=400",
  "audio_mix clips=16 block=128 us_per_block=10.00 realtime_pct=0.2",
  "[ffv1 @ 0x5d] high bits_per_raw_sample, forcing range coder",
  "graph_encode case=audit size=1920x1080 median_us=40.0 mean_us=50.0",
].join("\n");

test("measurements keep the fastest time of each case and skip what is not a time", () => {
  const found = measurements(
    `${baseline}\ntimeline_evaluation tracks=32 iterations=20000 ns_per_evaluation=990\n`,
  );
  const values = Object.fromEntries(
    [...found].map(([id, { value, step }]) => [id, [value, step]]),
  );
  assert.deepEqual(values, {
    "timeline_evaluation tracks=32 iterations=20000 ns_per_evaluation": [
      990, 1,
    ],
    "dsp processor=compressor fresh_silent_ns_per_block": [200, 1],
    "dsp processor=compressor decayed_silent_ns_per_block": [100, 1],
    "dsp processor=compressor loud_ns_per_block": [400, 1],
    "audio_mix clips=16 block=128 us_per_block": [10, 0.01],
    "graph_encode case=audit size=1920x1080 median_us": [40, 0.1],
    "graph_encode case=audit size=1920x1080 mean_us": [50, 0.1],
  });
});

test("a time more than 5 % slower is a regression, 5 % or less is not", () => {
  const current = measurements(
    baseline
      .replace("ns_per_evaluation=1000", "ns_per_evaluation=1051")
      .replace("loud_ns_per_block=400", "loud_ns_per_block=420")
      .replace("median_us=40.0", "median_us=30.0"),
  );
  const result = compare(measurements(baseline), current);
  assert.deepEqual(
    result.regressions.map(({ id, change }) => [
      id,
      Math.round(change * 10) / 10,
    ]),
    [["timeline_evaluation tracks=32 iterations=20000 ns_per_evaluation", 5.1]],
  );
  assert.equal(result.improvements, 1);
  assert.equal(result.unchanged, 5);
  assert.match(
    report(result)[0],
    /^REGRESSION: 1 of 7 benchmark times grew by more than 5 %$/,
  );
});

test("a change of one unit in the last printed digit is rounding, not a regression", () => {
  const before = measurements(
    "plane_upload size=64x64 prepare_ms=0.17 frame_ms=0.17",
  );
  const after = measurements(
    "plane_upload size=64x64 prepare_ms=0.18 frame_ms=0.19",
  );
  const result = compare(before, after);
  assert.deepEqual(
    result.regressions.map(({ id }) => id),
    ["plane_upload size=64x64 frame_ms"],
  );
  assert.equal(result.unchanged, 1);
});

test("a case only one side measured is listed, not compared", () => {
  const current = measurements(
    baseline.replace("clips=16", "clips=32") +
      "\nreadback size=64x64 finish_ms=1.0",
  );
  const result = compare(measurements(baseline), current);
  assert.deepEqual(result.regressions, []);
  assert.deepEqual(result.missing, [
    "audio_mix clips=16 block=128 us_per_block",
  ]);
  assert.deepEqual(result.added, [
    "audio_mix clips=32 block=128 us_per_block",
    "readback size=64x64 finish_ms",
  ]);
  assert.match(report(result)[0], /^ok: none of 6 benchmark times/);
});

test("a time printed as n/a is not measured, not unchanged", () => {
  const measured = measurements(
    "cpu_frame size=1920x1080 threads=1 wall_ms=120.0 cpu_ms=118.0",
  );
  const unmeasured = measurements(
    "cpu_frame size=1920x1080 threads=1 wall_ms=121.0 cpu_ms=n/a",
  );
  assert.deepEqual(
    unmeasured.get("cpu_frame size=1920x1080 threads=1 cpu_ms"),
    {
      value: null,
      step: 0,
    },
  );
  for (const [before, after] of [
    [measured, unmeasured],
    [unmeasured, measured],
  ]) {
    const result = compare(before, after);
    assert.deepEqual(result.notMeasured, [
      "cpu_frame size=1920x1080 threads=1 cpu_ms",
    ]);
    assert.equal(result.unchanged, 1, "wall_ms alone");
    assert.deepEqual(result.regressions, []);
    assert.ok(
      report(result).includes(
        "  not measured (n/a on this machine): cpu_frame size=1920x1080 threads=1 cpu_ms",
      ),
    );
  }
});

test("the command exits 1 on a regression, 0 without one, and 2 when nothing matches", () => {
  const dir = mkdtempSync(join(tmpdir(), "bench-compare-"));
  try {
    const write = (name, text) => {
      writeFileSync(join(dir, name), text);
      return join(dir, name);
    };
    const before = write("before.txt", baseline);
    const run = (after, ...args) =>
      spawnSync(process.execPath, [script, before, after, ...args], {
        encoding: "utf8",
      });
    const slower = write(
      "slower.txt",
      baseline.replace("mean_us=50.0", "mean_us=53.0"),
    );
    assert.equal(run(slower).status, 1);
    assert.match(run(slower).stdout, /^REGRESSION: 1 of 7/);
    assert.equal(run(slower, "--threshold", "10").status, 0);
    assert.equal(run(write("same.txt", baseline)).status, 0);
    assert.equal(run(write("other.txt", "nothing here\n")).status, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
