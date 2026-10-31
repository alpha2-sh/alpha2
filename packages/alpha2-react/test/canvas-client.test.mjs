// `<Alpha2Canvas>` in a DOM driving the real `Alpha2Player` and `WorkerRenderBackend`; only the
// worker is a stand-in, recording what it was sent.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { Window } from "happy-dom";

const window = new Window();
for (const name of ["window", "document", "navigator", "HTMLElement", "Node"])
  globalThis[name] ??= name === "window" ? window : window[name];
globalThis.IS_REACT_ACT_ENVIRONMENT = true;

/**
 * What an `OffscreenCanvas` keeps of the canvas it came from: the drawing-buffer size the element
 * had when it was transferred.
 */
window.HTMLCanvasElement.prototype.transferControlToOffscreen = function () {
  return { width: this.width, height: this.height };
};

const { act, Activity, createElement, StrictMode } = await import("react");
const { createRoot } = await import("react-dom/client");
const { Alpha2Player } = await import("alpha2/player");
const { WorkerRenderBackend } = await import("alpha2/worker");
const { Alpha2Canvas } = await import("../dist/index.js");

/**
 * The render worker, answering each request on a later turn as a real one does. Like the engine, it
 * finds a source frame only under the exact time it was uploaded at, and a presentation missing one
 * repeats the last complete frame (HoldPrevious).
 */
class FakeWorker extends EventTarget {
  requests = [];
  terminated = false;
  /** When set, `initialize` fails with this message. */
  failInitialize;
  /** Sources every presentation draws from, at the timeline's own time. */
  sources = [];
  uploaded = new Set();
  shown;
  /** When set, presentations are held until this resolves. */
  hold;
  /** When set, the next presentation finds the device lost (a WebGL2 one: no same-canvas rung). */
  loseOnPresent = false;
  postMessage(message) {
    this.requests.push(message);
    if (message.id === undefined || this.terminated) return;
    if (message.type === "upload-video-frame")
      this.uploaded.add(`${message.sourceId}@${message.requestedTimeTicks}`);
    const answer = () => {
      if (this.terminated) return;
      if (message.type === "present" && this.loseOnPresent) {
        this.loseOnPresent = false;
        this.dispatchEvent(
          new MessageEvent("message", { data: { type: "device-lost" } }),
        );
      }
      const reply =
        message.type === "initialize" && this.failInitialize !== undefined
          ? {
              id: message.id,
              ok: false,
              error: { message: this.failInitialize },
            }
          : { id: message.id, ok: true, result: this.#result(message) };
      this.dispatchEvent(new MessageEvent("message", { data: reply }));
    };
    const held = message.type === "present" ? this.hold : undefined;
    if (held === undefined) setImmediate(answer);
    else void held.then(() => setImmediate(answer));
  }
  terminate() {
    this.terminated = true;
  }
  #result(message) {
    if (message.type === "probe-surface")
      return { kind: "webgl2", webgpuAdapter: false };
    if (message.type === "recover")
      return {
        renderer: message.step.renderer,
        rendererReason: null,
        backend: "webgl2",
        adapter: null,
      };
    if (message.type !== "present") return undefined;
    const time = message.timeTicks;
    const missingFrames = this.sources
      .filter((sourceId) => !this.uploaded.has(`${sourceId}@${time}`))
      .map((sourceId) => ({
        clipId: `${sourceId}-clip`,
        sourceId,
        mediaTimeTicks: time,
      }));
    if (missingFrames.length === 0) this.shown = time;
    return {
      requestedTimeTicks: time,
      presentedTimeTicks: this.shown,
      missingSources: missingFrames.map((frame) => frame.sourceId),
      missingFrames,
      repeated: missingFrames.length > 0 && this.shown !== undefined,
    };
  }
  of(type) {
    return this.requests.filter((request) => request.type === type);
  }
  get presented() {
    return this.of("present").map((request) => request.timeTicks);
  }
}

/**
 * A `createPlayer` for the component: each call makes a real `Alpha2Player` over its own worker,
 * whose refill decodes what is asked and uploads it before resolving. `setup` configures each
 * worker as it is made.
 */
