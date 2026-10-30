// The bridge's preview and playback, run in Node against a recording renderer (support/): what is
// drawn where, what is reported, and how a failed preview worker hands over to the main thread.
import {
  FakeCanvas,
  failingWorker,
  workingWorker,
} from "./support/preview-stubs.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
  createEditor,
  createEmptyDocument,
  EditorError,
} from "../dist/index.js";
import { createBrowserBridge } from "../dist/browser/index.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

let wasm = null;
async function engineWasm() {
  if (wasm) return wasm;
  wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const ok = (result) => assert.ok(result.ok, result.error?.message);
const settle = (ms = 0) => new Promise((resolve) => setTimeout(resolve, ms));

/** A color clip on its own track, one second long; returns the track's id. */
function addColor(editor) {
  const id = crypto.randomUUID();
  const trackId = crypto.randomUUID();
  ok(
    editor.dispatch({
      type: "transaction",
      commands: [
        {
          type: "asset.put",
          asset: { id, kind: "generated", name: "Color", contentRevision: 0 },
          source: {
            id,
            kind: "generated",
            generator: {
              type: "fill",
              paint: {
                type: "solid",
                color: { red: 0.8, green: 0.3, blue: 0.2, alpha: 1 },
              },
            },
          },
        },
        {
          type: "track.add",
          track: { id: trackId, kind: "video", name: "V1", clips: [] },
        },
        {
          type: "clip.insert",
          trackId,
          clip: {
            id: crypto.randomUUID(),
            source_id: id,
            timeline_range: { start: 0, end: 120000 },
            source_range: { start: 0, end: 120000 },
            time_map: { type: "forward" },
          },
        },
      ],
    }),
  );
  return trackId;
}

async function session({ workerFactory, document, previewScale } = {}) {
  const reported = [];
  const editor = createEditor({
    document: document ?? createEmptyDocument({ width: 64, height: 36 }),
  });
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions: await engineWasm(),
    runtimeAssets: workerFactory ? { workerFactory } : {},
    onError: (error) => reported.push(error),
    ...(previewScale === undefined ? {} : { previewScale }),
  });
  return { editor, bridge, reported };
}

const latestEngine = () => globalThis.stubEngine.engines.at(-1);

for (const fail of ["init", "load", "replace-project"]) {
  test(
    `a preview worker that fails at ${fail} hands the surface to the main renderer`,
    { skip },
    async () => {
      let worker;
      const { editor, bridge, reported } = await session({
        workerFactory: () => (worker = failingWorker(fail)),
      });
      addColor(editor);
      const canvas = new FakeCanvas(64, 36);
      const surface = await bridge.bindPreview(canvas);
      const rendered = await bridge.seek(0);
      const again = await bridge.seek(0);
      assert.equal(rendered, true);
      assert.equal(again, true);
      assert.equal(bridge.previewBackend, "main-thread");
      assert.ok(worker.terminated, "the failed worker is let go");
      // The main renderer draws off screen and each frame is copied to the bound surface.
      assert.ok(latestEngine().renders.length >= 2);
      assert.ok(canvas.transferred >= 2, `copied ${canvas.transferred}`);
      assert.deepEqual(
        reported.map((e) => e.code),
        ["preview_worker_failed"],
      );
      await surface.dispose();
      await bridge.dispose();
      editor.dispose();
    },
  );
}

