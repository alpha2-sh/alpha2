// How long the browser runtime keeps what it uploaded: an export frame's uploads live until the
// engine renders that frame, and an edit keeps every source whose media did not change. Node has
// no GPU, so the wasm engine renderer is one that keeps uploads as the frame store does.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import * as wasm from "alpha2/wasm";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE_WASM=1 fails instead.
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

const TICKS_PER_SECOND = 120_000n;
/**
 * What one presented 4K upload retains on WebGPU: the Rgba16Float output texture and the RGBA8
 * source still pending.
 */
const PRESENTED_4K_BYTES = 3840 * 2160 * 8 + 3840 * 2160 * 4;

/**
 * A renderer that keeps uploads as `bindings/alpha2-wasm/src/frame_store.rs` does: every upload is
 * a new generation, the oldest generation goes first past the byte budget, and releasing a
 * generation drops it even while it is live. Export follows the engine's schedule: one frame per
 * second, and a frame waits until every source it needs is uploaded.
 */
function frameStoreRenderer({ sources, receiptBytes = 64 }) {
  let next = 0n;
  const frames = new Map();
  const images = new Map();
  let exportAt = 0n;
  let exportEnd = 0n;
  const needsAt = (ticks) =>
    sources.map((sourceId) => ({
      clipId: sourceId,
      sourceId,
      mediaTimeTicks: String(ticks),
    }));
  const missingAt = (ticks) =>
    needsAt(ticks)
      .filter(
        ({ sourceId, mediaTimeTicks }) =>
          !images.has(sourceId) && !frames.has(`${sourceId}@${mediaTimeTicks}`),
      )
      .map(({ sourceId }) => sourceId);
  const release = (generation) => {
    for (const map of [frames, images])
      for (const [key, entry] of map) if (entry === generation) map.delete(key);
  };
  const insert = (map, key) => {
    next += 1n;
    map.set(key, next);
    renderer.uploads.push(key);
    return { generation: next, retainedBytes: receiptBytes };
  };
  const renderer = {
    uploads: [],
    released: [],
    releasedSources: [],
    cleared: 0,
    deviceLost: false,
    supportsExternalVideoFrame: false,
    builtinCatalog: () => JSON.stringify({ effects: [], transitions: [] }),
    setResultCacheBudget() {},
    loadProject() {},
    resetPlayer() {},
    frameNeeds: (ticks) => JSON.stringify(needsAt(ticks)),
    uploadRgbaFrame: (id, ticks) => insert(frames, `${id}@${ticks}`),
    uploadRgbaImage: (id) => insert(images, id),
    aliasVideoFrame(id, existing, requested) {
      const entry = frames.get(`${id}@${existing}`);
      if (entry === undefined) return false;
      frames.set(`${id}@${requested}`, entry);
      return true;
    },
    holdsImage: (id, generation) => images.get(id) === generation,
    releaseFrameGeneration(generation) {
      renderer.released.push(generation);
      release(generation);
    },
    releaseSourceFrames(id) {
      renderer.releasedSources.push(id);
      images.delete(id);
      for (const key of frames.keys())
        if (key.startsWith(`${id}@`)) frames.delete(key);
    },
    clearFrames() {
      renderer.cleared += 1;
      frames.clear();
      images.clear();
    },
    renderAt(ticks) {
      const missingSources = missingAt(ticks);
      return JSON.stringify({
        requestedTimeTicks: Number(ticks),
        presentedTimeTicks: missingSources.length ? null : Number(ticks),
        missingSources,
        repeated: false,
      });
    },
    beginExport(start, end) {
      exportAt = start;
      exportEnd = end;
    },
    beginExportFrame() {
      if (exportAt >= exportEnd) return { kind: "complete" };
      const missingSources = missingAt(exportAt);
      if (missingSources.length)
        return {
          kind: "waiting",
          timeTicks: String(exportAt),
          missingSources,
        };
      exportAt += TICKS_PER_SECOND;
      return {
        kind: "rendered",
        retainedBytes: 1,
        readback: Promise.resolve(null),
      };
    },
  };
  return renderer;
}

/**
 * Decodes every source: video as RGBA frames at the requested time, stills as one array per media
 * object, as `SourcePixels` caches them.
 */
function pixelSource(library) {
  const stills = new WeakMap();
  return {
    release: async () => {},
    pixelsFor: async (layer, seconds) => {
      const media = library.get(layer.id);
      if (layer.kind === "image") {
        if (!stills.has(media)) stills.set(media, new Uint8Array(4 * 4 * 4));
        return stills.get(media);
      }
      return {
        kind: "rgba-frame",
        pixels: new Uint8Array(4 * 4 * 4),
        physicalTimeMicroseconds: Math.round(seconds * 1_000_000),
        sourceWidth: 4,
        sourceHeight: 4,
      };
    },
  };
}

