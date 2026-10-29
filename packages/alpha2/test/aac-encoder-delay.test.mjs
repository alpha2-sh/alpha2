// An AAC encoder's priming, which WebCodecs does not report, is measured with the browser's own
// encoder and decoder, and the sink starts AAC audio that much before the output's zero so the
// muxer trims it: on stand-ins that delay their audio as Chrome's and Safari's macOS encoders do,
// one of them dropping the first frame of its input as Safari's does.
import assert from "node:assert/strict";
import test from "node:test";

import {
  ALL_FORMATS,
  BufferSource,
  CustomAudioEncoder,
  EncodedPacket,
  Input,
  registerEncoder,
} from "mediabunny";

/** The stand-in's priming, in frames. */
const DELAY = 2_112;
/** Rates the stand-in encoder refuses, and rates at which it drops its first input frame. */
const REFUSED_RATES = [16_000];
const FIRST_FRAME_DROPPED_RATES = [32_000];
/** The rate it puts out above which, as Safari's encoder does, whatever rate it accepted. */
const HIGHEST_OUTPUT_RATE = 48_000;
let decoderFails = false;
const configured = [];
const descriptions = [];

class StandInAudioData {
  constructor({ sampleRate, numberOfFrames, data }) {
    this.sampleRate = sampleRate;
    this.numberOfFrames = numberOfFrames;
    this.data = data;
  }
  copyTo(destination) {
    destination.set(this.data.subarray(0, this.numberOfFrames));
  }
  close() {}
}

/** Puts out its first channel, delayed by its priming, as one chunk. */
class StandInAudioEncoder {
  static async isConfigSupported(config) {
    return { supported: !REFUSED_RATES.includes(config.sampleRate), config };
  }
  constructor({ output }) {
    this.output = output;
    this.state = "unconfigured";
  }
  configure(config) {
    this.config = config;
    configured.push([config.codec, config.sampleRate]);
    this.state = "configured";
  }
  encode(audio) {
    const delayed = new Float32Array(DELAY + audio.numberOfFrames);
    delayed.set(audio.data.subarray(0, audio.numberOfFrames), DELAY);
    if (FIRST_FRAME_DROPPED_RATES.includes(this.config.sampleRate))
      delayed.fill(0, DELAY, DELAY + 1_024);
    // Safari's encoder describes its stream with a whole ES_Descriptor.
    this.output(
      { delayed },
      {
        decoderConfig: {
          codec: this.config.codec,
          sampleRate: Math.min(this.config.sampleRate, HIGHEST_OUTPUT_RATE),
          description: new Uint8Array([3, 128, 128, 128, 34]),
        },
      },
    );
  }
  flush() {
    return Promise.resolve();
  }
  close() {
    this.state = "closed";
  }
}

class StandInAudioDecoder {
  constructor({ output, error }) {
    this.output = output;
    this.error = error;
    this.state = "unconfigured";
  }
  configure(config) {
    this.config = config;
    descriptions.push(Array.from(config.description));
    this.state = "configured";
  }
  decode(chunk) {
    if (decoderFails) {
      queueMicrotask(() => this.error(new DOMException("no", "EncodingError")));
      return;
    }
    this.output(
      new StandInAudioData({
        sampleRate: this.config.sampleRate,
        numberOfFrames: chunk.delayed.length,
        data: chunk.delayed,
      }),
    );
  }
  flush() {
    return Promise.resolve();
  }
  close() {
    this.state = "closed";
  }
}

globalThis.AudioData = StandInAudioData;
globalThis.AudioEncoder = StandInAudioEncoder;
globalThis.AudioDecoder = StandInAudioDecoder;

/** What the sink's AAC encoder was given: each sample's start, in frames, and its peak. */
const given = [];
const sinkCodecs = [];

