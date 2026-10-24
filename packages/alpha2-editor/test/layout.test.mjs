// A layout generator through the editor: measured by the bridge with the fonts and canvas preview
// draws with, previewed from an EditorDocument, and exported natively with the fonts it draws with.
import { FakeCanvas } from "./support/preview-stubs.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor, createEmptyDocument } from "../dist/index.js";
import { createBrowserBridge } from "../dist/browser/index.js";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmDirectory = join(root, "packages", "alpha2", "wasm");
const wasmFile = join(wasmDirectory, "alpha2_wasm_bg.wasm");
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE_WASM=1 fails instead.
const skip =
  !existsSync(wasmFile) &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

const fontBytes = () =>
  readFileSync(join(root, "tests", "fonts", "Alpha2Fixture.ttf"));

let wasm = null;
async function engineWasm() {
  if (wasm) return wasm;
  wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({ module: readFileSync(wasmFile) });
  return wasm;
}

const ok = (result) => assert.ok(result.ok, result.error?.message);
const constant = (value) => ({ mode: "constant", value });

/** A padded panel holding a title, as a project's layout generator holds it. */
const panel = {
  type: "layout",
  root: {
    justify_content: "center",
    align_items: "center",
    children: [
      {
        padding: constant("4px 8px"),
        background: constant({
          type: "solid",
          color: { red: 0.1, green: 0.1, blue: 0.2, alpha: 1 },
        }),
        children: [{ text: constant("Title"), font_size: constant("18px") }],
      },
    ],
  },
};

/** The layout as a generated source shown for a second on its own track. */
function showLayout(editor, id = "panel") {
  const trackId = crypto.randomUUID();
  ok(
    editor.dispatch({
      type: "transaction",
      commands: [
        {
          type: "asset.put",
          asset: { id, kind: "generated", name: "Panel", contentRevision: 0 },
          source: { id, kind: "generated", generator: panel },
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
}

test(
  "a layout measures with preview's fonts at the draft's canvas size",
  { skip },
  async () => {
    const fixture = {
      id: "fixture",
      name: "Alpha2 Fixture",
      bytes: new Uint8Array(fontBytes()),
      faceIndex: 0,
    };
    const editor = createEditor({
      document: createEmptyDocument({ width: 320, height: 180 }),
    });
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      runtimeAssets: { wasmUrl: readFileSync(wasmFile), fonts: [fixture] },
    });
    try {
      const bounds = await bridge.measureLayout(panel);
      assert.deepEqual(
        bounds.nodes.map((node) => node.at),
        ["root", "root.children[0]", "root.children[0].children[0]"],
      );
      const [picture, box, title] = bounds.nodes;
      assert.deepEqual(picture.border, {
        left: 0,
        top: 0,
        right: 1,
        bottom: 1,
      });
      // The panel is centred and wraps its title with its padding.
      assert.ok(
        Math.abs((box.border.left + box.border.right) / 2 - 0.5) < 0.01,
      );
      assert.ok(box.border.left < title.border.left);
      assert.equal(title.lineCount, 1);
      assert.ok(bounds.nodes.every((node) => !node.overflow));

      // The engine's own measurer with the same font, at the document's canvas size.
      const measurer = new (await engineWasm()).WasmTextMeasurer();
      measurer.addFont(fixture.name, fixture.bytes, 0);
      assert.deepEqual(
        bounds,
        JSON.parse(measurer.measureLayout(JSON.stringify(panel), 320, 180, 0)),
      );
      measurer.free();

      await assert.rejects(
        bridge.measureLayout({
          type: "fill",
          paint: {
            type: "solid",
            color: { red: 1, green: 1, blue: 1, alpha: 1 },
          },
        }),
        /only a layout generator/,
      );
      await assert.rejects(
        bridge.measureLayout({ type: "layout", root: { width: 0.5 } }),
        (error) => error.code === "INVALID_INPUT",
      );
    } finally {
      await bridge.dispose();
      editor.dispose();
    }
    await assert.rejects(
      bridge.measureLayout(panel),
      (error) => error.code === "disposed",
    );
  },
);

test(
  "a layout document previews with the document's fonts, and a native export uploads them",
  { skip },
  async () => {
    const editor = createEditor({
      document: createEmptyDocument({ width: 64, height: 36 }),
    });
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
    });
    try {
      const font = new File([fontBytes()], "Alpha2Fixture.ttf");
      const {
        assetIds: [fontId],
      } = await bridge.importFiles([font]);
      showLayout(editor);

      await bridge.bindPreview(new FakeCanvas(64, 36));
      assert.equal(await bridge.seek(0), true);
      const engine = globalThis.stubEngine.engines.at(-1);
      // The renderer draws the layout from the project, with the document's font.
      const source = engine.project.sources.find((s) => s.id === "panel");
      assert.deepEqual(source.generator, panel);
      assert.ok(engine.renders.length >= 1);
      assert.ok(
        engine.fonts.some((f) => f === bridge.getAsset(fontId)),
        "preview registers the document's font",
      );

      // Conform, the shown-text check and the native upload pass a layout, with its font.
      assert.equal((await bridge.conform("export_original")).ready, true);
      const native = await bridge.nativeAssets(editor.document);
      assert.deepEqual(
        native.map((entry) => [entry.id, entry.kind]),
        [[fontId, "caption-fonts"]],
      );
      assert.equal(native[0].blob.size, font.size);
    } finally {
      await bridge.dispose();
      editor.dispose();
    }
  },
);
