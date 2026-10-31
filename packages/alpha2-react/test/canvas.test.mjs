import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToString } from "react-dom/server";

import { Alpha2Canvas, bindCanvas } from "../dist/index.js";

/** A player that records what it is asked, presenting when released. */
function fakePlayer() {
  const calls = [];
  let release;
  return {
    calls,
    initialize: async (canvas) => void calls.push(["initialize", canvas]),
    present(time) {
      calls.push(["present", time]);
      return new Promise((resolve) => {
        release = () =>
          resolve({
            requestedTimeTicks: time,
            presentedTimeTicks: time,
            missingSources: [],
            repeated: false,
          });
      });
    },
    dispose: () => void calls.push(["dispose"]),
    release: () => release?.(),
  };
}

class Visibility extends EventTarget {
  visibilityState = "visible";
  set(state) {
    this.visibilityState = state;
    this.dispatchEvent(new Event("visibilitychange"));
  }
}

const settle = () => new Promise((resolve) => setImmediate(resolve));
const presents = (player) =>
  player.calls.filter(([kind]) => kind === "present").map(([, time]) => time);

test("the binding presents the latest time, coalescing while busy", async () => {
  const player = fakePlayer();
  const offscreen = { offscreen: true };
  const presented = [];
  const binding = bindCanvas(
    player,
    { transferControlToOffscreen: () => offscreen },
    {
      visibility: new Visibility(),
      onPresented: (frame) => presented.push(frame),
    },
  );
  binding.show(10n);
  await binding.ready;
  assert.deepEqual(player.calls[0], ["initialize", offscreen]);
  await settle();
  assert.deepEqual(presents(player), [10n]);
  // Two times while the first presentation runs: only the latest follows.
  binding.show(20n);
  binding.show(30n);
  player.release();
  await settle();
  assert.deepEqual(presents(player), [10n, 30n]);
  player.release();
  await settle();
  assert.deepEqual(
    presented.map((frame) => frame.requestedTimeTicks),
    [10n, 30n],
  );
  // The same time again is presented again.
  binding.show(30n);
  await settle();
  assert.deepEqual(presents(player), [10n, 30n, 30n]);
  player.release();
  await settle();
  binding.dispose();
  binding.dispose();
  binding.show(40n);
  await settle();
  assert.deepEqual(presents(player), [10n, 30n, 30n], "disposed: nothing more");
  // The player is its owner's: the binding only stops using it.
  assert.deepEqual(
    player.calls.filter(([kind]) => kind === "dispose"),
    [],
  );
});

test("a hidden page presents nothing until it shows again", async () => {
  const player = fakePlayer();
  const visibility = new Visibility();
  visibility.visibilityState = "hidden";
  const binding = bindCanvas(
    player,
    { transferControlToOffscreen: () => ({}) },
    { visibility },
  );
  binding.show(5n);
  binding.show(6n);
  await binding.ready;
  await settle();
  assert.deepEqual(presents(player), []);
  visibility.set("visible");
  await settle();
  assert.deepEqual(presents(player), [6n]);
  player.release();
  binding.dispose();
});

test("failures reach onError", async () => {
  const errors = [];
  const binding = bindCanvas(
    {
      initialize: async () => {},
      present: async () => {
        throw new Error("lost");
      },
      dispose() {},
    },
    { transferControlToOffscreen: () => ({}) },
    {
      visibility: new Visibility(),
      onError: (error) => errors.push(error.message),
    },
  );
  binding.show(1n);
  await binding.ready;
  await settle();
  assert.deepEqual(errors, ["lost"]);
  binding.dispose();
});

/** A player whose presentations fail or succeed as the test settles them, one at a time. */
function scriptedPlayer() {
  const presented = [];
  const waiting = [];
  return {
    presented,
    initialize: async () => {},
    present(time) {
      presented.push(time);
      return new Promise((resolve, reject) =>
        waiting.push({
          succeed: () =>
            resolve({
              requestedTimeTicks: time,
              presentedTimeTicks: time,
              missingSources: [],
              repeated: false,
            }),
          fail: (message) => reject(new Error(message)),
        }),
      );
    },
    next: () => waiting.shift(),
  };
}

