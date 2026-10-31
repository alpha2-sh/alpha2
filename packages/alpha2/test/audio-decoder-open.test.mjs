// A browser audio decoder decodes its source's first packet when it opens: a source the browser's
// decoder cannot decode is refused there, with its facts, instead of at its first refill; a decoder
// that adds a silent LFE to a 5.0 source (as Edge's E-AC-3 decoder does) has that layout adopted,
// and any other channel count is refused.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import { AudioSample, CustomAudioDecoder, registerDecoder } from "mediabunny";

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

/** What the stand-in E-AC-3 decoder does with every packet. */
let mode = "fail";

/**
 * Node has no WebCodecs: an E-AC-3 decoder that fails (`fail`), or delivers six planes for the
 * 5.0 source with a silent plane where 5.1 has its LFE (`silent-lfe`), the same with sound in
 * that plane (`loud-lfe`), or seven planes (`seven`). Each plane holds its own level.
 */
class StandInEac3Decoder extends CustomAudioDecoder {
  static supports(codec) {
    return codec === "eac3";
  }
  init() {}
  decode(packet) {
    if (mode === "fail") {
      // As a WebCodecs decoder does: one error through its callback, then nothing.
      if (!this.failed)
        this.onError(new DOMException("Decoding error.", "EncodingError"));
      this.failed = true;
      return;
    }
    const channels = mode === "seven" ? 7 : 6;
    const frames = 1_536;
    const data = new Float32Array(channels * frames);
    for (let plane = 0; plane < channels; plane += 1)
      data.fill(
        mode === "silent-lfe" && plane === 3 ? 0 : 0.1 * (plane + 1),
        plane * frames,
        (plane + 1) * frames,
      );
    this.onSample(
      new AudioSample({
        data,
        format: "f32-planar",
        numberOfChannels: channels,
        sampleRate: 48_000,
        timestamp: packet.timestamp,
      }),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(StandInEac3Decoder);

const corpus = (name) =>
  readFile(new URL(`../../../tests/corpus/${name}`, import.meta.url)).then(
    (bytes) => new Uint8Array(bytes),
  );

const refusedWith = (facts) => (error) => {
  assert.equal(error.name, "DecodeUnavailableError");
  assert.equal(error.code, "DECODE_UNAVAILABLE");
  assert.deepEqual(error.failure, {
    code: "INVALID_INPUT",
    phase: "decode",
    stage: "decode-unavailable",
  });
  for (const [key, value] of Object.entries(facts))
    assert.deepEqual(error.facts[key], value, key);
  assert.match(error.message, /^audio: this browser cannot decode/);
  return true;
};

test("a track that cannot decode is refused at open with facts", async () => {
  // A decoder exists, but fails on the first packet.
  mode = "fail";
  await assert.rejects(
    MediabunnyAudioDecoder.open(
      "audio",
      await corpus("audio-eac3-5.0.mp4"),
      {},
    ),
    refusedWith({
      kind: "audio",
      channels: 5,
      layout: ["FL", "FR", "FC", "SL", "SR"],
    }),
  );
  // No decoder at all: Node has no WebCodecs AAC decoder.
  await assert.rejects(
    MediabunnyAudioDecoder.open("audio", await corpus("audio-aac-3.0.mp4"), {}),
    refusedWith({ kind: "audio", channels: 3, layout: ["FL", "FR", "FC"] }),
  );
});

test(
  "a declared 5.0 decoded as 5.1 with a silent LFE is adopted",
  { skip: needsWasm },
  async () => {
    mode = "silent-lfe";
    const decoder = await MediabunnyAudioDecoder.open(
      "audio",
      await corpus("audio-eac3-5.0.mp4"),
      binding,
    );
    try {
      assert.deepEqual(decoder.speakers, ["FL", "FR", "FC", "LFE", "SL", "SR"]);
      const block = await decoder.refill(
        {
          sourceId: "audio",
          startMediaTimeTicks: 12_000n,
          endMediaTimeTicks: 24_000n,
          generation: 1,
        },
        48_000,
      );
      assert.deepEqual(block.speakers, decoder.speakers);
      assert.equal(block.channels.length, 6);
      // Each plane keeps its level, the LFE silent.
      const levels = block.channels.map((plane) =>
        Number(plane[plane.length >> 1].toFixed(4)),
      );
      assert.deepEqual(levels, [0.1, 0.2, 0.3, 0, 0.5, 0.6]);
    } finally {
      decoder.close();
    }
  },
);

test("another channel count change is refused at open", async () => {
  for (const changed of ["loud-lfe", "seven"]) {
    mode = changed;
    await assert.rejects(
      MediabunnyAudioDecoder.open(
        "audio",
        await corpus("audio-eac3-5.0.mp4"),
        {},
      ),
      (error) =>
        refusedWith({ channels: 5 })(error) &&
        /delivers (6|7) channels for a source that declares 5/.test(
          error.facts.browserError,
        ),
      changed,
    );
  }
});
