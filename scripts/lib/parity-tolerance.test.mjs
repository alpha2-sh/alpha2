// The cross-device bound host-parity holds the complex and effects projects to.
import assert from "node:assert/strict";
import test from "node:test";

import {
  OUTLIER_PEAK_CODES,
  OUTLIER_SAMPLES,
  PEAK_CODES,
  peakExcess,
} from "./parity-tolerance.mjs";

test("differences within the general bound pass", () => {
  assert.equal(peakExcess({ max: 0, outliers: 0 }), null);
  assert.equal(peakExcess({ max: PEAK_CODES, outliers: 0 }), null);
});

test("a sharp edge sampled a fraction of a texel apart on another device passes", () => {
  // Native lavapipe or Iris Xe against Chrome: alpha2.transition.directional at progress 1/3, two
  // vertically adjacent Y samples 7 codes apart in opposite directions.
  assert.equal(peakExcess({ max: 7, outliers: 2 }), null);
  assert.equal(
    peakExcess({ max: OUTLIER_PEAK_CODES, outliers: OUTLIER_SAMPLES }),
    null,
  );
});

test("more isolated samples than allowed fail", () => {
  assert.match(
    peakExcess({ max: 5, outliers: OUTLIER_SAMPLES + 1 }),
    /5 samples more than 4 codes apart \(at most 5\); allowed 4/,
  );
});

test("a larger local difference fails however few samples it touches", () => {
  assert.match(
    peakExcess({ max: OUTLIER_PEAK_CODES + 1, outliers: 1 }),
    /9 codes apart at most/,
  );
  // Plain `fwidth` anti-aliasing: up to 41 codes on a few rims between an RTX 4090 and RADV.
  assert.notEqual(peakExcess({ max: 41, outliers: 3 }), null);
});
