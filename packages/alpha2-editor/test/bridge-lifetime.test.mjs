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

/**
 * A bridge whose proxy store holds each conversion until the test finishes it or its signal
 * aborts, and counts proxy stagings, holds and releases.
 */
async function proxySession() {
  const editor = createEditor();
  const seen = { conversions: [], proxyStages: 0, holds: 0, released: 0 };
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions: await engineWasm(),
    mediaAdapter: {
      async stage(file, signal, options) {
        if (options?.fingerprint === false) seen.proxyStages += 1;
        return videoAdapter.stage(file);
      },
    },
    proxyStore: {
      async hold() {
        seen.holds += 1;
        return {
          release: async () => {
            seen.released += 1;
          },
        };
      },
      find: async () => null,
      generate(file, madeFrom, { signal }) {
        return new Promise((resolve, reject) => {
          const conversion = {
            signal,
            finish: () =>
              resolve({
                record: { made_from: madeFrom, width: 32, height: 18 },
                file: { name: `${file.name}.proxy`, fingerprint: {} },
              }),
          };
          seen.conversions.push(conversion);
          signal.addEventListener("abort", () => reject(signal.reason), {
            once: true,
          });
        });
      },
    },
  });
  await bridge.importFiles([{ name: "clip", fingerprint: fingerprint("a") }]);
  return { editor, bridge, seen };
}

const started = async (seen, count = 1) => {
  for (let i = 0; i < 200 && seen.conversions.length < count; i++)
    await settle(2);
  assert.equal(seen.conversions.length, count, "the conversion started");
  return seen.conversions.at(-1);
};

test(
  "disposing the bridge stops a running proxy conversion and refuses new ones",
  { skip },
  async () => {
    const { editor, bridge, seen } = await proxySession();
    const making = bridge.generateProxy("clip");
    making.catch(() => {});
    const conversion = await started(seen);
    await bridge.dispose();
    assert.equal(conversion.signal.aborted, true);
    await assert.rejects(making);
    assert.equal(seen.proxyStages, 0);
    assert.equal(seen.released, seen.holds);
    await assert.rejects(
      bridge.generateProxy("clip"),
      (error) => error.code === "disposed",
    );
    editor.dispose();
  },
);

test(
  "a proxy conversion outlives a project switch and is adopted only for a document with its media",
  { skip },
  async () => {
    const { editor, bridge, seen } = await proxySession();
    const first = bridge.generateProxy("clip");
    const conversion = await started(seen);
    editor.open(createEmptyDocument({ width: 64, height: 36 }));
    await settle(20);
    assert.equal(conversion.signal.aborted, false);
    conversion.finish();
    await first;
    assert.equal(seen.proxyStages, 0, "not adopted for a project without it");
    assert.equal(seen.released, seen.holds);

    editor.open(createEmptyDocument({ width: 64, height: 36 }));
    await bridge.importFiles([{ name: "clip", fingerprint: fingerprint("a") }]);
    const second = bridge.generateProxy("clip");
    const next = await started(seen, 2);
    const document = editor.document;
    editor.open(createEmptyDocument({ width: 64, height: 36 }));
    assert.ok(
      editor.dispatch({
        type: "asset.put",
        asset: { ...document.assets.clip, missing: true },
        source: document.project.sources.find((s) => s.id === "clip"),
      }).ok,
    );
    next.finish();
    await second;
    assert.equal(seen.proxyStages, 1, "adopted for a document with its media");
    assert.equal(bridge.proxyStatus("clip").available, true);
    await bridge.dispose();
    editor.dispose();
  },
);
