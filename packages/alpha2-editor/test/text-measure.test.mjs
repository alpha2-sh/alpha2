// Where the engine draws a text generator's text, asked of the bridge: laid out with the fonts and
// canvas preview draws with, as the packaged wasm engine measures it.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor } from "../dist/index.js";
import { createBrowserBridge } from "../dist/browser/index.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmDirectory = join(root, "packages", "alpha2", "wasm");
const wasmFile = join(wasmDirectory, "alpha2_wasm_bg.wasm");
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE=wasm fails instead.
const skip =
  !existsSync(wasmFile) &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

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
      // The text is far narrower than the wrapping width, and its ink lies on its lines.
      assert.ok(
        content.right - content.left < (layout.right - layout.left) / 2,
      );
      assert.ok(
        ink.left >= content.left - 0.01 && ink.right <= content.right + 0.01,
      );
      assert.ok(ink.top >= layout.top && ink.bottom <= layout.bottom);

      // The engine's own measurer with the same font, at the document's canvas size.
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
