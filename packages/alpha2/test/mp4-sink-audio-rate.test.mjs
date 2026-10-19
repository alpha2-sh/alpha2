// A project rate the browser's audio encoder does not take is converted to the rate a
// native host would choose, so every project rate exports.
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import {
  ALL_FORMATS,
  BufferSource,
  CustomAudioEncoder,
  EncodedPacket,
  Input,
  registerEncoder,
} from "mediabunny";

import { renderOfflineAudio } from "../dist/audio.js";
import {
  audioEncoderSampleRate,
  MediabunnyMp4Sink,
} from "../dist/mediabunny.js";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");

// The packaged binding, or null before `npm run build` made it. ALPHA2_REQUIRE_WASM=1 loads it
// regardless, so a missing package fails instead of skipping.
async function loadBinding() {
  if (
    process.env.ALPHA2_REQUIRE_WASM !== "1" &&
    !existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"))
  )
    return null;
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}
const binding = await loadBinding();
const needsWasm = binding === null && "the wasm package is not built";

/** What the encoder was given: each sample's rate, start and planes. */
const given = [];

/**
 * An AAC encoder that takes 44.1 and 48 kHz only, as Chrome's on macOS does, and records the
 * samples it is given. There is no Opus encoder here.
 */
class TwoRateAacEncoder extends CustomAudioEncoder {
  static supports(codec, config) {
    return codec === "aac" && [44_100, 48_000].includes(config.sampleRate);
  }
  init() {
    this.configured = false;
  }
  encode(sample) {
    const planes = Array.from(
      { length: sample.numberOfChannels },
      (_, index) => {
        const plane = new Float32Array(sample.numberOfFrames);
        sample.copyTo(plane, { planeIndex: index, format: "f32-planar" });
        return plane;
      },
    );
    given.push({
      sampleRate: sample.sampleRate,
      startSample: Math.round(sample.timestamp * sample.sampleRate),
      planes,
    });
    const meta = this.configured
      ? undefined
      : {
          decoderConfig: {
            codec: "mp4a.40.2",
            numberOfChannels: this.config.numberOfChannels,
            sampleRate: this.config.sampleRate,
            description: audioSpecificConfig(
              this.config.sampleRate,
              this.config.numberOfChannels,
            ),
          },
        };
    this.configured = true;
    this.onPacket(
      new EncodedPacket(
        new Uint8Array([1, 2, 3]),
        "key",
        sample.timestamp,
        sample.duration,
      ),
      meta,
    );
  }
  flush() {}
  close() {}
}
registerEncoder(TwoRateAacEncoder);

/** AAC-LC's two-byte `AudioSpecificConfig` for 44.1 or 48 kHz, which the muxer writes. */
function audioSpecificConfig(sampleRate, channels) {
  const frequencyIndex = sampleRate === 48_000 ? 3 : 4;
  return new Uint8Array([
    (2 << 3) | (frequencyIndex >> 1),
    ((frequencyIndex & 1) << 7) | (channels << 3),
  ]);
}

const BITRATE = 128_000;

/** A stereo tone at `rate`: left a sine, right its negation. */
function tone(rate, start, frames) {
  const left = new Float32Array(frames);
  for (let index = 0; index < frames; index += 1)
    left[index] = 0.25 * Math.sin((2 * Math.PI * 997 * (start + index)) / rate);
  return [left, left.map((value) => -value)];
}

/**
 * Exports `total` samples of the tone at `projectRate` in blocks of `sizes`; returns what the
 * encoder got.
 */
async function exportTone(projectRate, total, sizes, sinkOptions) {
  given.length = 0;
  const sink = new MediabunnyMp4Sink({
    audioCodec: "aac",
    audioBitrate: BITRATE,
    destination: { kind: "memory", maxBytes: 1 << 24 },
    ...sinkOptions,
  });
  await renderOfflineAudio(
    BigInt(total),
    sizes,
    async (startSample, frames) => ({
      sourceId: "mix",
      startSample,
      generation: 0,
      sampleRate: projectRate,
      channels: tone(projectRate, Number(startSample), frames),
    }),
    sink.audioTrack,
  );
  const input = new Input({
    source: new BufferSource(sink.output.buffer),
    formats: ALL_FORMATS,
  });
  const fileRate = (await input.getPrimaryAudioTrack()).sampleRate;
  input.dispose();
  const encoded = given.splice(0);
  const length = encoded.reduce(
    (sum, sample) => sum + sample.planes[0].length,
    0,
  );
  const planes = [0, 1].map((channel) => {
    const plane = new Float32Array(length);
    let at = 0;
    for (const sample of encoded) {
      plane.set(sample.planes[channel], at);
      at += sample.planes[channel].length;
    }
    return plane;
  });
  return { fileRate, encoded, planes };
}

