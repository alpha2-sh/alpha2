// An HDR video this browser decodes only to frames the engine cannot read: imported for a preview
// through the browser's own tone mapping, flagged while it shows, and never exported here.
import { FakeCanvas } from "./support/preview-stubs.mjs";
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

// Loaded after the stubs' module hook is registered, which a static import would precede.
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

async function engineWasm() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const ok = (result) => assert.ok(result.ok, result.error?.message);
const latestEngine = () => globalThis.stubEngine.engines.at(-1);

test(
  "degraded hdr preview is flagged and export is refused",
  { skip },
  async () => {
    const editor = createEditor({
      document: createEmptyDocument({ width: 64, height: 36 }),
    });
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      assetDecisions: await engineWasm(),
      onError: () => {},
      mediaAdapter: {
        // What the default adapter stages for such a file under `preview-degraded`.
        async stage(file) {
          return {
            runtime: {
              id: file.name,
              kind: "video",
              name: file.name,
              file,
              width: 64,
              height: 36,
              planarDecode: "unavailable",
            },
            description: {},
            fingerprint: file.fingerprint,
            dispose() {},
          };
        },
      },
    });
    await bridge.importFiles([
      {
        name: "phone.mov",
        fingerprint: { content_sha256: "a".repeat(64), byte_length: 10 },
      },
    ]);
    const trackId = crypto.randomUUID();
    ok(
      editor.dispatch({
        type: "transaction",
        commands: [
          {
            type: "track.add",
            track: { id: trackId, kind: "video", name: "V", clips: [] },
          },
          {
            type: "clip.insert",
            trackId,
            clip: {
              id: crypto.randomUUID(),
              source_id: "phone.mov",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
              time_map: { type: "forward" },
            },
          },
        ],
      }),
    );
    await bridge.bindPreview(new FakeCanvas(64, 36));
    // The renderer's frame supply presents it degraded and says so.
    globalThis.stubEngine.degraded = () => ["phone.mov"];
    try {
      assert.equal(await bridge.seek(0), true);
      assert.match(bridge.previewIssue, /browser's tone mapping/);
      assert.match(bridge.previewIssue, /native host/);
      assert.equal(
        latestEngine().options?.opaqueEngineFrames,
        "present-degraded",
      );
      globalThis.stubEngine.degraded = () => [];
      await bridge.seek(12000);
      assert.equal(bridge.previewIssue, null, "cleared once it is off screen");
    } finally {
      globalThis.stubEngine.degraded = null;
    }
    await bridge.conform("export_original");
    assert.throws(
      () => bridge.startExport({ output: { kind: "memory" } }),
      (error) => {
        assert.equal(error.code, "unsupported_media");
        assert.equal(error.operation, "export");
        assert.match(error.message, /phone\.mov/);
        assert.match(error.message, /native host/);
        assert.equal(error.cause.code, "PLANAR_DECODE_UNAVAILABLE");
        return true;
      },
    );
    await bridge.dispose();
    editor.dispose();
  },
);

test("the bridge refuses an hdrWithoutPlanes it does not know", async () => {
  const editor = createEditor();
  assert.throws(
    () =>
      createBrowserBridge({ editor, audio: false, hdrWithoutPlanes: "export" }),
    RangeError,
  );
  editor.dispose();
});
