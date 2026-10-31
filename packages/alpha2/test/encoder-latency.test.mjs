// The measured video encoder latency mode, on a stand-in VideoEncoder that behaves per codec as
// Safari's and Firefox's encoders do, and as a cold encoder on a loaded machine can.
import assert from "node:assert/strict";
import test from "node:test";

/**
 * Per codec and latency mode: `works`, `hangs` (puts nothing out and never flushes), `silent`
 * (flushes with nothing out), or `fails`. A list is taken one entry per encoder opened.
 */
const behaviours = {
  avc1: { quality: "hangs", realtime: "works" }, // Safari
  hev1: { quality: "works", realtime: "fails" }, // Firefox
  av01: { quality: "silent", realtime: "works" },
  vp09: { quality: "hangs", realtime: "fails" },
};
const opened = [];

class StandInVideoFrame {
  close() {}
}

class StandInVideoEncoder {
  static async isConfigSupported(config) {
    return { supported: config.codec.slice(0, 4) in behaviours, config };
  }

  constructor({ output, error }) {
    this.output = output;
    this.error = error;
    this.state = "unconfigured";
  }

  configure(config) {
    const codec = config.codec.slice(0, 4);
    const planned = behaviours[codec][config.latencyMode];
    this.behaviour = Array.isArray(planned) ? planned.shift() : planned;
    opened.push([codec, config.latencyMode]);
    this.state = "configured";
  }

  encode() {
    if (this.behaviour === "works") queueMicrotask(() => this.output({}));
    if (this.behaviour === "fails")
      queueMicrotask(() =>
        this.error(
          new DOMException(
            "The given encoding is not supported.",
            "EncodingError",
          ),
        ),
      );
  }

  flush() {
    if (this.behaviour === "works" || this.behaviour === "silent")
      return Promise.resolve();
    if (this.behaviour === "fails")
      return Promise.reject(new DOMException("closed", "EncodingError"));
    return new Promise(() => {});
  }

  close() {
    this.state = "closed";
  }
}

globalThis.VideoEncoder = StandInVideoEncoder;
globalThis.VideoFrame = StandInVideoFrame;
const { videoEncoderLatencyMode, videoEncoderLatencyProbe } =
  await import("../dist/mediabunny.js");

const openedFor = (codec) => opened.filter(([name]) => name === codec);

test("a quality probe that times out twice opens in realtime mode where that works", async () => {
  const started = performance.now();
  const probe = await videoEncoderLatencyProbe("avc");
  assert.equal(probe.mode, "realtime");
  assert.equal(probe.outcome, "output");
  assert.equal(probe.cached, false);
  // Cold quality, realtime, then quality once more, warm.
  assert.deepEqual(openedFor("avc1"), [
    ["avc1", "quality"],
    ["avc1", "realtime"],
    ["avc1", "quality"],
  ]);
  const waited = performance.now() - started;
  // Two quality probes wait out their 1.5 s; the upper bound catches only a probe that hangs.
  assert.ok(waited >= 2_900 && waited < 3_000 + 10_000, `waited ${waited} ms`);
  assert.ok(probe.ms >= 2_900);
  assert.equal(await videoEncoderLatencyMode("avc"), "realtime");
  assert.equal(openedFor("avc1").length, 3, "a definitive answer is kept");
  assert.equal((await videoEncoderLatencyProbe("avc")).cached, true);
});

test("a slow cold quality probe is measured again warm", async () => {
  behaviours.hev1 = { quality: ["hangs", "works"], realtime: "works" };
  const probe = await videoEncoderLatencyProbe("hevc");
  assert.equal(probe.mode, "quality");
  assert.equal(probe.outcome, "output");
  assert.deepEqual(openedFor("hev1"), [
    ["hev1", "quality"],
    ["hev1", "realtime"],
    ["hev1", "quality"],
  ]);
});

test("a timed out probe is not cached", async () => {
  const first = await videoEncoderLatencyProbe("vp9");
  assert.equal(first.mode, "quality");
  assert.equal(first.outcome, "failed");
  assert.equal(openedFor("vp09").length, 2);
  // Nothing definitive was learnt: the next export measures again, and this time it works.
  behaviours.vp09 = { quality: "works", realtime: "fails" };
  const second = await videoEncoderLatencyProbe("vp9");
  assert.equal(second.mode, "quality");
  assert.equal(second.cached, false);
  assert.equal(openedFor("vp09").length, 3);
});

test("a silent flush is cached as silent", async () => {
  // A quality encoder that flushed with nothing out is evidence: realtime is tried at once and the
  // answer kept, with no warm second look.
  assert.equal(await videoEncoderLatencyMode("av1"), "realtime");
  assert.deepEqual(openedFor("av01"), [
    ["av01", "quality"],
    ["av01", "realtime"],
  ]);
  assert.equal(await videoEncoderLatencyMode("av1"), "realtime");
  assert.equal(openedFor("av01").length, 2);
});

test("a silent quality encoder opens in realtime mode when the realtime probe only timed out", async () => {
  // A cold hardware realtime encoder can miss the probe's deadline; quality never puts out. A
  // fresh copy of the module has nothing measured yet.
  behaviours.avc1 = { quality: "silent", realtime: ["hangs", "works"] };
  const fresh = await import("../dist/mediabunny.js?silent-quality");
  const first = await fresh.videoEncoderLatencyProbe("avc");
  assert.equal(first.mode, "realtime");
  assert.equal(first.outcome, "timed-out");
  // Not definitive: the next export measures again, and the warm realtime encoder decides.
  const second = await fresh.videoEncoderLatencyProbe("avc");
  assert.equal(second.mode, "realtime");
  assert.equal(second.outcome, "output");
  assert.equal(second.cached, false);
});
