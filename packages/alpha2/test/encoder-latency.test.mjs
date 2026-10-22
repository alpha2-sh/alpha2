// The measured video encoder latency mode, on a stand-in VideoEncoder that behaves per codec as
// Safari's and Firefox's H.264 encoders do.
import assert from "node:assert/strict";
import test from "node:test";

/** Per codec and latency mode: `works`, `silent` (never puts out or flushes), or `fails`. */
const behaviours = {
  avc1: { quality: "silent", realtime: "works" }, // Safari
  hev1: { quality: "works", realtime: "fails" }, // Firefox
  av01: { quality: "fails", realtime: "works" },
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
    this.behaviour = behaviours[config.codec.slice(0, 4)][config.latencyMode];
    opened.push([config.codec.slice(0, 4), config.latencyMode]);
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
    if (this.behaviour === "works") return Promise.resolve();
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
const { videoEncoderLatencyMode } = await import("../dist/mediabunny.js");

test("an encoder that puts nothing out in quality mode opens in realtime mode where that works", async () => {
  const started = performance.now();
  assert.equal(await videoEncoderLatencyMode("avc"), "realtime");
  assert.deepEqual(
    opened.filter(([codec]) => codec === "avc1"),
    [
      ["avc1", "quality"],
      ["avc1", "realtime"],
    ],
  );
  const waited = performance.now() - started;
  assert.ok(waited >= 1_450 && waited < 2_500, `waited ${waited} ms`);
  assert.equal(await videoEncoderLatencyMode("avc"), "realtime");
  assert.equal(opened.filter(([codec]) => codec === "avc1").length, 2);
});

test("an encoder that works in quality mode keeps it, and realtime is never tried", async () => {
  assert.equal(await videoEncoderLatencyMode("hevc"), "quality");
  assert.deepEqual(
    opened.filter(([codec]) => codec === "hev1"),
    [["hev1", "quality"]],
  );
});

test("an encoder that fails, or is not offered, is no evidence for realtime mode", async () => {
  assert.equal(await videoEncoderLatencyMode("av1"), "quality");
  assert.deepEqual(
    opened.filter(([codec]) => codec === "av01"),
    [["av01", "quality"]],
  );
  assert.equal(await videoEncoderLatencyMode("vp9"), "quality");
  assert.equal(opened.filter(([codec]) => codec === "vp09").length, 0);
});
