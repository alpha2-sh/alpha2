// A fingerprint records a frame rate only when it is one a project holds.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { validateProject } from "alpha2/project";

import {
  probeEvidence,
  rationalRate,
} from "../dist/browser/runtime/fingerprint.js";

test("a measured rate is the rational a stream declares", () => {
  assert.deepEqual(rationalRate(29.97003), { num: 30000, den: 1001 });
  assert.deepEqual(rationalRate(25.001), { num: 25, den: 1 });
  assert.deepEqual(rationalRate(12.5), { num: 12500, den: 1000 });
  assert.deepEqual(rationalRate(120000), { num: 120000, den: 1 });
});

test("a measure no project holds records no rate", () => {
  // 120 packets within a millisecond, and a rate that rounds to zero.
  for (const rate of [1_200_000, 120_000.4, 120_001, 0.0004, 0, -30, NaN]) {
    assert.equal(rationalRate(rate), null, String(rate));
  }
  // Each rate it does give is one the engine reads.
  for (const rate of [0.006, 1, 23.976, 59.94, 1000.5, 119_999.6]) {
    const frame_rate = rationalRate(rate);
    assert.ok(frame_rate, String(rate));
    const document = {
      schema_version: 1,
      id: "rate",
      name: "Rate",
      settings: { width: 64, height: 64, frame_rate, sample_rate: 48000 },
      sources: [],
      timeline: { tracks: [] },
    };
    assert.deepEqual(validateProject(document), [], String(rate));
  }
});

const evidence = JSON.parse(
  readFileSync(
    new URL("../../../tests/cases/media-evidence-corpus.json", import.meta.url),
    "utf8",
  ),
);

test("a measured rate rounds as the engine rounds it", () => {
  for (const [measured, expected] of evidence.rational)
    assert.deepEqual(rationalRate(measured), expected, String(measured));
});

test("the evidence recorded of the same bytes is what every host records", async () => {
  // The native host is held to the same file (bindings/alpha2-host/src/tests.rs): a variable-rate
  // video's rate is the one its frames are presented at, and a transport stream that starts at 1.47
  // s ends where its last frame does.
  for (const { file, size, frame_rate, duration_ticks } of evidence.files) {
    const bytes = readFileSync(
      new URL(`../../../tests/corpus/${file}`, import.meta.url),
    );
    const found = await probeEvidence(new File([bytes], file));
    const rate = found.streams?.video?.frame_rate ?? null;
    assert.equal(
      rate && frame_rate ? rate.num * frame_rate.den : rate,
      rate && frame_rate ? frame_rate.num * rate.den : frame_rate,
      `${file}: frame rate ${JSON.stringify(rate)}`,
    );
    assert.equal(found.duration_ticks, duration_ticks, `${file}: duration`);
    // The size it is shown at: an MP4 whose stretch only its parameter sets signal (no `pasp`) is
    // 171x96, as natively and as the browser's decoder shows it.
    const shown = found.streams?.video;
    assert.deepEqual(
      shown ? { width: shown.width, height: shown.height } : undefined,
      size,
      `${file}: size`,
    );
  }
});
