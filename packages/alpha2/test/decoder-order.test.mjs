// A browser audio decoder may deliver a multichannel stream's planes in another order than FFmpeg's
// (Safari's AAC keeps the bitstream's C L R for 3.0). The mediabunny adapter measures the order
// once per kind of stream with a built-in tone-per-plane file and reorders every block to it.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { AudioSample, CustomAudioDecoder, registerDecoder } from "mediabunny";

import { MediabunnyAudioDecoder } from "../dist/mediabunny.js";
import { measurePlaneOrder, planeOrderOfTones } from "../dist/decoder-order.js";

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

const RATE = 48_000;
/** Every built-in fixture, by key. */
const FIXTURES = {};
for (const codec of ["aac", "opus", "vorbis", "ac3", "eac3"])
  Object.assign(
    FIXTURES,
    (await import(`../dist/generated/${codec}-order-fixtures.js`))
      .ORDER_FIXTURES,
  );

const sine = (frequency, frames, start = 0) =>
  Float32Array.from({ length: frames }, (_, index) =>
    Math.fround(
      0.25 * Math.sin((2 * Math.PI * frequency * (start + index)) / RATE),
    ),
  );

function power(samples, frequency) {
  const coefficient = 2 * Math.cos((2 * Math.PI * frequency) / RATE);
  let previous = 0;
  let before = 0;
  for (const sample of samples) {
    const current = sample + coefficient * previous - before;
    before = previous;
    previous = current;
  }
  return (
    previous * previous + before * before - coefficient * previous * before
  );
}

/** The tone of `tones` loudest in 1024 samples of `plane` from `start`. */
function loudestTone(plane, tones, start) {
  const samples = plane.subarray(start, start + 1_024);
  const powers = tones.map((tone) => power(samples, tone));
  return tones[powers.indexOf(Math.max(...powers))];
}

/**
 * Node has no WebCodecs: a decoder modelled on one that delivers another order than `FFmpeg`'s.
 * Every packet of any stream decodes into the tones of {@link ModelDecoder.stream} (a fixture's,
 * in `FFmpeg`'s order), continuous across packets, plane `p` holding `FFmpeg`'s plane
 * `planes[p]`; so a fixture decodes as it would there, and so does a source of the same kind
 * (whose own tones it does not read).
 */
