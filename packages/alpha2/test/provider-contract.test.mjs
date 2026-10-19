import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { MediabunnyAudioDecoder } from "../dist/mediabunny.js";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
// The adapter resamples with the packaged binding; ALPHA2_REQUIRE_WASM=1 loads it even when
// missing, so the run fails instead of skipping.
const binding =
  process.env.ALPHA2_REQUIRE_WASM === "1" ||
  existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"))
    ? await import(pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href)
    : null;
binding?.initSync({
  module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
});

// The audio provider contract every provider passes; the Rust providers are held to the same
// fixture by `tests/provider_contract`.
const tests = new URL("../../../tests/", import.meta.url);
const contract = JSON.parse(
  await readFile(new URL("cases/audio-provider-contract.json", tests), "utf8"),
);

function expected(channel, sample) {
  const { firstSample, endSample, levels, scale } = contract.stream;
  if (sample < firstSample || sample >= endSample) return 0;
  return Math.fround(levels[channel] / scale);
}

function ticks(sample) {
  const scaled = BigInt(sample) * 120_000n;
  const rate = BigInt(contract.projectSampleRate);
  assert.equal(scaled % rate, 0n, "requests start on whole ticks");
  return scaled / rate;
}

test(
  "the mediabunny decoder meets the audio provider contract",
  { skip: binding === null && "the wasm package is not built" },
  async () => {
    const decoder = await MediabunnyAudioDecoder.open(
      "surround",
      new Uint8Array(await readFile(new URL(contract.source, tests))),
      binding,
    );
    try {
      for (const request of contract.requests) {
        const start = request.startSample;
        const end = start + request.frames;
        // A browser block holds its samples outright, silence included, so it must cover the request.
        const block = await decoder.refill(
          {
            sourceId: "surround",
            startMediaTimeTicks: ticks(start),
            endMediaTimeTicks: ticks(end),
            generation: 1,
          },
          contract.projectSampleRate,
        );
        const first = Number(block.startSample);
        assert.equal(
          block.sampleRate,
          contract.projectSampleRate,
          request.name,
        );
        assert.equal(
          block.channels.length,
          contract.stream.levels.length,
          request.name,
        );
        assert.deepEqual(
          block.speakers,
          contract.stream.speakers,
          request.name,
        );
        assert.ok(
          first <= start && first + block.channels[0].length >= end,
          `${request.name}: the block does not cover the request`,
        );
        block.channels.forEach((plane, channel) => {
          for (let sample = start; sample < end; sample += 1) {
            assert.equal(
              plane[sample - first],
              expected(channel, sample),
              `${request.name}: channel ${channel} at sample ${sample}`,
            );
          }
        });
      }
    } finally {
      decoder.close();
    }
  },
);

test(
  "the mediabunny decoder reports the speakers a source declares, its planes as the file lays them out",
  { skip: binding === null && "the wasm package is not built" },
  async (t) => {
    const frames = contract.layoutFrames;
    for (const layout of contract.layouts)
      await t.test(layout.source, async () => {
        const decoder = await MediabunnyAudioDecoder.open(
          "layout",
          new Uint8Array(await readFile(new URL(layout.source, tests))),
          binding,
        );
        try {
          assert.deepEqual(decoder.speakers, layout.speakers, layout.source);
          const block = await decoder.refill(
            {
              sourceId: "layout",
              startMediaTimeTicks: 0n,
              endMediaTimeTicks: ticks(frames),
              generation: 1,
            },
            contract.projectSampleRate,
          );
          assert.equal(block.startSample, 0n, layout.source);
          assert.deepEqual(block.speakers, layout.speakers, layout.source);
          assert.equal(block.channels.length, layout.levels.length);
          block.channels.forEach((plane, index) => {
            const level = Math.fround(
              layout.levels[index] / contract.stream.scale,
            );
            const wrong = plane
              .subarray(0, frames)
              .findIndex((sample) => sample !== level);
            assert.equal(
              wrong,
              -1,
              `${layout.source}: plane ${index} at sample ${wrong} is ${plane[wrong]}, not ${level}`,
            );
          });
        } finally {
          decoder.close();
        }
      });
  },
);

test(
  "the mediabunny decoder reads the speakers a codec's header declares",
  { skip: binding === null && "the wasm package is not built" },
  async (t) => {
    // Decoding these needs WebCodecs; the browser harness (`audio-speaker-layout`) checks them.
    for (const layout of contract.codecLayouts)
      await t.test(layout.source, async () => {
        const decoder = await MediabunnyAudioDecoder.open(
          "codec",
          new Uint8Array(await readFile(new URL(layout.source, tests))),
          binding,
        );
        try {
          assert.deepEqual(decoder.speakers, layout.speakers, layout.source);
        } finally {
          decoder.close();
        }
      });
  },
);