test("the encoder's rate is the project's where the encoder takes it, else the next it takes, else the highest", async () => {
  for (const [projectRate, expected] of [
    [48_000, 48_000],
    [44_100, 44_100],
    [8_000, 44_100],
    [22_050, 44_100],
    [44_101, 48_000],
    [50_000, 48_000],
    [96_000, 48_000],
  ]) {
    assert.equal(
      await audioEncoderSampleRate("aac", {
        sampleRate: projectRate,
        numberOfChannels: 2,
        bitrate: BITRATE,
      }),
      expected,
      `${projectRate} Hz`,
    );
  }
  // No encoder takes the codec at any rate.
  assert.equal(
    await audioEncoderSampleRate("opus", {
      sampleRate: 48_000,
      numberOfChannels: 2,
      bitrate: BITRATE,
    }),
    null,
  );
});

test(
  "a project at a rate the encoder does not take is converted by the engine's converter",
  { skip: needsWasm },
  async () => {
    for (const [projectRate, encoderRate, total] of [
      [22_050, 44_100, 11_025],
      [50_000, 48_000, 25_000],
      [96_000, 48_000, 48_001],
      [8_000, 44_100, 4_001],
    ]) {
      const label = `${projectRate} -> ${encoderRate}`;
      const whole = tone(projectRate, 0, total);
      const resampler = new binding.AudioResampler(projectRate, encoderRate);
      const outputs = resampler.targetAtOrAfter(total);
      const expected = whole.map((plane) =>
        resampler.resample(plane, 0, 0, outputs),
      );
      resampler.free();
      // Irregular blocks, and blocks of one size: the encoder gets the same samples either way.
      for (const sizes of [[1_000, 333, 4_096], [480]]) {
        const { fileRate, encoded, planes } = await exportTone(
          projectRate,
          total,
          sizes,
          { audioSampleRate: encoderRate, audioResampler: binding },
        );
        assert.equal(fileRate, encoderRate, label);
        assert.ok(
          encoded.every((sample) => sample.sampleRate === encoderRate),
          label,
        );
        let next = 0;
        for (const sample of encoded) {
          assert.equal(sample.startSample, next, `${label}: contiguous`);
          next += sample.planes[0].length;
        }
        // It ends where the project's audio does, as a native export's AAC does.
        assert.equal(planes[0].length, outputs, `${label}: length`);
        assert.deepEqual(planes, expected, `${label} ${sizes}`);
      }
    }
  },
);

test("a project at a rate the encoder takes is encoded as it is", async () => {
  const total = 4_800;
  const { fileRate, planes } = await exportTone(48_000, total, [1_000], {
    audioSampleRate: 48_000,
  });
  assert.equal(fileRate, 48_000);
  assert.deepEqual(planes, tone(48_000, 0, total));
});

test(
  "audio to convert needs the engine's converter and blocks that follow one another",
  { skip: needsWasm },
  async () => {
    const block = (startSample, frames) => ({
      sourceId: "mix",
      startSample: BigInt(startSample),
      generation: 0,
      sampleRate: 22_050,
      channels: tone(22_050, startSample, frames),
    });
    const sink = (options) =>
      new MediabunnyMp4Sink({
        audioCodec: "aac",
        audioBitrate: BITRATE,
        audioSampleRate: 44_100,
        destination: { kind: "memory", maxBytes: 1 << 24 },
        ...options,
      });
    await assert.rejects(
      sink({}).writeAudio(block(0, 256)),
      (error) =>
        error instanceof RangeError &&
        /audioResampler is required/.test(error.message),
    );
    const gap = sink({ audioResampler: binding });
    await gap.writeAudio(block(0, 256));
    await assert.rejects(
      gap.writeAudio(block(512, 256)),
      (error) =>
        error instanceof RangeError && /follow one another/.test(error.message),
    );
    await gap.abort(new Error("done"));
    assert.throws(
      () => sink({ audioSampleRate: 44_100.5 }),
      /audioSampleRate must be a positive integer/,
    );
  },
);