test("a failed presentation still presents the time asked for meanwhile", async () => {
  const player = scriptedPlayer();
  const errors = [];
  const binding = bindCanvas(
    player,
    { transferControlToOffscreen: () => ({}) },
    {
      visibility: new Visibility(),
      onError: (error) => errors.push(error.message),
    },
  );
  binding.show(1n);
  await binding.ready;
  await settle();
  binding.show(2n);
  player.next().fail("no decoder for cam2");
  await settle();
  assert.deepEqual(errors, ["no decoder for cam2"]);
  assert.deepEqual(player.presented, [1n, 2n]);
  player.next().succeed();
  await settle();
  binding.dispose();
});

test("a time shown from onError is presented", async () => {
  const player = scriptedPlayer();
  let binding;
  binding = bindCanvas(
    player,
    { transferControlToOffscreen: () => ({}) },
    {
      visibility: new Visibility(),
      onError: () => binding.show(7n),
    },
  );
  binding.show(7n);
  await binding.ready;
  await settle();
  player.next().fail("canvas surface is temporarily unavailable");
  await settle();
  assert.deepEqual(player.presented, [7n, 7n]);
  player.next().succeed();
  await settle();
  binding.dispose();
});

test("work in flight when the binding is disposed fails silently", async () => {
  let failInitialize;
  const player = scriptedPlayer();
  player.initialize = () =>
    new Promise((_, reject) => (failInitialize = reject));
  const errors = [];
  const binding = bindCanvas(
    player,
    { transferControlToOffscreen: () => ({}) },
    {
      visibility: new Visibility(),
      onError: (error) => errors.push(error.message),
    },
  );
  binding.show(1n);
  binding.dispose();
  failInitialize(new Error("alpha2 render worker disposed"));
  await assert.rejects(binding.ready, /disposed/);

  const presenting = scriptedPlayer();
  const second = bindCanvas(
    presenting,
    { transferControlToOffscreen: () => ({}) },
    {
      visibility: new Visibility(),
      onError: (error) => errors.push(error.message),
    },
  );
  second.show(1n);
  await second.ready;
  await settle();
  second.dispose();
  presenting.next().fail("alpha2 render worker disposed");
  await settle();
  assert.deepEqual(errors, []);
});

test("a canvas that cannot be taken over fails ready, reports it, and leaves no listener", async () => {
  const visibility = new Visibility();
  const listeners = [];
  visibility.addEventListener = (type, listener) =>
    listeners.push([type, listener]);
  const errors = [];
  const binding = bindCanvas(
    scriptedPlayer(),
    {
      transferControlToOffscreen() {
        throw new Error("canvas already transferred");
      },
    },
    { visibility, onError: (error) => errors.push(error.message) },
  );
  await assert.rejects(binding.ready, /already transferred/);
  await settle();
  assert.deepEqual(errors, ["canvas already transferred"]);
  assert.deepEqual(listeners, []);
  binding.dispose();
});

test("without onError a failure goes to the page's reportError", async () => {
  const reported = [];
  globalThis.reportError = (error) => reported.push(error.message);
  try {
    const player = scriptedPlayer();
    const binding = bindCanvas(
      player,
      { transferControlToOffscreen: () => ({}) },
      { visibility: new Visibility() },
    );
    binding.show(1n);
    await binding.ready;
    await settle();
    player.next().fail("wasm unreachable");
    await settle();
    assert.deepEqual(reported, ["wasm unreachable"]);
    binding.dispose();
  } finally {
    delete globalThis.reportError;
  }
});

test("the component keeps the project's aspect ratio", () => {
  const html = renderToString(
    createElement(Alpha2Canvas, {
      createPlayer: fakePlayer,
      timeTicks: 0n,
      width: 1920,
      height: 1080,
      className: "preview",
    }),
  );
  assert.match(html, /class="preview"/);
  assert.match(html, /aspect-ratio:1920 \/ 1080/);
});

test("replaceSurface transfers the canvas replaceCanvas puts in place", async () => {
  const offscreen = { offscreen: "second" };
  const binding = bindCanvas(
    fakePlayer(),
    { transferControlToOffscreen: () => ({ offscreen: "first" }) },
    {
      visibility: new Visibility(),
      replaceCanvas: async () => ({
        transferControlToOffscreen: () => offscreen,
      }),
    },
  );
  await binding.ready;
  assert.equal(await binding.replaceSurface(), offscreen);
  binding.dispose();
  await assert.rejects(binding.replaceSurface(), /disposed/);
  const without = bindCanvas(
    fakePlayer(),
    { transferControlToOffscreen: () => ({}) },
    { visibility: new Visibility() },
  );
  await assert.rejects(without.replaceSurface(), /replaceCanvas/);
  without.dispose();
});