function players(setup = () => {}) {
  const workers = [];
  const createPlayer = (replaceSurface) => {
    const worker = new FakeWorker();
    setup(worker);
    workers.push(worker);
    const player = new Alpha2Player(new WorkerRenderBackend(worker), {
      async onRefill(requests) {
        for (const request of requests)
          await player.uploadVideoFrame({
            sourceId: request.sourceId,
            requestedTimeTicks: request.mediaTimeTicks,
            decodedTimeTicks: request.mediaTimeTicks,
            frame: { close() {} },
            generation: request.generation,
          });
      },
      replaceSurface,
    });
    return player;
  };
  return {
    createPlayer,
    workers,
    get worker() {
      return workers.at(-1);
    },
  };
}

/** Lets the worker's replies and the effects they trigger run. */
async function settle() {
  for (let turn = 0; turn < 8; turn += 1)
    await act(() => new Promise((resolve) => setImmediate(resolve)));
}

let container;
let root;
beforeEach(() => {
  container = window.document.createElement("div");
  window.document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(() => root.unmount());
  container.remove();
});

const props = (extra) => ({
  timeTicks: 0n,
  width: 1920,
  height: 1080,
  ...extra,
});

test("mounting gives a player a canvas, presents the time, and reports ready", async () => {
  const made = players();
  const events = [];
  await act(() =>
    root.render(
      createElement(
        Alpha2Canvas,
        props({
          createPlayer: made.createPlayer,
          timeTicks: 7n,
          className: "preview",
          onReady: () => events.push("ready"),
          onPresented: (frame) => events.push(frame.presentedTimeTicks),
          onError: (error) => events.push(error),
        }),
      ),
    ),
  );
  await settle();
  const host = container.firstElementChild;
  assert.equal(host.className, "preview");
  assert.equal(host.style.aspectRatio, "1920 / 1080");
  const canvas = host.querySelector("canvas");
  assert.ok(canvas, "the component adds its canvas");
  assert.equal(canvas.style.width, "100%");
  assert.equal(made.workers.length, 1);
  assert.equal(made.worker.of("initialize").length, 1);
  assert.deepEqual(made.worker.presented, [7n]);
  assert.deepEqual(events, ["ready", 7n]);
});

test("a recovery onto a fresh surface swaps the canvas element in place", async () => {
  const made = players();
  const element = (timeTicks) =>
    createElement(
      Alpha2Canvas,
      props({ createPlayer: made.createPlayer, timeTicks }),
    );
  await act(() => root.render(element(0n)));
  await settle();
  const first = container.querySelector("canvas");
  made.worker.loseOnPresent = true;
  await act(() => root.render(element(1n)));
  await settle();
  await act(() => root.render(element(2n)));
  await settle();
  const canvases = container.querySelectorAll("canvas");
  assert.equal(canvases.length, 1, "the new canvas replaced the old one");
  assert.notEqual(canvases[0], first);
  assert.equal(canvases[0].style.width, "100%");
  const [recover] = made.worker.of("recover");
  assert.deepEqual(recover.step.surface, "fresh");
  assert.deepEqual(recover.canvas, { width: 1920, height: 1080 });
  // The same player recovered: one worker, one initialize, the latest time presented.
  assert.equal(made.workers.length, 1);
  assert.equal(made.worker.presented.at(-1), 2n);
});

test("a new time presents it", async () => {
  const made = players();
  const element = (timeTicks) =>
    createElement(
      Alpha2Canvas,
      props({ createPlayer: made.createPlayer, timeTicks }),
    );
  await act(() => root.render(element(0n)));
  await settle();
  await act(() => root.render(element(42n)));
  await settle();
  assert.deepEqual(made.worker.presented, [0n, 42n]);
});

test("unmounting disposes the player it made and removes the canvas", async () => {
  const made = players();
  await act(() =>
    root.render(
      createElement(Alpha2Canvas, props({ createPlayer: made.createPlayer })),
    ),
  );
  await settle();
  await act(() => root.render(null));
  assert.equal(made.worker.of("dispose").length, 1);
  assert.equal(made.worker.terminated, true);
  assert.equal(container.querySelector("canvas"), null);
});

test("a new createPlayer gets a canvas of its own, and the old player is disposed", async () => {
  const first = players();
  const second = players();
  await act(() =>
    root.render(
      createElement(Alpha2Canvas, props({ createPlayer: first.createPlayer })),
    ),
  );
  await settle();
  const firstCanvas = container.querySelector("canvas");
  await act(() =>
    root.render(
      createElement(
        Alpha2Canvas,
        props({ createPlayer: second.createPlayer, timeTicks: 3n }),
      ),
    ),
  );
  await settle();
  assert.equal(first.worker.terminated, true);
  assert.equal(second.worker.of("initialize").length, 1);
  assert.deepEqual(second.worker.presented, [3n]);
  const canvases = container.querySelectorAll("canvas");
  assert.equal(canvases.length, 1);
  assert.notEqual(canvases[0], firstCanvas);
});

