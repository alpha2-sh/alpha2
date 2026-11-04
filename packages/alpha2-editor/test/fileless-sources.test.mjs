// Node has no OffscreenCanvas or FileReader for thumbnails, and cannot fetch the packaged modules.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createDocument, createEditor } from "../dist/index.js";
import { createBrowserBridge } from "../dist/browser/index.js";
import { SourcePixels } from "../dist/browser/runtime/sources.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const packaged = existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm"));
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE=wasm fails instead.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm packages are not built",
  );

globalThis.OffscreenCanvas ??= class {
  constructor(width, height) {
    Object.assign(this, { width, height });
  }
  getContext() {
    return { putImageData() {} };
  }
  async convertToBlob() {
    return new Blob([new Uint8Array(1)], { type: "image/png" });
  }
};
globalThis.ImageData ??= class {
  constructor(data, width, height) {
    Object.assign(this, { data, width, height });
  }
};
globalThis.FileReader ??= class {
  readAsDataURL() {
    this.result = "data:image/png;base64,AA==";
    queueMicrotask(() => this.onload());
  }
};

async function modules() {
  const engine = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  engine.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  (await import("alpha2/extensions-wasm")).initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
  });
  return engine;
}

const latex = (formula, size = 48) => ({
  format: "latex",
  latex: formula,
  font_size: size,
});

async function session(document) {
  const assetDecisions = await modules();
  const editor = createEditor(document ? { document } : {});
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions,
    mediaAdapter: {
      async stage() {
        throw new Error("no files in this test");
      },
    },
  });
  return { editor, bridge };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 50));
const sourceOf = (editor, id) =>
  editor.document.project.sources.find((source) => source.id === id);

test(
  "a source that reads no file is added and edited in one undoable step each",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    const { assetId } = await bridge.putProviderSource({
      id: "eq",
      name: "Quadratic",
      provider: latex("x^2"),
    });
    assert.equal(assetId, "eq");
    assert.deepEqual(
      [sourceOf(editor, "eq").kind, sourceOf(editor, "eq").locator],
      ["image", ""],
    );
    assert.equal(editor.document.assets.eq.missing, false);
    const small = bridge.getAsset("eq").width;
    assert.ok(small > 0 && bridge.getAsset("eq").thumbnail, "described");

    await bridge.putProviderSource({ id: "eq", provider: latex("x^2", 96) });
    assert.equal(bridge.getAsset("eq").provider.font_size, 96);
    assert.ok(bridge.getAsset("eq").width > small);
    editor.undo();
    assert.equal(sourceOf(editor, "eq").provider.font_size, 48);
    assert.equal(bridge.getAsset("eq").provider.font_size, 48);
    await settle();
    assert.equal(bridge.getAsset("eq").width, small, "described again");
    editor.undo();
    assert.equal(sourceOf(editor, "eq"), undefined);
    editor.redo();
    assert.equal(sourceOf(editor, "eq").provider.latex, "x^2");

    const shown = editor.dispatch({
      type: "transaction",
      commands: [
        {
          type: "track.add",
          track: { id: "v", kind: "video", name: "V", clips: [] },
        },
        {
          type: "clip.insert",
          trackId: "v",
          clip: {
            id: "c",
            source_id: "eq",
            timeline_range: { start: 0, end: 120000 },
            source_range: { start: 0, end: 120000 },
            time_map: { type: "forward" },
          },
        },
      ],
    });
    assert.ok(shown.ok, shown.error?.message);
    const report = await bridge.conform("preview");
    const eq = report.sources.find((source) => source.source_id === "eq");
    assert.deepEqual(eq.issues, [], "online without a file");
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a provider that reads a file, or a source that does not open, is refused and changes nothing",
  { skip },
  async () => {
    const { editor, bridge } = await session();
    await assert.rejects(
      bridge.putProviderSource({ provider: { format: "svg" } }),
      { code: "unsupported_media" },
    );
    await assert.rejects(
      bridge.putProviderSource({
        id: "bad",
        provider: latex(String.raw`\frac{1}`),
      }),
      { code: "unreadable_media", message: /missing argument/ },
    );
    assert.equal(editor.document.project.sources.length, 0);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a project read back keeps its file-less sources drawable and only file sources missing",
  { skip },
  async () => {
    const document = createDocument({
      schema_version: 1,
      id: "reopened",
      name: "Reopened",
      settings: {
        width: 64,
        height: 36,
        frame_rate: { num: 30, den: 1 },
        sample_rate: 48000,
      },
      sources: [
        { id: "eq", kind: "image", locator: "", provider: latex("y") },
        {
          id: "logo",
          kind: "image",
          locator: "asset://logo",
          provider: { format: "svg" },
        },
      ],
      timeline: { tracks: [] },
    });
    assert.equal(document.assets.eq.missing, false);
    assert.equal(document.assets.logo.missing, true);
    const { editor, bridge } = await session(document);
    assert.equal(bridge.getAsset("eq").provider.latex, "y");
    assert.equal(bridge.getAsset("logo"), null);
    await bridge.dispose();
    editor.dispose();
  },
);

test("a source that reads no file draws without one", { skip }, async () => {
  await modules();
  const pixels = new SourcePixels(null);
  pixels.setOutputSize(64, 36);
  const frame = await pixels.pixelsFor(
    { id: "eq", kind: "image", provider: latex(String.raw`\blacksquare`) },
    0,
  );
  assert.ok(frame.width > 0 && frame.pixels.some((sample) => sample > 0));
});
