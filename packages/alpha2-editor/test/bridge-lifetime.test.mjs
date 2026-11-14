import "./support/preview-stubs.mjs";
import { register } from "node:module";
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createEditor, createEmptyDocument } from "../dist/index.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

register("./support/gated-wasm-hook.mjs", import.meta.url);
// Loaded after the module hooks are registered, which a static import would precede.
const { createBrowserBridge } = await import("../dist/browser/index.js");

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

const settle = (ms = 0) => new Promise((r) => setTimeout(r, ms));
const hash = (digit) => digit.repeat(64);
const fingerprint = (digit) => ({
  content_sha256: hash(digit),
  byte_length: 10,
});

/** Makes the next engine-wasm load of a bridge without `assetDecisions` wait for `open()`. */
async function gateWasm() {
  const binding = await engineWasm();
  let open, requested;
  const asked = new Promise((r) => (requested = r));
  globalThis.gatedWasm = {
    requested,
    gate: new Promise((r) => (open = r)),
    binding,
  };
  return {
    asked,
    open() {
      open();
      delete globalThis.gatedWasm;
    },
  };
}

const videoAdapter = {
  async stage(file) {
    return {
      runtime: { id: file.name, kind: "video", file, width: 64, height: 36 },
      description: {},
      fingerprint: file.fingerprint,
      dispose() {},
    };
  },
};

test(
  "a relink whose engine load ends after another project opened changes nothing",
  { skip },
  async () => {
    const editor = createEditor();
    // No `assetDecisions`: the bridge loads the engine wasm on first use.
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      mediaAdapter: videoAdapter,
    });
    await bridge.importFiles([{ name: "clip", fingerprint: fingerprint("a") }]);
    const gate = await gateWasm();
    const relinking = bridge.relink("clip", {
      name: "clip",
      fingerprint: fingerprint("a"),
    });
    relinking.catch(() => {});
    await gate.asked;
    editor.open(createEmptyDocument({ width: 64, height: 36 }));
    const opened = editor.document;
    gate.open();
    await assert.rejects(relinking, (error) => error.code === "stale_import");
    assert.equal(editor.document, opened);
    assert.deepEqual(Object.keys(editor.document.assets), []);
    assert.equal(editor.getSnapshot().canUndo, false);
    await settle();
    assert.equal(bridge.resourceCount, 0);
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a caption import whose engine load ends after another project opened changes nothing",
  { skip },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({ editor, audio: false });
    const gate = await gateWasm();
    const importing = bridge.importCaptions(
      new File(["1\n00:00:00,000 --> 00:00:01,000\nHello\n"], "words.srt"),
    );
    importing.catch(() => {});
    await gate.asked;
    editor.open(createEmptyDocument({ width: 64, height: 36 }));
    const opened = editor.document;
    gate.open();
    await assert.rejects(
      importing,
      (error) => error.name === "EditorError" && error.code === "stale_import",
    );
    assert.equal(editor.document, opened);
    await bridge.dispose();
    await assert.rejects(
      bridge.importCaptions(new File([""], "late.srt")),
      (error) => error.code === "disposed",
    );
    editor.dispose();
  },
);