/** Starts the editor's `Engine` on `renderer`, in place of a WebGPU canvas. */
async function startEngine(renderer, library) {
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  const { Engine } = await import("../dist/browser/runtime/engine.js");
  const restore = [];
  for (const [name, value] of [
    ["OffscreenCanvas", class OffscreenCanvas {}],
    ["VideoFrame", class VideoFrame {}],
  ]) {
    if (!(name in globalThis)) {
      globalThis[name] = value;
      restore.push(() => delete globalThis[name]);
    }
  }
  const create = wasm.WebEffectRegistry.createForCanvas;
  wasm.WebEffectRegistry.createForCanvas = async () => renderer;
  try {
    const engine = new Engine(
      new globalThis.OffscreenCanvas(),
      pixelSource(library),
      library,
    );
    await engine.start();
    return engine;
  } finally {
    wasm.WebEffectRegistry.createForCanvas = create;
    for (const undo of restore) undo();
  }
}

const context = (ids, kinds = {}) => ({
  width: 3840,
  height: 2160,
  liveSourceIds: new Set(ids),
  getLayer: (id) => ({ id, kind: kinds[id] ?? "video" }),
});

test(
  "an export frame whose uploads outgrow the retained budget renders",
  { skip },
  async () => {
    // Three 4K video layers at one time: their uploads together are over the 256 MiB the frame
    // supply retains.
    const ids = ["a", "b", "c"];
    assert.ok(ids.length * PRESENTED_4K_BYTES > 256 * 1024 * 1024);
    const renderer = frameStoreRenderer({
      sources: ids,
      receiptBytes: PRESENTED_4K_BYTES,
    });
    const library = new Map(ids.map((id) => [id, { id, kind: "video" }]));
    const engine = await startEngine(renderer, library);
    engine.loadProject({ effect_resources: [] });
    renderer.beginExport(0n, 3n * TICKS_PER_SECOND);
    // The editor exporter's loop: a waiting frame is supplied and asked again, four times at most.
    let rendered = 0;
    let waitingAt = null;
    let waits = 0;
    for (;;) {
      const ticket = engine.beginExportFrame();
      if (ticket.kind === "complete") break;
      if (ticket.kind === "waiting") {
        waits = ticket.timeTicks === waitingAt ? waits + 1 : 0;
        waitingAt = ticket.timeTicks;
        assert.ok(
          waits < 4,
          `export stalled at ${ticket.timeTicks} waiting for ${ticket.missingSources.join(", ")}`,
        );
        await engine.supply(Number(ticket.timeTicks) / 120_000, {
          context: context(ids),
        });
        continue;
      }
      waitingAt = null;
      rendered += 1;
    }
    assert.equal(rendered, 3);
    // Each frame is uploaded once, and what a rendered frame used is retired, so the retained
    // budget still bounds it.
    assert.equal(renderer.uploads.length, 9);
    assert.ok(renderer.released.length > 0);
  },
);

test(
  "an edit keeps the uploads of every source whose media did not change",
  { skip },
  async () => {
    const ids = ["logo", "video", "relinked"];
    const kinds = { logo: "image", relinked: "image" };
    const renderer = frameStoreRenderer({ sources: ids });
    const library = new Map(ids.map((id) => [id, { id }]));
    const engine = await startEngine(renderer, library);
    const project = { effect_resources: [] };
    engine.loadProject(project);
    assert.equal(
      (await engine.render(0, { context: context(ids, kinds) })).rendered,
      true,
    );
    assert.deepEqual(renderer.uploads, ["logo", "video@0", "relinked"]);
    // Moving a clip reloads the project; no media changed, so nothing is uploaded again.
    engine.loadProject(project);
    assert.equal(
      (await engine.render(0, { context: context(ids, kinds) })).rendered,
      true,
    );
    assert.deepEqual(renderer.uploads, ["logo", "video@0", "relinked"]);
    assert.equal(renderer.cleared, 0);
    // A relink keeps the source's ID and gives the library another runtime for it: that source
    // alone goes.
    library.set("relinked", { id: "relinked" });
    engine.loadProject(project);
    assert.equal(
      (await engine.render(0, { context: context(ids, kinds) })).rendered,
      true,
    );
    assert.deepEqual(renderer.uploads, [
      "logo",
      "video@0",
      "relinked",
      "relinked",
    ]);
    assert.deepEqual(renderer.releasedSources, ["relinked"]);
  },
);
