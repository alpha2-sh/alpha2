import assert from "node:assert/strict";
import test from "node:test";

import {
  ActiveStopwatch,
  activeTimeout,
  MAX_CREDIT_MS,
  withinActiveTime,
} from "../dist/active-clock.js";

function fakeClock() {
  const clock = {
    time: 0,
    ticks: new Set(),
    now: () => clock.time,
    every(_ms, tick) {
      clock.ticks.add(tick);
      return () => clock.ticks.delete(tick);
    },
    advance(ms) {
      clock.time += ms;
      for (const tick of [...clock.ticks]) tick();
    },
  };
  return clock;
}

test("a 20 second freeze counts as 1.1 seconds", () => {
  const clock = fakeClock();
  const watch = new ActiveStopwatch(clock);
  for (let i = 0; i < 4; i += 1) clock.advance(250);
  assert.equal(watch.elapsed(), 1_000);
  clock.advance(20_000);
  assert.equal(watch.elapsed(), 1_000 + MAX_CREDIT_MS);
  assert.equal(MAX_CREDIT_MS, 1_100);
  clock.advance(1_000);
  assert.equal(watch.elapsed(), 2_000 + MAX_CREDIT_MS);
  watch.reset();
  assert.equal(watch.elapsed(), 0);
  watch.dispose();
  assert.equal(clock.ticks.size, 0, "a disposed stopwatch schedules nothing");
});

test("an active timeout ignores a freeze and stops when aborted", async () => {
  const clock = fakeClock();
  let fired = false;
  const timeout = activeTimeout(5_000, undefined, clock).then(() => {
    fired = true;
  });
  clock.advance(60_000);
  await Promise.resolve();
  assert.equal(fired, false, "a minute frozen is one tick");
  for (let i = 0; i < 16; i += 1) clock.advance(250);
  await timeout;
  assert.equal(fired, true);
  assert.equal(clock.ticks.size, 0);
  const stop = new AbortController();
  void activeTimeout(1_000, stop.signal, clock);
  assert.equal(clock.ticks.size, 1);
  stop.abort();
  assert.equal(clock.ticks.size, 0, "an aborted timeout stops counting");
});

test("work within active time resolves, and the count stops with it", async () => {
  const clock = fakeClock();
  assert.equal(
    await withinActiveTime(
      Promise.resolve(7),
      1_000,
      () => new Error("late"),
      clock,
    ),
    7,
  );
  assert.equal(clock.ticks.size, 0);
  const never = withinActiveTime(
    new Promise(() => {}),
    500,
    () => new Error("stalled"),
    clock,
  );
  clock.advance(250);
  clock.advance(250);
  await assert.rejects(never, /stalled/);
});
