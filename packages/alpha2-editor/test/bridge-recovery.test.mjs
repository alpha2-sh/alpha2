// The bridge's GPU-loss recovery ladder and the CPU renderer it falls to, in Node against the
// recording renderer (support/): which renderer a preview or an export is made with after a loss.
import { FakeCanvas, resetPageGpuLosses } from "./support/preview-stubs.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor, createEmptyDocument } from "../dist/index.js";
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
const skip =
  !existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")) &&
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

async function session() {
  resetPageGpuLosses();
  const reported = [];
  const editor = createEditor({
    document: createEmptyDocument({ width: 64, height: 36 }),
  });
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions: await engineWasm(),
    runtimeAssets: {},
    onError: (error) => reported.push(error),
  });
  return { editor, bridge, reported };
}

/** A color clip, one second long. */
function addColor(editor) {
  const id = crypto.randomUUID();
  const trackId = crypto.randomUUID();
  const result = editor.dispatch({
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
  });
  assert.ok(result.ok, result.error?.message);
}

const latestEngine = () => globalThis.stubEngine.engines.at(-1);

/** Makes `engine` lose its GPU under its next frame. */
function loseUnderNextFrame(engine) {
  engine.render = async () => {
    engine.deviceLost = true;
    throw Object.assign(new Error("the device was lost"), {
      code: "GPU_DEVICE_LOST",
    });
  };
}

test(
  "a lost gpu preview rebuilds on the cpu after two losses",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    const canvas = new FakeCanvas(64, 36);
    await bridge.bindPreview(canvas);
    assert.equal(await bridge.seek(0), true);
    assert.equal(latestEngine().options.renderer, "auto");
    assert.equal(bridge.capabilities.renderer.renderer, "gpu");
    assert.equal(bridge.capabilities.renderer.runtimeEffects, true);

    // A first loss: the GPU again, on a fresh internal canvas.
    const first = latestEngine();
    loseUnderNextFrame(first);
    assert.equal(await bridge.seek(4000), true);
    assert.equal(bridge.previewRecoveries, 1);
    assert.notEqual(latestEngine(), first);
    assert.notEqual(latestEngine().canvas, first.canvas);
    assert.equal(latestEngine().options.renderer, "auto");

    // A second loss in this page: the browser withdraws GPU access at its next reset.
    loseUnderNextFrame(latestEngine());
    const copied = canvas.transferred;
    assert.equal(await bridge.seek(8000), true);
    assert.equal(bridge.previewRecoveries, 2);
    assert.equal(latestEngine().options.renderer, "cpu");
    assert.equal(bridge.capabilities.renderer.renderer, "cpu");
    assert.equal(bridge.capabilities.renderer.runtimeEffects, false);
    assert.ok(canvas.transferred > copied, "the CPU frame is copied");
    // Recovery is not reported; the preview renders on.
    assert.deepEqual(reported, []);

    // A runtime effect is kept but reported once as not drawn on the CPU.
    await bridge.registerEffects([{ id: "blur", version: 1 }]);
    await bridge.registerEffects([{ id: "blur", version: 1 }]);
    assert.deepEqual(latestEngine().effects, []);
    assert.deepEqual(
      reported.map((error) => [error.code, error.entityId]),
      [["effect_unavailable", "blur@1"]],
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a preview whose every recovery rung fails is unavailable, reported once",
  { skip },
  async () => {
    const { editor, bridge, reported } = await session();
    addColor(editor);
    await bridge.bindPreview(new FakeCanvas(64, 36));
    assert.equal(await bridge.seek(0), true);
    loseUnderNextFrame(latestEngine());
    globalThis.stubEngine.start = async () => {
      throw Object.assign(new Error("no renderer"), {
        code: "GPU_INIT_FAILED",
      });
    };
    try {
      assert.equal(await bridge.seek(4000), false);
      assert.equal(await bridge.seek(8000), false);
      assert.deepEqual(
        reported.map((error) => error.code),
        ["preview_unavailable"],
      );
      assert.equal(reported[0].failureCode, "DEVICE_LOST");
    } finally {
      globalThis.stubEngine.start = null;
    }
    await bridge.dispose();
    editor.dispose();
  },
);

test("export retries on the cpu after device hung", { skip }, async () => {
  const { editor, bridge } = await session();
  addColor(editor);
  await bridge.conform("export_original");
  const engines = globalThis.stubEngine.engines.length;
  // The first attempt's GPU stops on the export's own work.
  globalThis.stubEngine.start = async (engine) => {
    if (engine.options.renderer !== "cpu")
      throw Object.assign(new Error("the GPU device hung"), {
        code: "GPU_DEVICE_HUNG",
      });
  };
  try {
    const job = bridge.startExport({ output: { kind: "memory" } });
    await job.result.catch(() => {});
    const renderers = globalThis.stubEngine.engines
      .slice(engines)
      .map((engine) => engine.options.renderer);
    assert.deepEqual(renderers.slice(0, 2), ["auto", "cpu"]);
    const [first, second] = job.attempts;
    assert.equal(first.renderer, "gpu");
    assert.equal(first.failure.code, "DEVICE_HUNG");
    assert.equal(second.renderer, "cpu");
  } finally {
    globalThis.stubEngine.start = null;
  }
  await bridge.dispose();
  editor.dispose();
});

test(
  "an export with runtime effects does not move to the cpu",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    addColor(editor);
    await bridge.registerEffects([{ id: "blur", version: 1 }]);
    await bridge.conform("export_original");
    globalThis.stubEngine.start = async (engine) => {
      if (engine.options.renderer !== "cpu")
        throw Object.assign(new Error("the GPU device hung"), {
          code: "GPU_DEVICE_HUNG",
        });
    };
    try {
      const job = bridge.startExport({ output: { kind: "memory" } });
      await job.result.catch(() => {});
      assert.equal(job.attempts.length, 1);
      assert.equal(
        job.failure?.code ?? job.attempts[0].failure.code,
        "DEVICE_HUNG",
      );
    } finally {
      globalThis.stubEngine.start = null;
    }
    await bridge.dispose();
    editor.dispose();
  },
);
