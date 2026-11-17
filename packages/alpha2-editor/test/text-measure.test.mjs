import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor } from "../dist/index.js";
import { wasmDirectory, wasmSkip } from "./support/wasm.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmFile = join(wasmDirectory, "alpha2_wasm_bg.wasm");
const skip = wasmSkip(import.meta.url);
const { createBrowserBridge } = skip
  ? {}
  : await import("../dist/browser/index.js");

const fixture = {
  id: "fixture",
  name: "Alpha2 Fixture",
  bytes: new Uint8Array(
    skip ? 0 : readFileSync(join(root, "tests", "fonts", "Alpha2Fixture.ttf")),
  ),
  faceIndex: 0,
};

test(
  "a text generator measures with preview's fonts at the draft's canvas size",
  { skip },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      runtimeAssets: { wasmUrl: readFileSync(wasmFile), fonts: [fixture] },
    });
    try {
      const title = {
        type: "text",
        text: "Title",
        style: { font_size: 0.1 },
      };
      const bounds = await bridge.measureText(title);
      // The default style hangs the box's bottom centre at (0.5, 0.92).
      const { layout, content, ink } = bounds;
      assert.ok(Math.abs((layout.left + layout.right) / 2 - 0.5) < 1e-6);
      assert.ok(Math.abs(layout.bottom - 0.92) < 1e-6);
      assert.equal(bounds.lineCount, 1);
      assert.ok(
        content.right - content.left < (layout.right - layout.left) / 2,
      );
      assert.ok(
        ink.left >= content.left - 0.01 && ink.right <= content.right + 0.01,
      );
      assert.ok(ink.top >= layout.top && ink.bottom <= layout.bottom);

      const wasm = await import(
        pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
      );
      const measurer = new wasm.WasmTextMeasurer();
      measurer.addFont(fixture.name, fixture.bytes, 0);
      const { width, height } = editor.document.project.settings;
      assert.deepEqual(
        bounds,
        JSON.parse(measurer.measure(JSON.stringify(title), width, height)),
      );
      measurer.free();

      const blank = await bridge.measureText({ type: "text", text: "  " });
      assert.equal(blank.ink, null);
      await assert.rejects(
        bridge.measureText({ type: "text", text: "", style: {} }),
        /text generator/,
      );
      await assert.rejects(
        bridge.measureText({ type: "fill", paint: { type: "solid" } }),
        /text generator/,
      );
    } finally {
      await bridge.dispose();
      editor.dispose();
    }
    await assert.rejects(
      bridge.measureText({ type: "text", text: "Title" }),
      (error) => error.code === "disposed",
    );
  },
);
