import assert from "node:assert/strict";
import test from "node:test";

import {
  GpuRecoveryLadder,
  lossReportFromJson,
  pageGpuLosses,
} from "../dist/gpu-recovery.js";

/** A loss count of its own, so one test's losses do not move another's ladder. */
function losses() {
  let count = 0;
  return { count: () => count, add: () => void (count += 1) };
}

const external = (timeTicks = null) => ({
  hung: false,
  frameIndex: null,
  timeTicks,
  gpuMs: 12,
  message: "the GPU device was lost",
});
const webgpu = { kind: "webgpu", webgpuAdapter: true };
const rung = (step) => `${step.renderer}/${step.surface}`;

test("a hung loss goes to the cpu at once", () => {
  const ladder = new GpuRecoveryLadder({ losses: losses() });
  const step = ladder.onLoss(
    { ...external(4_000n), hung: true, gpuMs: 1900 },
    webgpu,
  );
  assert.equal(rung(step), "cpu/fresh");
  assert.match(step.reason, /1900 ms in one submission/);
  assert.equal(ladder.renderer, "cpu");
});

test("a first external loss retries the gpu on the same webgpu surface", () => {
  const ladder = new GpuRecoveryLadder({ losses: losses() });
  assert.equal(rung(ladder.onLoss(external(), webgpu)), "gpu/same");
  assert.equal(ladder.renderer, "gpu");
  // Without a WebGPU adapter now, or on a WebGL2 canvas, the GPU needs a fresh canvas.
  for (const surface of [
    { kind: "webgpu", webgpuAdapter: false },
    { kind: "webgl2", webgpuAdapter: true },
    { kind: "none", webgpuAdapter: true },
  ]) {
    const fresh = new GpuRecoveryLadder({ losses: losses() });
    assert.equal(rung(fresh.onLoss(external(), surface)), "gpu/fresh");
  }
});

test("a loss again at the same time goes to the cpu", () => {
  const ladder = new GpuRecoveryLadder({
    losses: losses(),
    maxGpuLosses: 5,
  });
  assert.equal(rung(ladder.onLoss(external(8_000n), webgpu)), "gpu/same");
  assert.equal(rung(ladder.onLoss(external(8_000n), webgpu)), "cpu/fresh");
});

test("a second page loss goes to the cpu even from another host", () => {
  const page = losses();
  const first = new GpuRecoveryLadder({ losses: page });
  const second = new GpuRecoveryLadder({ losses: page });
  assert.equal(rung(first.onLoss(external(1n), webgpu)), "gpu/same");
  const step = second.onLoss(external(2n), webgpu);
  assert.equal(rung(step), "cpu/fresh");
  assert.match(step.reason, /lost the GPU 2 times/);
  assert.equal(page.count(), 2);
});

test("the realm's loss count is shared by every ladder that does not bring its own", () => {
  const before = pageGpuLosses.count();
  new GpuRecoveryLadder().onLoss(external(), webgpu);
  assert.equal(pageGpuLosses.count(), before + 1);
  pageGpuLosses.add();
  assert.equal(pageGpuLosses.count(), before + 2);
});

test("a failed gpu recovery moves down, never repeats", () => {
  const ladder = new GpuRecoveryLadder({ losses: losses() });
  let step = ladder.onLoss(external(), webgpu);
  const tried = [rung(step)];
  for (;;) {
    const next = ladder.onRecoveryFailed(step, new Error("no"));
    if (next === "terminal") break;
    tried.push(rung(next));
    step = next;
  }
  assert.deepEqual(tried, ["gpu/same", "gpu/fresh", "cpu/fresh"]);
  // Starting on a fresh canvas skips the same-canvas rung on the way down.
  const fresh = new GpuRecoveryLadder({ losses: losses() });
  const first = fresh.onLoss(external(), {
    kind: "webgl2",
    webgpuAdapter: false,
  });
  const next = fresh.onRecoveryFailed(first, new Error("no WebGL2"));
  assert.equal(rung(next), "cpu/fresh");
  assert.match(next.reason, /no WebGL2/);
});

test("the cpu rung failing is terminal", () => {
  const ladder = new GpuRecoveryLadder({ losses: losses() });
  const step = ladder.onLoss({ ...external(), hung: true }, webgpu);
  assert.equal(ladder.onRecoveryFailed(step, new Error("no 2d")), "terminal");
});

test("maxGpuLosses must be a positive integer", () => {
  assert.throws(() => new GpuRecoveryLadder({ maxGpuLosses: 0 }), RangeError);
  assert.throws(() => new GpuRecoveryLadder({ maxGpuLosses: 1.5 }), RangeError);
});

test("a wasm loss report becomes a typed report", () => {
  const report = lossReportFromJson(
    JSON.stringify({
      hung: true,
      gpuMs: 1450.5,
      watchdogMs: 2000,
      timeTicks: "36000",
      frameIndex: 9,
      effects: ["user.heavy@1"],
    }),
  );
  assert.deepEqual(report, {
    hung: true,
    frameIndex: 9,
    timeTicks: 36_000n,
    gpuMs: 1450.5,
    message:
      "the GPU device hung: one submission ran 1450.5 ms against a 2000 ms watchdog",
  });
  assert.deepEqual(lossReportFromJson(undefined, "lost"), {
    hung: false,
    frameIndex: null,
    timeTicks: null,
    gpuMs: null,
    message: "lost",
  });
});
