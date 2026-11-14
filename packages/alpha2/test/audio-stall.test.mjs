// Node has no WebCodecs; a registered mediabunny decoder stands in and stops delivering on request.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import { AudioSample, CustomAudioDecoder, registerDecoder } from "mediabunny";

import { classifyBrowserFailure } from "../dist/export-failure.js";
import { MediabunnyAudioDecoder } from "../dist/mediabunny.js";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const binding = existsSync(join(wasmDirectory, "alpha2_wasm.js"))
  ? await (async () => {
      const wasm = await import(
        pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
      );
      await wasm.default({
        module_or_path: await readFile(
          join(wasmDirectory, "alpha2_wasm_bg.wasm"),
        ),
      });
      return wasm;
    })()
  : null;
const needsWasm = binding === null && "the wasm package is not built";

const decoders = { stalled: false };

/** Decodes AAC packets to silence until `decoders.stalled`, then never answers again. */
class StallingDecoder extends CustomAudioDecoder {
  static supports(codec) {
    return codec === "aac";
  }
  init() {}
  decode(packet) {
    if (decoders.stalled) return;
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
  flush() {
    return decoders.stalled ? new Promise(() => {}) : undefined;
  }
  close() {}
}
registerDecoder(StallingDecoder);

const request = {
  sourceId: "audio",
  startMediaTimeTicks: 0n,
  endMediaTimeTicks: 60_000n,
  generation: 1,
};

async function stalledDecoder(t, options = {}) {
  decoders.stalled = false;
  const bytes = new Uint8Array(
    await readFile(
      new URL("../../../tests/corpus/audio-seek-aac.mp4", import.meta.url),
    ),
  );
  const decoder = await MediabunnyAudioDecoder.open(
    "audio",
    bytes,
    binding,
    options,
  );
  decoders.stalled = true;
  t.after(() => {
    decoders.stalled = false;
    decoder.close();
  });
  return decoder;
}

test(
  "an audio refill whose decoder stops delivering rejects as a stall",
  { skip: needsWasm },
  async (t) => {
    const decoder = await stalledDecoder(t, { stallTimeoutMs: 300 });
    const started = performance.now();
    const error = await decoder.refill(request, 48_000).then(
      () => null,
      (failure) => failure,
    );
    assert.ok(performance.now() - started < 3_000);
    assert.equal(error?.code, "DECODE_STALLED");
    assert.match(error.message, /audio decoder audio stalled/);
    const failure = classifyBrowserFailure(error, { phase: "export" });
    assert.equal(failure.code, "GPU_TIMEOUT");
    assert.equal(failure.phase, "decode");
  },
);

test(
  "an audio refill whose decoder stops delivering rejects at once on abort",
  { skip: needsWasm },
  async (t) => {
    const decoder = await stalledDecoder(t);
    const controller = new AbortController();
    const reason = new Error("export cancelled");
    const refilling = decoder.refill(request, 48_000, controller.signal);
    setTimeout(() => controller.abort(reason), 50);
    const started = performance.now();
    await assert.rejects(refilling, (error) => error === reason);
    assert.ok(performance.now() - started < 1_000);
  },
);

test("an audio stall limit outside the allowed range is refused at open", async () => {
  await assert.rejects(
    MediabunnyAudioDecoder.open("audio", new Uint8Array(16), binding, {
      stallTimeoutMs: 0,
    }),
    RangeError,
  );
});