test(
  "a worker seek that fails keeps drawing on a bound OffscreenCanvas",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session({
      workerFactory: () => failingWorker("seek"),
    });
    addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(0), true);
    const copied = canvas.transferred;
    assert.equal(await bridge.seek(12000), true);
    assert.equal(bridge.previewBackend, "main-thread");
    assert.ok(canvas.transferred > copied, "later frames reach the surface");
    assert.deepEqual(
      reported.map((e) => e.code),
      ["preview_worker_failed"],
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a preview made again after a lost GPU stays on the main thread once the worker was given up",
  { skip },
  async () => {
    let workers = 0;
    const { editor, bridge, reported } = await session({
      workerFactory: () => (workers++, failingWorker("seek")),
    });
    addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.previewBackend, "main-thread");
    // The main renderer loses its GPU under the next frame and the preview is made again.
    const lostEngine = latestEngine();
    lostEngine.render = async () => {
      lostEngine.deviceLost = true;
      throw Object.assign(new Error("the device was lost"), {
        code: "GPU_DEVICE_LOST",
      });
    };
    const copied = canvas.transferred;
    assert.equal(await bridge.seek(12000), true);
    assert.equal(bridge.previewRecoveries, 1);
    assert.notEqual(latestEngine(), lostEngine);
    assert.equal(bridge.previewBackend, "main-thread");
    assert.equal(workers, 1, "the failed worker is not started again");
    assert.ok(
      canvas.transferred > copied,
      "the new renderer's frame is copied",
    );
    assert.equal(await bridge.seek(24000), true);
    assert.deepEqual(
      reported.map((e) => e.code),
      ["preview_worker_failed"],
    );
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(workers, 2);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a preview worker that loses its GPU is replaced by the main renderer without a report",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session({
      workerFactory: () => failingWorker("seek", "GPU_DEVICE_LOST"),
    });
    addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.previewBackend, "main-thread");
    assert.ok(canvas.transferred >= 1);
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "Play with every content track hidden plays nothing, never the last loaded project",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    const trackId = addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(0), true);
    ok(
      editor.dispatch({
        type: "track.metadata",
        id: trackId,
        patch: { enabled: false },
      }),
    );
    // The refresh the edit scheduled runs before Play.
    await settle(20);
    assert.equal(await bridge.seek(0), false);
    const before = latestEngine().renders.length;
    await bridge.play();
    await settle(40);
    assert.equal(bridge.playing, false);
    assert.equal(latestEngine().renders.length, before);
    bridge.pause();
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "Play in a document opened with its only track hidden neither throws nor plays",
  { skip },
  async () => {
    const first = createEditor({
      document: createEmptyDocument({ width: 64, height: 36 }),
    });
    const trackId = addColor(first);
    ok(
      first.dispatch({
        type: "track.metadata",
        id: trackId,
        patch: { enabled: false },
      }),
    );
    const { editor, bridge, reported } = await session({
      document: first.document,
    });
    first.dispose();
    await bridge.bindPreview(new FakeCanvas(64, 36));
    await bridge.play();
    await settle(20);
    assert.equal(bridge.playing, false);
    assert.equal(latestEngine().renders.length, 0);
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "playback goes on after the preview recovers from a lost GPU, without a report",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    await bridge.play(1, { loop: true });
    await settle(60);
    assert.equal(bridge.playing, true);
    // The device is lost under a playing frame: the next render fails with it, once.
    const lostEngine = latestEngine();
    lostEngine.render = async () => {
      lostEngine.deviceLost = true;
      throw Object.assign(new Error("the device was lost"), {
        code: "GPU_DEVICE_LOST",
      });
    };
    for (let wait = 0; wait < 100 && bridge.previewRecoveries === 0; wait++)
      await settle(10);
    assert.equal(bridge.previewRecoveries, 1);
    const recovered = latestEngine();
    assert.notEqual(recovered, lostEngine);
    const from = bridge.tick;
    await settle(120);
    assert.equal(bridge.playing, true, "playback goes on");
    assert.ok(
      bridge.tick > from,
      `the time moves on: ${from} -> ${bridge.tick}`,
    );
    assert.ok(recovered.renders.length >= 2, "the new renderer draws it");
    assert.deepEqual(reported, []);

    recovered.render = async () => {
      recovered.deviceLost = true;
      bridge.pause();
      throw Object.assign(new Error("the device was lost"), {
        code: "GPU_DEVICE_LOST",
      });
    };
    for (let wait = 0; wait < 100 && bridge.previewRecoveries === 1; wait++)
      await settle(10);
    await settle(60);
    assert.equal(bridge.playing, false);
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a failed preview render a caller awaits is reported to that caller only",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    latestEngine().render = async () => {
      throw new Error("injected render failure");
    };
    await assert.rejects(bridge.seek(0), /injected render failure/);
    await settle();
    assert.deepEqual(reported, []);
    // Work the bridge starts itself (a refresh after an edit) reports to onError, once.
    ok(editor.dispatch({ type: "project.update", patch: { name: "edited" } }));
    await settle(40);
    assert.deepEqual(
      reported.map((e) => e.message),
      ["injected render failure"],
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a preview that waits for media names it by the asset's name",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    ok(
      editor.dispatch({
        type: "asset.put",
        asset: {
          id: "src-0b7c2a6e",
          kind: "video",
          name: "interview.mov",
          contentRevision: 0,
          missing: true,
        },
        source: { id: "src-0b7c2a6e", kind: "video", locator: "browser://x" },
      }),
    );
    const trackId = crypto.randomUUID();
    ok(
      editor.dispatch({
        type: "transaction",
        commands: [
          {
            type: "track.add",
            track: { id: trackId, kind: "video", name: "V1", clips: [] },
          },
          {
            type: "clip.insert",
            trackId,
            clip: {
              id: crypto.randomUUID(),
              source_id: "src-0b7c2a6e",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
              time_map: { type: "forward" },
            },
          },
        ],
      }),
    );
    await bridge.bindPreview(new FakeCanvas(64, 36));
    await assert.rejects(bridge.seek(0), {
      code: "missing_asset",
      entityId: "src-0b7c2a6e",
      message: "Relink interview.mov before preview",
    });
    await bridge.dispose();
    editor.dispose();
  },
);

