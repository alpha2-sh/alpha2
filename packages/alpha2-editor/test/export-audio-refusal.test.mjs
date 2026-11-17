// Node has no WebCodecs `AudioDecoder`; a registered mediabunny decoder stands in where a source's
// audio must decode.
import "./support/preview-stubs.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { AudioSample, CustomAudioDecoder, registerDecoder } from "mediabunny";
import { createEditor, createEmptyDocument } from "../dist/index.js";
import { wasmDirectory, wasmSkip } from "./support/wasm.mjs";

const skip = wasmSkip(import.meta.url);
const { createBrowserBridge } = skip
  ? {}
  : await import("../dist/browser/index.js");

async function engineWasm() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const decoding = { aac: false };

class SilentAacDecoder extends CustomAudioDecoder {
  static supports(codec) {
    return decoding.aac && codec === "aac";
  }
  init() {}
  decode(packet) {
    this.onSample(
      new AudioSample({
        data: new Float32Array(1_024),
        format: "f32-planar",
        numberOfChannels: 1,
        sampleRate: 48_000,
        timestamp: packet.timestamp,
      }),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(SilentAacDecoder);

const ok = (result) => assert.ok(result.ok, result.error?.message);
const voice = async () =>
  new File(
    [
      await readFile(
        new URL(
          "../../../tests/corpus/audio-only-aac-priming.m4a",
          import.meta.url,
        ),
      ),
    ],
    "voice.m4a",
  );

/** A bridge whose adapter imports `voice.m4a` as audio the import found undecodable. */
async function session() {
  const editor = createEditor({
    document: createEmptyDocument({ width: 64, height: 36 }),
  });
  const bridge = createBrowserBridge({
    editor,
    audio: false,
    assetDecisions: await engineWasm(),
    mediaAdapter: {
      async stage(file) {
        return {
          runtime: {
            id: file.name,
            kind: "audio",
            name: file.name,
            file,
            duration: 1,
            hasAudio: true,
            audioDecodable: false,
            audioDecodeFacts: { kind: "audio", codec: "mp4a.40.2" },
          },
          description: {},
          dispose() {},
        };
      },
    },
  });
  await bridge.importFiles([await voice()]);
  ok(
    editor.dispatch({
      type: "track.add",
      track: {
        id: "a",
        name: "A",
        kind: "audio",
        clips: [
          {
            id: "c",
            source_id: "voice.m4a",
            timeline_range: { start: 0, end: 60000 },
            source_range: { start: 0, end: 60000 },
          },
        ],
      },
    }),
  );
  return { editor, bridge };
}

test(
  "an export whose audible audio this browser cannot decode is refused, naming it",
  { skip },
  async () => {
    decoding.aac = false;
    const { editor, bridge } = await session();
    await bridge.conform("export_original");
    assert.throws(
      () => bridge.startExport({ output: { kind: "memory" } }),
      (error) => {
        assert.equal(error.code, "unsupported_media");
        assert.equal(error.operation, "export");
        assert.match(error.message, /voice\.m4a/);
        assert.equal(error.cause.code, "DECODE_UNAVAILABLE");
        assert.deepEqual(
          error.details.sources.map((s) => s.id),
          ["voice.m4a"],
        );
        return true;
      },
    );
    assert.equal(bridge.resourceCount, 1, "nothing stays leased");
    assert.throws(
      () => bridge.startExport({ undecodableAudio: "drop" }),
      (error) => error.code === "invalid_undecodable_audio",
    );
    for (const options of [
      { undecodableAudio: "silence" },
      { includeAudio: false },
    ])
      await bridge
        .startExport({ output: { kind: "memory" }, ...options })
        .cancel();
    await bridge.dispose();
    editor.dispose();
  },
);

test(
  "a source whose audio decodes when the export conforms is not refused",
  { skip },
  async () => {
    decoding.aac = false;
    const { editor, bridge } = await session();
    decoding.aac = true;
    try {
      await bridge.conform("export_original");
      assert.equal(bridge.getAsset("voice.m4a").audioDecodable, true);
      const job = bridge.startExport({ output: { kind: "memory" } });
      await job.cancel();
    } finally {
      decoding.aac = false;
    }
    await bridge.dispose();
    editor.dispose();
  },
);
