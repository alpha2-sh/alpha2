import assert from "node:assert/strict";
import test from "node:test";

import { BrowserStartError } from "../browser-errors.mjs";
import { runCases } from "./run-cases.mjs";

const timeout = () =>
  Promise.reject(new Error("browser page timed out at pending after 60 s"));

async function run(pages) {
  const reported = [];
  const ran = [];
  const stop = await runCases(
    Object.keys(pages),
    (name) => {
      ran.push(name);
      return pages[name]();
    },
    (name, result) => reported.push([name, result]),
  );
  return { reported, ran, stop };
}

test("a start error ends the run after the first case", async () => {
  const { ran, reported, stop } = await run({
    a: () => Promise.reject(new BrowserStartError("Chrome exited 3")),
    b: () => Promise.resolve({ ok: true }),
    c: () => Promise.resolve({ ok: true }),
  });
  assert.deepEqual(ran, ["a"]);
  assert.match(
    reported[0][1].error,
    /browser could not start: Chrome exited 3/,
  );
  assert.deepEqual(stop, [
    "FAIL 2 remaining case(s) not run: the browser could not start",
  ]);
});

test("two page timeouts before any report end the run", async () => {
  const { ran, stop } = await run({
    a: timeout,
    b: timeout,
    c: () => Promise.resolve({ ok: true }),
  });
  assert.deepEqual(ran, ["a", "b"]);
  assert.match(stop[0], /never ran a page/);
  assert.equal(stop[1], "FAIL 1 remaining case(s) not run");
});

test("a timeout after a reported page does not end the run", async () => {
  const { ran, stop } = await run({
    a: () => Promise.resolve({ ok: true }),
    b: timeout,
    c: timeout,
    d: () => Promise.resolve({ ok: true }),
  });
  assert.deepEqual(ran, ["a", "b", "c", "d"]);
  assert.deepEqual(stop, []);
});
