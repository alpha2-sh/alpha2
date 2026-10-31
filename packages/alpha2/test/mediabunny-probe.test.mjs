import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { CustomVideoDecoder, VideoSample, registerDecoder } from "mediabunny";

import {
  DecodeUnavailableError,
  PlanarDecodeUnavailableError,
} from "../dist/hdr-frames.js";
import { probePlanarDecode } from "../dist/mediabunny.js";

/**
 * What the stand-in browser does: whether its decoder takes the codec, the format of the frames
 * its decoder gives (null for an opaque hardware frame), and whether it has a software decoder and
 * what that one gives.
 */
const browser = {
  decodes: true,
  format: "I420P10",
  software: false,
  softwareFormat: "I420P10",
  decoded: [],
  closed: 0,
};

/** Node has no WebCodecs: the frame properties the probe reads. */
class TestVideoFrame {
  #closed = false;
  constructor(source, init = {}) {
    this.format =
      source instanceof TestVideoFrame ? source.format : init.format;
    this.codedWidth = 64;
    this.codedHeight = 48;
    this.timestamp = init.timestamp ?? source.timestamp ?? 0;
    this.duration = init.duration ?? source.duration ?? 0;
    this.displayWidth = 64;
    this.displayHeight = 48;
    this.colorSpace = { transfer: "hlg", toJSON: () => ({}) };
  }
  allocationSize() {
    if (this.format === null)
      throw new DOMException("opaque", "NotSupportedError");
    return 64 * 48 * 3;
  }
  clone() {
    return new TestVideoFrame(this);
  }
  close() {
    if (this.#closed) return;
    this.#closed = true;
    browser.closed += 1;
  }
}

class StandInDecoder extends CustomVideoDecoder {
  static supports() {
    return browser.decodes;
  }
  init() {}
  decode(packet) {
    const software = this.config.hardwareAcceleration === "prefer-software";
    browser.decoded.push(software ? "software" : "default");
    this.onSample(
      new VideoSample(
        new TestVideoFrame(
          {},
          {
            format: software ? browser.softwareFormat : browser.format,
            timestamp: Math.round(packet.timestamp * 1e6),
            duration: Math.round(packet.duration * 1e6),
          },
        ),
      ),
    );
  }
  flush() {}
  close() {}
}

globalThis.VideoFrame = TestVideoFrame;
globalThis.VideoDecoder = {
  async isConfigSupported(config) {
    return {
      supported:
        config.hardwareAcceleration === "prefer-software"
          ? browser.software
          : browser.decodes,
      config,
    };
  },
};
registerDecoder(StandInDecoder);

const corpus = (name) =>
  readFile(new URL(`../../../tests/corpus/${name}`, import.meta.url));

/** Sets the stand-in browser for one test. */
function reset(changes = {}) {
  Object.assign(browser, {
    decodes: true,
    format: "I420P10",
    software: false,
    softwareFormat: "I420P10",
    decoded: [],
    closed: 0,
    ...changes,
  });
}

async function probe(name, declared = null) {
  const bytes = await corpus(name);
  return probePlanarDecode(new File([bytes], name, { type: "video/mp4" }), {
    declared,
    timeoutMs: 2_000,
  });
}

test("an SDR source without declared color needs no planes and is not decoded", async () => {
  reset();
  assert.deepEqual(await probe("h264-padded.mp4"), { planes: "not-required" });
  assert.deepEqual(browser.decoded, []);
});

test("an HLG source whose decoder gives planes is the decoder's", async () => {
  reset();
  assert.deepEqual(await probe("hdr-hlg.mp4"), { planes: "decoder" });
  assert.deepEqual(browser.decoded, ["default"]);
  assert.ok(browser.closed >= 1, "the frame was closed");
});

test("a declared color needs planes even on an SDR stream", async () => {
  reset({ format: null, software: true });
  const declared = {
    dynamic_range: "sdr",
    matrix: "bt601",
    primaries: "bt601",
    range: "full",
    transfer: "bt709",
  };
  assert.deepEqual(await probe("h264-padded.mp4", declared), {
    planes: "software",
  });
});

test("an opaque decoder frame with a planar software decoder is the software decoder's", async () => {
  reset({ format: null, software: true });
  assert.deepEqual(await probe("hdr-hlg.mp4"), { planes: "software" });
  assert.deepEqual(browser.decoded, ["default", "software"]);
});

test("an opaque frame with no software decoder is refused with the track's facts", async () => {
  reset({ format: null, software: false });
  await assert.rejects(probe("hdr-hlg.mp4"), (error) => {
    assert.ok(error instanceof PlanarDecodeUnavailableError, error.stack);
    assert.equal(error.code, "PLANAR_DECODE_UNAVAILABLE");
    assert.equal(error.facts.kind, "video");
    assert.equal(error.facts.transfer, "hlg");
    assert.equal(error.facts.bitDepth, 10);
    assert.equal(error.facts.frameFormat, null);
    assert.equal(error.facts.softwareDecoder, "none");
    assert.equal(error.failure.code, "INVALID_INPUT");
    assert.match(
      error.message,
      /^hdr-hlg\.mp4 is HDR video \(H\.264, 10-bit, HLG\)/,
    );
    assert.match(error.message, /no software decoder/);
    return true;
  });
  // An opaque software frame is no better.
  reset({ format: null, software: true, softwareFormat: null });
  await assert.rejects(
    probe("hdr-hlg.mp4"),
    (error) =>
      error instanceof PlanarDecodeUnavailableError &&
      error.facts.softwareDecoder === "opaque",
  );
});

test("an HLG track the browser cannot decode is refused before decoding", async () => {
  reset({ decodes: false, software: false });
  await assert.rejects(probe("hdr-hlg.mp4"), (error) => {
    assert.ok(error instanceof PlanarDecodeUnavailableError);
    assert.equal(error.facts.transfer, "hlg");
    assert.equal(error.facts.frameFormat, undefined);
    assert.match(error.message, /this browser cannot decode it/);
    return true;
  });
  assert.deepEqual(browser.decoded, []);
});

test("a file without a video track is refused as undecodable", async () => {
  reset();
  await assert.rejects(
    probe("recovery-aac.m4a"),
    (error) =>
      error instanceof DecodeUnavailableError &&
      error.code === "DECODE_UNAVAILABLE",
  );
});
