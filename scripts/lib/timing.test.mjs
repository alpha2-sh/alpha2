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

test("a step that is slow until its code warms up is read over many ticks of a coarse CPU clock", async () => {
  // A 4 ms clock (Windows counts 15.6 ms), a step of 1 ms at the small size, and two cold runs of
  // 25 ms each, as a JIT gives. Calibrated on the cold runs, the small reading is one run, under a
  // tick, and reads 0.
  const tick = 4_000;
  const cpuUsage = process.cpuUsage;
  process.cpuUsage = () => {
    const { user, system } = cpuUsage.call(process);
    return {
      user: Math.floor(user / tick) * tick,
      system: Math.floor(system / tick) * tick,
    };
  };
  let runs = 0;
  const spin = (ms) => {
    const until = performance.now() + ms;
    while (performance.now() < until);
  };
  try {
    await assertScalesBelow(
      "a step that warms up",
      1,
      (size) => size,
      (size) => {
        runs += 1;
        spin(runs <= 2 ? 25 : size);
      },
    );
  } finally {
    process.cpuUsage = cpuUsage;
  }
});

test("the least of several readings is returned", () => {
  let runs = 0;
  const least = minOf(5, () => {
    runs += 1;
  });
  assert.equal(runs, 5);
  assert.ok(least >= 0 && least < 1_000, String(least));
});
