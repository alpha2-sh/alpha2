// How the AAC priming probe treats an encoder that does not answer in time: asked once more, and
// never kept as a measurement, unlike an answer.
import assert from "node:assert/strict";
import test from "node:test";

const DELAY = 2_112;
/** Encoders the stand-in opens that never flush, by sample rate; each opening takes one. */
const hangs = new Map();
const opened = [];

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

class StandInAudioEncoder {
  static async isConfigSupported(config) {
    return { supported: true, config };
  }
  constructor({ output }) {
    this.output = output;
    this.state = "unconfigured";
  }
  configure(config) {
    this.config = config;
    opened.push(config.sampleRate);
    const left = hangs.get(config.sampleRate) ?? 0;
    this.hangs = left > 0;
    hangs.set(config.sampleRate, left - 1);
    this.state = "configured";
  }
  encode(audio) {
    if (this.hangs) return;
    const delayed = new Float32Array(DELAY + audio.numberOfFrames);
    delayed.set(audio.data.subarray(0, audio.numberOfFrames), DELAY);
    this.output(
      { delayed },
      {
        decoderConfig: {
          codec: this.config.codec,
          sampleRate: this.config.sampleRate,
        },
      },
    );
  }
  flush() {
    return this.hangs ? new Promise(() => {}) : Promise.resolve();
  }
  close() {
    this.state = "closed";
  }
}

class StandInAudioDecoder {
  constructor({ output }) {
    this.output = output;
    this.state = "unconfigured";
  }
  configure(config) {
    this.config = config;
    this.state = "configured";
  }
  decode(chunk) {
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
const { aacEncoderPrimingProbe } = await import("../dist/mediabunny.js");

const config = (sampleRate) => ({
  sampleRate,
  numberOfChannels: 2,
  bitrate: 128_000,
});
const openedAt = (rate) => opened.filter((at) => at === rate).length;

test("a timed out aac probe is retried once and not cached", async () => {
  hangs.set(44_100, 2);
  const timedOut = await aacEncoderPrimingProbe(config(44_100));
  assert.equal(timedOut.outcome, "timed-out");
  assert.equal(timedOut.delay, 0, "this export goes on without the priming");
  assert.equal(timedOut.attempts, 2);
  assert.ok(timedOut.ms >= 2_900, `${timedOut.ms} ms`);
  assert.equal(openedAt(44_100), 2);
  // The next export measures again, and this time the encoder answers.
  const measured = await aacEncoderPrimingProbe(config(44_100));
  assert.equal(measured.outcome, "output");
  assert.equal(measured.delay, DELAY);
  assert.equal(measured.attempts, 1);
  assert.equal(openedAt(44_100), 3);
});

test("an aac probe that answers the second time is kept", async () => {
  hangs.set(48_000, 1);
  const probe = await aacEncoderPrimingProbe(config(48_000));
  assert.equal(probe.outcome, "output");
  assert.equal(probe.delay, DELAY);
  assert.equal(probe.attempts, 2);
  assert.equal(await aacEncoderPrimingProbe(config(48_000)), probe);
  assert.equal(openedAt(48_000), 2);
});
