import test from "node:test";
import assert from "node:assert/strict";
import { assertScalesBelow, minOf } from "./timing.mjs";

const values = (size) => Float64Array.from({ length: size }, (_, i) => i);

function sumPairs(input, quadratic) {
  let total = 0;
  if (quadratic) {
    for (const a of input) for (const b of input) total += a * b;
  } else {
    for (const a of input) total += a;
  }
  return total;
}

test("linear work passes the scaling check", async () => {
  const ratio = await assertScalesBelow("a sum", 200_000, values, (input) =>
    sumPairs(input, false),
  );
  assert.ok(ratio > 0);
});

test("quadratic work fails the scaling check, naming what it measured", async () => {
  await assert.rejects(
    assertScalesBelow("a sum over every pair", 300, values, (input) =>
      sumPairs(input, true),
    ),
    /^Error: a sum over every pair: 8x the input \(300 -> 2400/,
  );
});

test("an async step is measured once its promise settles", async () => {
  const ratio = await assertScalesBelow(
    "an awaited sum",
    200_000,
    async (size) => values(size),
    async (input) => {
      await null;
      return sumPairs(input, false);
    },
  );
  // Only the sum grows with the input: a ratio near 1 would mean the reading ended at the await.
  assert.ok(ratio > 4, `${ratio}`);
});

test("the least of several readings is returned", () => {
  let runs = 0;
  const least = minOf(5, () => {
    runs += 1;
  });
  assert.equal(runs, 5);
  assert.ok(least >= 0 && least < 1_000, String(least));
});