class ModelDecoder extends CustomAudioDecoder {
  /** @type {{ tones: number[], planes: number[] }} */
  static stream;
  static supports(codec) {
    return ["aac", "opus", "vorbis", "ac3", "eac3"].includes(codec);
  }
  init() {}
  decode(packet) {
    const { tones, planes } = ModelDecoder.stream;
    const start = Math.round(packet.timestamp * RATE);
    const frames = Math.round(packet.duration * RATE);
    const data = new Float32Array(planes.length * frames);
    planes.forEach((plane, index) =>
      data.set(sine(tones[plane], frames, start), index * frames),
    );
    this.onSample(
      new AudioSample({
        data,
        format: "f32-planar",
        numberOfChannels: planes.length,
        sampleRate: RATE,
        timestamp: packet.timestamp,
      }),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(ModelDecoder);

/**
 * Opens `bytes` with the model decoder delivering `planes` of `fixture`'s tones and checks every
 * block plane holds the tone of the speaker it is listed as, read from `at`.
 */
async function assertSpeakersOrder(bytes, fixture, planes, at) {
  ModelDecoder.stream = { tones: fixture.tones, planes };
  const decoder = await MediabunnyAudioDecoder.open("audio", bytes, binding);
  try {
    assert.deepEqual(decoder.speakers, fixture.speakers);
    const block = await decoder.refill(
      {
        sourceId: "audio",
        startMediaTimeTicks: 0n,
        endMediaTimeTicks: 48_000n,
        generation: 1,
      },
      RATE,
    );
    assert.deepEqual(block.speakers, fixture.speakers);
    assert.deepEqual(
      block.channels.map((plane) => loudestTone(plane, fixture.tones, at)),
      fixture.tones,
    );
  } finally {
    decoder.close();
  }
}

/**
 * Corpus sources of each codec whose decoders could deliver another order, with the fixture that
 * measures them and that order as `FFmpeg`'s planes: the bitstream's for AAC 3.0 (C L R) and 5.1
 * (C L R Ls Rs LFE) and AC-3 5.1 (L C R Ls Rs LFE), and Vorbis's for Opus and Vorbis 5.1 (L C R
 * Ls Rs LFE).
 */
const CORPUS = [
  ["audio-aac-3.0.mp4", "aac:3", [2, 0, 1]],
  ["audio-aac-5.1.mp4", "aac:6", [2, 0, 1, 4, 5, 3]],
  ["audio-opus-5.1.webm", "opus:1:6", [0, 2, 1, 4, 5, 3]],
  ["audio-vorbis-5.1.ogg", "vorbis:6", [0, 2, 1, 4, 5, 3]],
  ["audio-ac3-5.1.mp4", "ac3:FL FR FC LFE SL SR", [0, 2, 1, 4, 5, 3]],
];
for (const [name, key, planes] of CORPUS)
  test(
    `${name}: a decoder that delivers another order has its planes put in the speakers' order`,
    { skip: needsWasm },
    async () => {
      const bytes = new Uint8Array(
        await readFile(
          new URL(`../../../tests/corpus/${name}`, import.meta.url),
        ),
      );
      await assertSpeakersOrder(bytes, FIXTURES[key], planes, 4_800);
    },
  );

test(
  "every fixture's stream is measured and put in the speakers' order",
  { skip: needsWasm },
  async (t) => {
    for (const [key, fixture] of Object.entries(FIXTURES))
      await t.test(key, async () => {
        // A page measures each kind of stream once: a kind a corpus source above measured keeps
        // the order measured there; in any other, each decoded plane holds the next of FFmpeg's.
        const planes =
          CORPUS.find(([, corpusKey]) => corpusKey === key)?.[2] ??
          fixture.tones.map((_, plane) => (plane + 1) % fixture.tones.length);
        await assertSpeakersOrder(
          Uint8Array.from(atob(fixture.file), (character) =>
            character.charCodeAt(0),
          ),
          fixture,
          planes,
          fixture.start,
        );
      });
  },
);

test("each plane's tone names the plane of FFmpeg's order it holds", () => {
  const tones = [375, 750, 1125, 93.75];
  const planes = (order) => order.map((plane) => sine(tones[plane], 2_048));
  assert.deepEqual(
    planeOrderOfTones(planes([0, 1, 2, 3]), tones, RATE, 1_024),
    [0, 1, 2, 3],
  );
  assert.deepEqual(
    planeOrderOfTones(planes([2, 0, 1, 3]), tones, RATE, 1_024),
    [2, 0, 1, 3],
  );
  assert.deepEqual(
    planeOrderOfTones(planes([3, 2, 1, 0]), tones, RATE, 0),
    [3, 2, 1, 0],
  );
  // Two planes of one tone, a plane holding two tones alike, too few samples, a plane missing:
  // nothing is measured.
  assert.equal(
    planeOrderOfTones(planes([0, 0, 1, 3]), tones, RATE, 1_024),
    null,
  );
  const mixed = planes([0, 1, 2, 3]);
  mixed[0] = mixed[0].map((sample, index) => sample + mixed[1][index]);
  assert.equal(planeOrderOfTones(mixed, tones, RATE, 1_024), null);
  assert.equal(
    planeOrderOfTones(
      tones.map((tone) => sine(tone, 2_000)),
      tones,
      RATE,
      1_024,
    ),
    null,
  );
  assert.equal(
    planeOrderOfTones(planes([0, 1, 2, 3]), tones, RATE, 1_025),
    null,
  );
  assert.equal(planeOrderOfTones(planes([0, 1, 2]), tones, RATE, 1_024), null);
});

test("a decoder that cannot decode the fixture leaves the order unmeasured", async () => {
  const fixture = FIXTURES["aac:3"];
  assert.equal(
    await measurePlaneOrder(fixture, async () => {
      throw new Error("unsupported");
    }),
    null,
  );
  assert.equal(
    await measurePlaneOrder(fixture, async () => ({
      planes: [sine(375, 2_048), sine(375, 2_048), sine(375, 2_048)],
      sampleRate: RATE,
    })),
    null,
  );
});