/**
 * A clock that moves one frame (4000 ticks, 30 fps) per step; under a waiting policy only once the
 * frame asked for was presented, as the engine's `PlaybackClock` does.
 */
function frameClock(policy, frameRate, rate, tick) {
  let at = tick;
  const waits = policy === "preserve_every_video_frame";
  let first = true;
  return {
    step(now, heard, presented) {
      if (first) first = false;
      else if (!waits || presented) at += 4000 * Math.sign(rate);
      return { tick: at, waitForFrame: waits, free() {} };
    },
    free() {},
  };
}

test(
  "a waiting policy asks for each frame once and moves on only after it is shown",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    await bridge.seek(0);
    const engine = latestEngine();
    const from = engine.renders.length;
    globalThis.stubEngine.clock = frameClock;
    try {
      await bridge.play(1, { policy: "preserve_every_video_frame" });
      await settle(150);
      bridge.pause();
    } finally {
      globalThis.stubEngine.clock = null;
    }
    const rendered = engine.renders
      .slice(from)
      .map((r) => Math.round(r.seconds * 120000));
    // Reaching the end draws its last frame once more, where playback stops.
    const ticks =
      rendered.at(-1) === 116000 && rendered.at(-2) === 116000
        ? rendered.slice(0, -1)
        : rendered;
    assert.ok(ticks.length >= 3, `rendered ${ticks}`);
    assert.deepEqual(
      ticks,
      ticks.map((_, i) => i * 4000),
      "every frame, in order, once",
    );
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "playback stops on the last frame's start, and a loop starts again from the range's start",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    globalThis.stubEngine.clock = frameClock;
    try {
      await bridge.seek(108000);
      await bridge.play(1);
      for (let i = 0; i < 100 && bridge.playing; i++) await settle(5);
      assert.equal(bridge.playing, false);
      // The clip ends at 120000: its last frame at 30 fps starts at 116000.
      assert.equal(bridge.tick, 116000);
      await bridge.seek(108000);
      await bridge.play(1, { loop: true });
      let restarted = false;
      for (let i = 0; i < 200 && !restarted; i++) {
        await settle(5);
        restarted = bridge.playing && bridge.tick < 108000;
      }
      bridge.pause();
      assert.ok(restarted, `tick ${bridge.tick}`);
    } finally {
      globalThis.stubEngine.clock = null;
    }
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "an edit during playback plays on with the edited project, and a pause or seek first keeps it stopped",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    globalThis.stubEngine.clock = frameClock;
    try {
      await bridge.play(1, { loop: true });
      await settle(40);
      assert.equal(bridge.playing, true);
      // An empty track changes what the renderer loads, so the preview loads the project again.
      const engine = latestEngine();
      const loads = engine.loads.length;
      ok(
        editor.dispatch({
          type: "track.add",
          track: {
            id: crypto.randomUUID(),
            kind: "video",
            name: "V2",
            clips: [],
          },
        }),
      );
      await settle(60);
      assert.ok(engine.loads.length > loads, "the edited project is loaded");
      const edited = engine.loads.at(-1);
      assert.equal(edited.timeline.tracks.length, 2);
      const from = bridge.tick;
      const drawn = engine.renders.length;
      await settle(80);
      assert.equal(bridge.playing, true, "playback goes on");
      assert.notEqual(bridge.tick, from, "the time moves on");
      assert.ok(engine.renders.length > drawn, "frames are drawn");
      assert.equal(
        engine.renders.at(-1).project,
        edited,
        "from the edited project",
      );

      ok(editor.dispatch({ type: "project.update", patch: { name: "a" } }));
      ok(
        editor.dispatch({
          type: "track.add",
          track: {
            id: crypto.randomUUID(),
            kind: "video",
            name: "V3",
            clips: [],
          },
        }),
      );
      bridge.pause();
      await settle(80);
      assert.equal(bridge.playing, false);

      await bridge.play(1, { loop: true });
      await settle(40);
      ok(
        editor.dispatch({
          type: "track.add",
          track: {
            id: crypto.randomUUID(),
            kind: "video",
            name: "V4",
            clips: [],
          },
        }),
      );
      await settle(40);
      await bridge.seek(0);
      await settle(80);
      assert.equal(bridge.playing, false);
      assert.equal(bridge.tick, 0);
    } finally {
      globalThis.stubEngine.clock = null;
    }
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a non-waiting policy asks for each frame once however often the display refreshes",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    await bridge.seek(0);
    const engine = latestEngine();
    engine.lastDiagnostics = { host: {} };
    const from = engine.renders.length;
    // Three display refreshes per 30 fps frame, as a 90 Hz display gives: the clock asks for the
    // same frame start three times.
    let steps = 0;
    globalThis.stubEngine.clock = (policy, frameRate, rate, tick) => ({
      step: () => ({
        tick: tick + Math.floor(steps++ / 3) * 4000,
        waitForFrame: false,
        free() {},
      }),
      free() {},
    });
    try {
      await bridge.play(1);
      await settle(150);
      bridge.pause();
    } finally {
      globalThis.stubEngine.clock = null;
    }
    const ticks = engine.renders
      .slice(from)
      .map((r) => Math.round(r.seconds * 120000));
    assert.ok(ticks.length >= 3, `rendered ${ticks}`);
    assert.deepEqual(
      ticks,
      [...new Set(ticks)],
      `each frame is asked for once: ${ticks}`,
    );
    assert.ok(
      bridge.diagnostics.host.skippedRequests >= ticks.length,
      `skipped ${bridge.diagnostics.host.skippedRequests} of ${steps}`,
    );
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "in worker mode the diagnostics are the worker renderer's, and tracing reaches it",
  { skip },
  async () => {
    let worker;
    let seeks = 0;
    const { editor, bridge, reported } = await session({
      workerFactory: () =>
        (worker = workingWorker({
          // The renderer's sample comes with the first reply only, as a 4 Hz sample does.
          seek: () => ({
            rendered: true,
            unchanged: false,
            bitmap: { close() {} },
            ...(seeks++ === 0
              ? {
                  engineDiagnostics: {
                    host: { renders: 1, backend: "worker" },
                  },
                }
              : {}),
          }),
        })),
    });
    addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(4000), true);
    assert.equal(bridge.previewBackend, "worker");
    // The main-thread renderer draws nothing in worker mode; its sample is not the one reported.
    latestEngine().lastDiagnostics = { host: { renders: 99 } };
    const { host } = bridge.diagnostics;
    assert.equal(host.renders, 1);
    assert.equal(host.backend, "worker");
    assert.equal(host.previewScale, 0.5);
    assert.equal(host.bitmapTransfers, canvas.transferred);
    assert.ok(host.bitmapTransfers >= 2);
    bridge.setPassTiming(true);
    bridge.setTraceUploads(true);
    await settle(5);
    const tracing = worker.requests.filter((r) => r.type === "set-tracing");
    assert.deepEqual(
      tracing.map(({ passTiming, traceUploads }) => [passTiming, traceUploads]),
      [
        [true, false],
        [true, true],
      ],
    );
    assert.equal(latestEngine().passTiming, true, "and the fallback renderer");
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "an unchanged frame is not copied to the bound canvas",
  { skip },
  async () => {
    // From a worker: no bitmap comes.
    {
      const { editor, bridge } = await session({
        workerFactory: () =>
          workingWorker({
            seek: ({ tick }) =>
              tick === 0
                ? { rendered: true, unchanged: false, bitmap: { close() {} } }
                : { rendered: true, unchanged: true },
          }),
      });
      addColor(editor);
      const canvas = new FakeCanvas(64, 36);
      await bridge.bindPreview(canvas);
      assert.equal(await bridge.seek(0), true);
      const copied = canvas.transferred;
      assert.equal(await bridge.seek(4000), true);
      assert.equal(canvas.transferred, copied);
      await bridge.dispose();
      editor.dispose();
    }
    // From the main-thread renderer drawing off screen: its surface is not copied.
    const { editor, bridge } = await session({
      workerFactory: () => failingWorker("seek"),
    });
    addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(0), true);
    assert.equal(bridge.previewBackend, "main-thread");
    const copied = canvas.transferred;
    globalThis.stubEngine.unchanged = () => true;
    try {
      assert.equal(await bridge.seek(4000), true);
    } finally {
      globalThis.stubEngine.unchanged = null;
    }
    assert.equal(canvas.transferred, copied);
    assert.equal(await bridge.seek(8000), true);
    assert.equal(canvas.transferred, copied + 1, "a changed frame is");
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "the bound canvas is the size frames render at, which the preview scale sets",
  { skip },
  async () => {
    for (const [previewScale, size] of [
      [undefined, [32, 18]],
      [1, [64, 36]],
      [0.25, [16, 9]],
    ]) {
      const { editor, bridge } = await session({ previewScale });
      addColor(editor);
      const canvas = new FakeCanvas(1, 1);
      await bridge.bindPreview(canvas);
      assert.deepEqual([canvas.width, canvas.height], size, `${previewScale}`);
      await bridge.dispose();
      editor.dispose();
    }
    const { editor, bridge } = await session();
    addColor(editor);
    const canvas = new FakeCanvas(1, 1);
    await bridge.bindPreview(canvas);
    await bridge.seek(4000);
    const engine = latestEngine();
    const drawn = engine.renders.length;
    await bridge.setPreviewScale(1);
    assert.equal(bridge.previewScale, 1);
    assert.deepEqual([canvas.width, canvas.height], [64, 36]);
    assert.equal(engine.renders.length, drawn + 1, "the frame is drawn again");
    assert.equal(Math.round(engine.renders.at(-1).seconds * 120000), 4000);
    await assert.rejects(bridge.setPreviewScale(0), {
      code: "invalid_preview_scale",
    });
    await assert.rejects(bridge.setPreviewScale(Number.NaN), {
      code: "invalid_preview_scale",
    });
    assert.throws(() => createBrowserBridge({ editor, previewScale: 9 }), {
      code: "invalid_preview_scale",
    });
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "in worker mode the bound canvas takes the render size the worker reports",
  { skip },
  async () => {
    let worker;
    const { editor, bridge } = await session({
      workerFactory: () => (worker = workingWorker()),
      previewScale: 1,
    });
    addColor(editor);
    const canvas = new FakeCanvas(1, 1);
    await bridge.bindPreview(canvas);
    assert.deepEqual([canvas.width, canvas.height], [64, 36]);
    assert.deepEqual(
      worker.requests
        .filter((r) => r.type === "set-preview-scale")
        .map((r) => r.scale),
      [1],
    );
    await bridge.setPreviewScale(0.5);
    assert.deepEqual([canvas.width, canvas.height], [32, 18]);
    assert.equal(worker.requests.at(-1).type, "seek", "and draws again");
    await bridge.dispose();
    editor.dispose();
  },
);

const effect = (id, version = 1, extra = {}) => ({
  id,
  version,
  kind: "effect",
  ...extra,
});
const effectRequests = (worker) =>
  worker.requests
    .filter((r) => r.type === "register-effects")
    .map((r) => r.effects.map((e) => `${e.id}@${e.version}`));

test(
  "runtime effects reach the worker and the main renderer, kept from before binding, each once",
  { skip },
  async () => {
    let worker;
    const { editor, bridge, reported } = await session({
      workerFactory: () => (worker = workingWorker()),
    });
    addColor(editor);
    await bridge.registerEffects([effect("blur")]);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(bridge.previewBackend, "worker");
    assert.deepEqual(latestEngine().effects, ["blur@1"]);
    assert.deepEqual(effectRequests(worker), [["blur@1"]]);
    const seeks = () => worker.requests.filter((r) => r.type === "seek").length;
    const before = seeks();
    // blur@1 again is nothing new; glow@1 is compiled on both and the frame drawn again.
    await bridge.registerEffects([effect("blur"), effect("glow")]);
    await settle(10);
    assert.deepEqual(latestEngine().effects, ["blur@1", "glow@1"]);
    assert.deepEqual(effectRequests(worker), [["blur@1"], ["glow@1"]]);
    assert.ok(seeks() > before, "the frame is drawn again");
    await bridge.registerEffects([effect("glow")]);
    assert.equal(effectRequests(worker).length, 2);
    await assert.rejects(
      bridge.registerEffects([effect("glow", 1, { label: "Glow 2" })]),
      (error) =>
        error.code === "effect_conflict" && error.entityId === "glow@1",
    );
    await assert.rejects(
      bridge.registerEffects([{ id: "nameless" }]),
      (error) => error.code === "invalid_effect",
    );
    assert.deepEqual(reported, []);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "an effect the device refuses when binding is dropped and reported, and one refused while bound keeps nothing",
  { skip },
  async () => {
    globalThis.stubEngine.registerEffects = (_engine, definitions) => {
      const refused = definitions.find((d) => d.id.startsWith("bad"));
      if (refused)
        throw new EditorError(
          "invalid_effect",
          `Effect ${refused.id}@1 is not one the engine runs`,
          "preview",
          `${refused.id}@1`,
        );
    };
    const { editor, bridge, reported } = await session();
    try {
      addColor(editor);
      await bridge.registerEffects([effect("good"), effect("bad")]);
      await bridge.bindPreview(new FakeCanvas(64, 36));
      assert.deepEqual(latestEngine().effects, ["good@1"]);
      assert.deepEqual(
        reported.map((e) => [e.code, e.entityId]),
        [["invalid_effect", "bad@1"]],
      );
      await assert.rejects(
        bridge.registerEffects([effect("fine"), effect("bad-too")]),
        (error) => error.code === "invalid_effect",
      );
      // A lost GPU makes the preview again with what is kept: neither the dropped one nor the
      // refused call.
      const lostEngine = latestEngine();
      lostEngine.render = async () => {
        lostEngine.deviceLost = true;
        throw Object.assign(new Error("the device was lost"), {
          code: "GPU_DEVICE_LOST",
        });
      };
      assert.equal(await bridge.seek(4000), true);
      assert.equal(bridge.previewRecoveries, 1);
      assert.notEqual(latestEngine(), lostEngine);
      assert.deepEqual(latestEngine().effects, ["good@1"]);
      assert.equal(reported.length, 1);
    } finally {
      globalThis.stubEngine.registerEffects = null;
      await bridge.dispose();
      editor.dispose();
    }
  },
);

test(
  "a preview worker that refuses an effect the main renderer took hands the surface over",
  { skip },
  async () => {
    let worker;
    const { editor, bridge, reported } = await session({
      workerFactory: () => (worker = failingWorker("register-effects")),
    });
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(bridge.previewBackend, "worker");
    await bridge.registerEffects([effect("blur")]);
    assert.equal(bridge.previewBackend, "main-thread");
    assert.ok(worker.terminated);
    assert.deepEqual(latestEngine().effects, ["blur@1"]);
    assert.deepEqual(
      reported.map((e) => e.code),
      ["preview_worker_failed"],
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "an export registers the effects kept when it starts, and one the engine refuses fails it as invalid input, once",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    addColor(editor);
    await bridge.registerEffects([effect("blur")]);
    await bridge.conform("export_original");
    const engines = globalThis.stubEngine.engines.length;
    const exported = bridge.startExport({ output: { kind: "memory" } });
    // Registered later, it is not this export's.
    await bridge.registerEffects([effect("glow")]);
    await exported.result.catch(() => {});
    const exportEngines = globalThis.stubEngine.engines.slice(engines);
    assert.ok(exportEngines.length >= 1);
    for (const engine of exportEngines)
      assert.deepEqual(engine.effects, ["blur@1"]);

    globalThis.stubEngine.registerEffects = (
      _engine,
      _definitions,
      operation,
    ) => {
      if (operation === "export")
        throw new EditorError(
          "invalid_effect",
          "Effect blur@1 is not one the engine runs",
          operation,
          "blur@1",
        );
    };
    try {
      const refused = bridge.startExport({ output: { kind: "memory" } });
      await assert.rejects(
        refused.result,
        (error) =>
          error.code === "invalid_effect" &&
          error.failureCode === "INVALID_INPUT",
      );
      assert.equal(refused.attempts.length, 1, "not retried");
    } finally {
      globalThis.stubEngine.registerEffects = null;
    }
    await bridge.dispose();
    editor.dispose();
  },
);