test("a player that fails to initialize reports it once and presents nothing", async () => {
  const made = players((worker) => {
    worker.failInitialize = "no GPU adapter";
  });
  const errors = [];
  const unhandled = [];
  const onUnhandled = (reason) => unhandled.push(reason.message);
  process.on("unhandledRejection", onUnhandled);
  try {
    await act(() =>
      root.render(
        createElement(
          Alpha2Canvas,
          props({
            createPlayer: made.createPlayer,
            onError: (error) => errors.push(error.message),
          }),
        ),
      ),
    );
    await settle();
  } finally {
    process.off("unhandledRejection", onUnhandled);
  }
  assert.deepEqual(errors, ["no GPU adapter"]);
  assert.deepEqual(unhandled, []);
  assert.deepEqual(made.worker.presented, []);
});

test("the player draws at the project's size", async () => {
  const made = players();
  await act(() =>
    root.render(
      createElement(Alpha2Canvas, props({ createPlayer: made.createPlayer })),
    ),
  );
  await settle();
  const [{ canvas }] = made.worker.of("initialize");
  assert.deepEqual(
    { width: canvas.width, height: canvas.height },
    { width: 1920, height: 1080 },
  );
});

test("under StrictMode the canvas still presents, from the player of the effect that stayed", async () => {
  const made = players();
  const errors = [];
  await act(() =>
    root.render(
      createElement(
        StrictMode,
        null,
        createElement(
          Alpha2Canvas,
          props({
            createPlayer: made.createPlayer,
            timeTicks: 5n,
            onError: (error) => errors.push(error.message),
          }),
        ),
      ),
    ),
  );
  await settle();
  assert.deepEqual(errors, []);
  assert.equal(made.workers.length, 2, "one player for each setup");
  assert.equal(made.workers[0].terminated, true);
  assert.equal(made.worker.terminated, false);
  assert.deepEqual(made.worker.presented, [5n]);
});

test("an <Activity> hidden and shown again presents again", async () => {
  const made = players();
  const errors = [];
  const element = (mode, timeTicks) =>
    createElement(
      Activity,
      { mode },
      createElement(
        Alpha2Canvas,
        props({
          createPlayer: made.createPlayer,
          timeTicks,
          onError: (error) => errors.push(error.message),
        }),
      ),
    );
  await act(() => root.render(element("visible", 1n)));
  await settle();
  await act(() => root.render(element("hidden", 1n)));
  await settle();
  assert.equal(made.workers[0].terminated, true, "hiding destroys effects");
  await act(() => root.render(element("visible", 2n)));
  await settle();
  assert.deepEqual(errors, []);
  assert.deepEqual(made.worker.presented, [2n]);
  assert.equal(made.worker.terminated, false);
});

test("a time whose frames arrive after it was asked for is shown, though the time does not change", async () => {
  const made = players((worker) => {
    worker.sources = ["cam"];
  });
  const shown = [];
  const element = (timeTicks) =>
    createElement(
      Alpha2Canvas,
      props({
        createPlayer: made.createPlayer,
        timeTicks,
        onPresented: (frame) => shown.push(frame.presentedTimeTicks),
      }),
    );
  await act(() => root.render(element(0n)));
  await settle();
  // A paused playhead moved to a time nothing has decoded yet.
  await act(() => root.render(element(4_000n)));
  await settle();
  assert.deepEqual(shown, [0n, 4_000n]);
  assert.equal(made.worker.of("upload-video-frame").length, 2);
});

test("replacing the player while one presentation runs reports nothing for it", async () => {
  let release;
  const first = players((worker) => {
    worker.hold = new Promise((resolve) => (release = resolve));
  });
  const second = players();
  const errors = [];
  const element = (createPlayer) =>
    createElement(
      Alpha2Canvas,
      props({
        createPlayer,
        timeTicks: 9n,
        onError: (error) => errors.push(error.message),
      }),
    );
  await act(() => root.render(element(first.createPlayer)));
  await settle();
  assert.deepEqual(first.worker.presented, [9n], "a presentation is running");
  await act(() => root.render(element(second.createPlayer)));
  release();
  await settle();
  assert.deepEqual(errors, []);
  assert.deepEqual(second.worker.presented, [9n]);
});
