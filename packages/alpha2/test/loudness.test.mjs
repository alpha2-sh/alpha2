import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import {
  measureLoudness,
  planLoudnessNormalization,
} from "../dist/loudness.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");

// The packaged binding, or null before `npm run build` made it. A missing package is a
// recorded skip, which ALPHA2_REQUIRE=wasm turns into a failure.
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

/**
 * A binding shaped like `alpha2/wasm`'s: the meter takes an empty block as the wasm one does, and
 * gives up after a thousand of them so a loop that never advances fails instead of hanging.
 */
function fakeBinding() {
  const pushed = [];
  return {
    pushed,
    LoudnessMeter: class {
      #channels;
      #frames = 0;
      #empty = 0;
      constructor(_rate, channels) {
        this.#channels = channels;
      }
      push(planar) {
        if (planar.length % this.#channels !== 0)
          throw new Error(
            "a planar block holds the same number of samples per channel",
          );
        if (planar.length === 0 && ++this.#empty > 1000)
          throw new Error("the measurement never advanced");
        pushed.push(planar.length / this.#channels);
        this.#frames += planar.length / this.#channels;
      }
      report() {
        return JSON.stringify({ frames: this.#frames, integratedLufs: null });
      }
    },
    planLoudnessNormalization: () => "null",
  };
}

test("a block size that is not a positive integer is refused before anything is measured", () => {
  const planes = [new Float32Array(10), new Float32Array(10)];
  for (const blockFrames of [
    0,
    Number.NaN,
    2.5,
    -5,
    Number.POSITIVE_INFINITY,
  ]) {
    const binding = fakeBinding();
    assert.throws(
      () => measureLoudness(binding, planes, 48_000, blockFrames),
      (error) =>
        error instanceof RangeError &&
        /loudness blockFrames must be a positive integer/.test(error.message),
      `blockFrames ${blockFrames}`,
    );
    assert.deepEqual(binding.pushed, []);
  }
  const binding = fakeBinding();
  assert.equal(measureLoudness(binding, planes, 48_000, 3).frames, 10);
  assert.deepEqual(binding.pushed, [3, 3, 3, 1]);
});

test("a report that is not an object is refused instead of planned as silence", () => {
  const planned = [];
  const recording = {
    ...fakeBinding(),
    planLoudnessNormalization: (json) => {
      planned.push(json);
      return "null";
    },
  };
  const parsed = { frames: 48_000, integratedLufs: -20 };
  for (const report of [JSON.stringify(parsed), null, "null"]) {
    assert.throws(
      () => planLoudnessNormalization(recording, report, -14),
      (error) =>
        error instanceof TypeError && /JSON\.parse/.test(error.message),
      String(report),
    );
  }
  assert.deepEqual(planned, []);
  planLoudnessNormalization(recording, parsed, -14);
  assert.deepEqual(planned, [JSON.stringify(parsed)]);
});

test(
  "the binding refuses a report that is not an object with its frames",
  { skip: needsWasm },
  () => {
    const tone = Float32Array.from(
      { length: 48_000 * 2 },
      (_, index) => 0.1 * Math.sin((2 * Math.PI * 1_000 * index) / 48_000),
    );
    const reportJson = JSON.stringify(
      measureLoudness(binding, [tone, tone], 48_000),
    );
    assert.notEqual(
      JSON.parse(binding.planLoudnessNormalization(reportJson, -14, -1)),
      null,
    );
    for (const json of [
      JSON.stringify(reportJson),
      "null",
      "[]",
      JSON.stringify({ integratedLufs: -20 }),
    ]) {
      assert.throws(
        () => binding.planLoudnessNormalization(json, -14, -1),
        /loudness report/,
        json,
      );
    }
  },
);

test(
  "a plan keeps the mix under the master limiter its report states",
  { skip: needsWasm },
  () => {
    // A -30 dBFS sine asked for -16 LUFS would peak at -16 dBFS, over a 0.1 (-20 dBFS) limiter.
    const tone = Float32Array.from(
      { length: 48_000 * 4 },
      (_, index) =>
        10 ** (-30 / 20) * Math.sin((2 * Math.PI * 1_000 * index) / 48_000),
    );
    const report = measureLoudness(binding, [tone, tone], 48_000);
    const free = planLoudnessNormalization(binding, report, -16);
    assert.equal(free.peakLimited, false);
    assert.equal(free.limiterLimited, false);
    assert.deepEqual(
      planLoudnessNormalization(
        binding,
        { ...report, limiterThreshold: null },
        -16,
      ),
      free,
    );
    const limited = planLoudnessNormalization(
      binding,
      { ...report, limiterThreshold: 0.1 },
      -16,
    );
    assert.equal(limited.peakLimited, true);
    assert.equal(limited.limiterLimited, true);
    assert.ok(report.samplePeakDbfs + limited.gainDb < -20, limited.gainDb);
    assert.ok(
      Math.abs(limited.integratedLufs + 20) < 0.1,
      limited.integratedLufs,
    );
    for (const limiterThreshold of [0, -1, "0.1"]) {
      assert.throws(
        () =>
          planLoudnessNormalization(
            binding,
            { ...report, limiterThreshold },
            -16,
          ),
        /limiterThreshold/,
        String(limiterThreshold),
      );
    }
  },
);