/** The sink's AAC encoder: one packet per sample, at the sample's time. */
class RecordingAacEncoder extends CustomAudioEncoder {
  static supports(codec, config) {
    sinkCodecs.push(config.codec);
    return codec === "aac";
  }
  init() {
    this.configured = false;
  }
  encode(sample) {
    const plane = new Float32Array(sample.numberOfFrames);
    sample.copyTo(plane, { planeIndex: 0, format: "f32-planar" });
    given.push({
      start: Math.round(sample.timestamp * sample.sampleRate),
      peak: Math.max(...plane.map(Math.abs)),
    });
    const meta = this.configured
      ? undefined
      : {
          decoderConfig: {
            codec: "mp4a.40.2",
            numberOfChannels: 2,
            sampleRate: 48_000,
            // AAC-LC, 48 kHz, stereo.
            description: new Uint8Array([0x11, 0x90]),
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
registerEncoder(RecordingAacEncoder);

const { aacEncoderDelay, audioEncoderSampleRate, MediabunnySink } =
  await import("../dist/mediabunny.js");

test("the AAC-LC encoder's priming is measured once per configuration", async () => {
  configured.length = 0;
  descriptions.length = 0;
  const stereo = { sampleRate: 48_000, numberOfChannels: 2, bitrate: 128_000 };
  assert.equal(await aacEncoderDelay(stereo), DELAY);
  assert.equal(await aacEncoderDelay(stereo), DELAY);
  // AAC-LC at 24 kHz and below too, where mediabunny would open HE-AAC.
  assert.equal(await aacEncoderDelay({ ...stereo, sampleRate: 22_050 }), DELAY);
  assert.deepEqual(configured, [
    ["mp4a.40.2", 48_000],
    ["mp4a.40.2", 22_050],
  ]);
  // The decoder is given AAC-LC's AudioSpecificConfig, not the encoder's ES_Descriptor.
  assert.deepEqual(descriptions, [
    [0x11, 0x90],
    [0x13, 0x90],
  ]);
});

test("an encoder that drops the first frame of its input measures the same priming", async () => {
  assert.equal(
    await aacEncoderDelay({
      sampleRate: 32_000,
      numberOfChannels: 2,
      bitrate: 128_000,
    }),
    DELAY,
  );
});

test("an AAC encoder or decoder that fails measures no priming", async () => {
  assert.equal(
    await aacEncoderDelay({
      sampleRate: 16_000,
      numberOfChannels: 1,
      bitrate: 64_000,
    }),
    0,
  );
  decoderFails = true;
  try {
    assert.equal(
      await aacEncoderDelay({
        sampleRate: 44_100,
        numberOfChannels: 1,
        bitrate: 96_000,
      }),
      0,
    );
  } finally {
    decoderFails = false;
  }
});

test("the sink encodes AAC-LC starting a silent frame and its priming before zero, and the MP4 trims them", async () => {
  given.length = 0;
  sinkCodecs.length = 0;
  const sink = new MediabunnySink({
    container: "mp4",
    audioCodec: "aac",
    audioBitrate: 128_000,
    destination: { kind: "memory", maxBytes: 1 << 24 },
  });
  const block = new Float32Array(1_024).fill(0.25);
  for (let start = 0; start < 48_000; start += 1_024)
    await sink.writeAudio({
      sourceId: "mix",
      startSample: BigInt(start),
      generation: 0,
      sampleRate: 48_000,
      channels: [block, block],
    });
  await sink.finish();
  assert.ok(sinkCodecs.every((codec) => codec === "mp4a.40.2"));
  assert.deepEqual(given.slice(0, 3), [
    { start: -1_024 - DELAY, peak: 0 },
    { start: -DELAY, peak: 0.25 },
    { start: 1_024 - DELAY, peak: 0.25 },
  ]);
  const input = new Input({
    source: new BufferSource(sink.output.buffer),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    // The edit list puts the encoder's first frame before zero, where players drop it.
    assert.ok(
      Math.abs((await track.getFirstTimestamp()) + (1_024 + DELAY) / 48_000) <
        1e-6,
    );
  } finally {
    input.dispose();
  }
});

test("an AAC rate the encoder puts out at another rate is not taken, nor one MPEG-4 does not name", async () => {
  const options = { numberOfChannels: 2, bitrate: 128_000 };
  // 96, 88.2 and 64 kHz come out at 48 kHz.
  assert.equal(
    await audioEncoderSampleRate("aac", { ...options, sampleRate: 96_000 }),
    48_000,
  );
  // 50 kHz is no MPEG-4 rate: the next above, 64 kHz, comes out at 48 kHz too.
  assert.equal(
    await audioEncoderSampleRate("aac", { ...options, sampleRate: 50_000 }),
    48_000,
  );
  assert.equal(
    await audioEncoderSampleRate("aac", { ...options, sampleRate: 44_100 }),
    44_100,
  );
  // Such a rate measures no priming either.
  assert.equal(await aacEncoderDelay({ ...options, sampleRate: 96_000 }), 0);
});
