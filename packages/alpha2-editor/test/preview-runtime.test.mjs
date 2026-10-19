// The bridge's preview and playback, run in Node against a recording renderer (support/): what is
// drawn where, what is reported, and how a failed preview worker hands over to the main thread.
import { FakeCanvas, failingWorker } from "./support/preview-stubs.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor, createEmptyDocument } from "../dist/index.js";
import { createBrowserBridge } from "../dist/browser/index.js";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

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

async function session({ workerFactory, document } = {}) {
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
