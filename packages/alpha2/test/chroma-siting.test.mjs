import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import {
  ALL_FORMATS,
  BufferSource,
  CustomVideoDecoder,
  EncodedPacketSink,
  Input,
  VideoSample,
  registerDecoder,
} from "mediabunny";

import {
  av1SequenceHeaderSiting,
  chromaSitingForCodec,
  chromaSitingForStream,
  h264SpsSiting,
  hevcSpsSiting,
  sampleAspectForStream,
} from "../dist/chroma-siting.js";
import { WebCodecsVideoDecoder } from "../dist/media.js";
import { MediabunnyVideoDecoder } from "../dist/mediabunny.js";

const corpus = join(
  resolve(import.meta.dirname, "..", "..", ".."),
  "tests",
  "corpus",
);

/**
 * What an SDK adapter reads of a file: its decoder configuration, and its first packet when that
 * has no parameter sets.
 */
async function streamOf(name) {
  const input = new Input({
    source: new BufferSource(readFileSync(join(corpus, name))),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryVideoTrack();
    const config = await track.getDecoderConfig();
    const description = config.description ?? undefined;
    const first =
      description === undefined
        ? await new EncodedPacketSink(track).getFirstPacket()
        : null;
    return { codec: config.codec, description, packet: first?.data };
  } finally {
    input.dispose();
  }
}

test("a stream's parameter sets say where its chroma sits, as FFmpeg reports it natively", async () => {
  // FFmpeg reports these for the same files: ffprobe -show_entries frame=chroma_location.
  for (const [name, siting] of [
    ["hdr-pq.mp4", "left"],
    ["hdr-pq-topleft.mp4", "top-left"],
    ["h264-bt709-copy.mp4", "left"],
    // MPEG-TS has no avcC: the SPS is read from the first packet, Annex B.
    ["h264-bt709-copy.ts", "left"],
    // No VUI: H.264 infers type 0, which FFmpeg leaves unspecified and the native decoder reads as
    // left.
    ["h264-bt709-no-vui.mp4", "left"],
    ["hevc-topleft.mp4", "top-left"],
    // AV1 in MP4 carries no configuration OBUs here: the sequence header is the first packet's.
    ["av1.mp4", "unspecified"],
    ["av1-colocated.mp4", "top-left"],
    ["vp9.webm", "unspecified"],
  ]) {
    const { codec, description, packet } = await streamOf(name);
    assert.equal(
      chromaSitingForStream(codec, description, packet),
      siting,
      name,
    );
  }
});

test("without parameter sets, or with ones that end early, a codec's default answers", () => {
  assert.equal(chromaSitingForStream("avc1.64000a"), "left");
  assert.equal(chromaSitingForStream("hvc1.1.6.L93.B0", null, null), "left");
  assert.equal(chromaSitingForStream("av01.0.04M.08"), "unspecified");
  const truncated = Uint8Array.from([1, 0x64, 0, 10, 0xff, 0xe1, 0, 9, 0x67]);
  assert.equal(chromaSitingForStream("avc1.64000a", truncated), "left");
  assert.equal(chromaSitingForCodec("mp4v.20.9"), "left");
});

test("an H.264 SPS is read through its scaling lists and frame fields to the VUI", () => {
  // FFmpeg's H.264 decoder: type + 1 for 0 through 5, centred natively for the top and bottom
  // types; a reserved type, or no VUI, is left, as H.264 infers it without one and the native
  // decoder reads it.
  for (const [type, siting] of [
    [0, "left"],
    [1, "center"],
    [2, "top-left"],
    [3, "unspecified"],
    [5, "unspecified"],
    [6, "left"],
  ]) {
    assert.equal(h264SpsSiting(h264Sps(type)), siting, `type ${type}`);
  }
  assert.equal(h264SpsSiting(h264Sps(undefined)), "left", "no VUI");
  assert.equal(
    h264SpsSiting(h264Sps(null)),
    "left",
    "VUI without a chroma location",
  );
  // The same SPS as an Annex B packet, the way MPEG-TS carries it.
  const packet = Uint8Array.from([
    0,
    0,
    0,
    1,
    ...h264Sps(2),
    0,
    0,
    1,
    0x65,
    0x88,
  ]);
  assert.equal(
    chromaSitingForStream("avc1.64001e", undefined, packet),
    "top-left",
  );
});

test("an HEVC SPS is read through sub-layers, scaling lists, predicted reference sets, and long-term pictures to the VUI", () => {
  for (const subLayers of [0, 1]) {
    for (const [type, siting] of [
      [0, "left"],
      [1, "center"],
      [2, "top-left"],
      [4, "unspecified"],
    ]) {
      assert.equal(
        hevcSpsSiting(hevcSps(type, subLayers)),
        siting,
        `type ${type}, ${subLayers + 1} sub-layers`,
      );
    }
  }
});

test("a stream's parameter sets say the shape of its samples, as FFmpeg's decoder reports it", async () => {
  // An MP4 with no `pasp` whose SPS VUI says 4:3 (natively 171x96 from 128x96 samples), and its
  // transport stream twin read from the first packet; streams whose VUI says nothing.
  for (const [name, aspect] of [
    ["h264-bt709-vui-sar.mp4", { num: 4, den: 3 }],
    ["h264-bt709-vui-sar.ts", { num: 4, den: 3 }],
    // x264 writes square samples explicitly.
    ["h264-bt709-copy.mp4", { num: 1, den: 1 }],
    ["h264-bt709-no-vui.mp4", undefined],
    ["vp9.webm", undefined],
  ]) {
    const { codec, description, packet } = await streamOf(name);
    assert.deepEqual(
      sampleAspectForStream(codec, description, packet),
      aspect,
      name,
    );
  }
  // Parse through scaling lists to the VUI: a table index (H.264's 2, 12:11) and an Extended_SAR
  // (HEVC's 4:3) from Annex B first packets; a truncated parameter set says nothing.
  const annexB = (nal) => Uint8Array.from([0, 0, 0, 1, ...nal]);
  assert.deepEqual(
    sampleAspectForStream("avc1.64001e", null, annexB(h264Sps(1))),
    { num: 12, den: 11 },
  );
  assert.deepEqual(
    sampleAspectForStream("hvc1.1.6.L90.B0", null, annexB(hevcSps(1, 1))),
    { num: 4, den: 3 },
  );
  assert.equal(
    sampleAspectForStream(
      "avc1.64000a",
      Uint8Array.from([1, 0x64, 0, 10, 0xff, 0xe1, 0, 9, 0x67]),
    ),
    undefined,
  );
});

test("an AV1 sequence header names a position only for 4:2:0", () => {
  // seq_profile 0, reduced still picture header, 8-bit, no colour description, studio range,
  // chroma_sample_position.
  const header = (position) => {
    const bits = new BitWriter();
    bits.u(3, 0).flag(1).flag(1).u(5, 0);
    bits.u(4, 6).u(4, 5).u(7, 63).u(6, 47);
    bits.flag(0).flag(0).flag(0);
    bits.flag(0).flag(0).flag(0);
    bits.flag(0).flag(0).flag(0).flag(0).u(2, position);
    return bits.bytes();
  };
  assert.equal(av1SequenceHeaderSiting(header(0)), "unspecified");
  assert.equal(av1SequenceHeaderSiting(header(1)), "left");
  assert.equal(av1SequenceHeaderSiting(header(2)), "top-left");
  assert.equal(av1SequenceHeaderSiting(header(3)), "unspecified");
});

/** Node has no WebCodecs: a frame with what the adapters read. */
class TestVideoFrame {
  constructor(source, init = {}) {
    this.timestamp = init.timestamp ?? source.timestamp ?? 0;
    this.duration = init.duration ?? null;
    this.format = "I420";
    this.codedWidth = 64;
    this.codedHeight = 48;
    this.displayWidth = 64;
    this.displayHeight = 48;
    this.visibleRect = { x: 0, y: 0, width: 64, height: 48 };
    this.colorSpace = {
      primaries: null,
      transfer: null,
      matrix: null,
      fullRange: false,
      toJSON() {
        return { ...this };
      },
    };
  }

  clone() {
    return new TestVideoFrame(this, this);
  }

  allocationSize() {
    return 64 * 48 * 1.5;
  }

  async copyTo() {
    return [];
  }

  close() {}
}

globalThis.VideoFrame = TestVideoFrame;

class FrameEachPacketDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return codec === "avc";
  }
  init() {}
  decode(packet) {
    this.onSample(
      new VideoSample(
        new TestVideoFrame(
          {},
          { timestamp: Math.round(packet.timestamp * 1e6) },
        ),
      ),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(FrameEachPacketDecoder);

test("the mediabunny adapter tells the engine where the stream's chroma sits", async () => {
  for (const [name, siting] of [
    ["hdr-pq.mp4", "left"],
    ["hdr-pq-topleft.mp4", "top-left"],
  ]) {
    const decoder = await MediabunnyVideoDecoder.open(
      "clip",
      new Blob([readFileSync(join(corpus, name))]),
    );
    try {
      const decoded = await decoder.refill({
        sourceId: "clip",
        mediaTimeTicks: 0n,
        reason: "seek",
        generation: 0,
      });
      assert.equal(decoded.chromaSiting, siting, name);
      decoded.frame.close();
    } finally {
      decoder.close();
    }
  }
});

test("the WebCodecs adapter reads the siting from the first key chunk of a stream configured without parameter sets", async () => {
  const original = globalThis.VideoDecoder;
  globalThis.VideoDecoder = class extends EventTarget {
    decodeQueueSize = 0;
    #output;
    constructor({ output }) {
      super();
      this.#output = output;
    }
    configure() {}
    decode(chunk) {
      queueMicrotask(() => this.#output(new TestVideoFrame(chunk)));
    }
    async flush() {}
    close() {}
  };
  // An Annex B access unit that repeats its SPS, as avc3 and MPEG-TS streams do.
  const data = Uint8Array.from([
    0,
    0,
    0,
    1,
    ...h264Sps(2),
    0,
    0,
    1,
    0x65,
    0x88,
  ]);
  const chunk = (timestamp, type) => ({
    type,
    timestamp,
    duration: 33_333,
    byteLength: data.byteLength,
    copyTo(destination) {
      destination.set(data);
    },
  });
  const decoder = new WebCodecsVideoDecoder("source", {
    config: { codec: "avc3.64001e" },
    chunksFrom: () =>
      (async function* chunks() {
        yield chunk(0, "key");
        yield chunk(33_333, "delta");
      })(),
  });
  try {
    const decoded = await decoder.refill({
      sourceId: "source",
      mediaTimeTicks: 0n,
      reason: "seek",
      generation: 0,
    });
    assert.equal(decoded.chromaSiting, "top-left");
    decoded.frame.close();
  } finally {
    decoder.close();
    globalThis.VideoDecoder = original;
  }
});

/** Writes a parameter set's bits, then the RBSP trailing bits and emulation prevention. */
class BitWriter {
  #bits = [];

  u(count, value) {
    for (let bit = count - 1; bit >= 0; bit -= 1)
      this.#bits.push(Math.floor(value / 2 ** bit) % 2);
    return this;
  }

  flag(value) {
    return this.u(1, value ? 1 : 0);
  }

  ue(value) {
    const code = value + 1;
    const length = Math.floor(Math.log2(code));
    this.u(length, 0);
    return this.u(length + 1, code);
  }

  se(value) {
    return this.ue(value > 0 ? 2 * value - 1 : -2 * value);
  }

  /** The bits as bytes, zero-padded, without trailing bits. */
  bytes() {
    const bits = [...this.#bits];
    while (bits.length % 8 !== 0) bits.push(0);
    const bytes = [];
    for (let at = 0; at < bits.length; at += 8)
      bytes.push(Number.parseInt(bits.slice(at, at + 8).join(""), 2));
    return Uint8Array.from(bytes);
  }

  nal(header) {
    this.#bits.push(1);
    const output = [...header];
    let zeros = 0;
    for (const byte of this.bytes()) {
      if (zeros >= 2 && byte <= 3) {
        output.push(3);
        zeros = 0;
      }
      output.push(byte);
      zeros = byte === 0 ? zeros + 1 : 0;
    }
    return Uint8Array.from(output);
  }
}

/**
 * An H.264 High SPS with explicit scaling lists, POC type 1 and cropping, and VUI whose chroma
 * location is `type`: `null` for VUI without one, `undefined` for no VUI. FFmpeg's CBS reads its
 * chroma location where this does (`trace_headers`).
 */
function h264Sps(type) {
  const bits = new BitWriter();
  bits.u(8, 100).u(8, 0).u(8, 30).ue(0).ue(1).ue(0).ue(0).flag(0);
  bits.flag(1);
  for (let list = 0; list < 8; list += 1) {
    if (list % 3 === 1) {
      bits.flag(0);
      continue;
    }
    bits.flag(1);
    for (let index = 0; index < (list < 6 ? 16 : 64); index += 1)
      bits.se(index === 5 ? -8 : 1);
  }
  bits.ue(0).ue(1).flag(0).se(-1).se(2).ue(2).se(1).se(-1);
  bits.ue(4).flag(0).ue(3).ue(2).flag(0).flag(1).flag(1);
  bits.flag(1).ue(0).ue(2).ue(0).ue(1);
  if (type === undefined) return bits.flag(0).nal([0x67]);
  bits.flag(1);
  bits.flag(1).u(8, 2).flag(0);
  bits.flag(1).u(3, 5).flag(0).flag(1).u(8, 1).u(8, 1).u(8, 1);
  if (type === null) bits.flag(0);
  else bits.flag(1).ue(type).ue(type);
  bits.flag(0).flag(0).flag(0).flag(0).flag(0);
  return bits.nal([0x67]);
}

/**
 * An HEVC Main SPS with `subLayers` extra sub-layers, explicit scaling lists, PCM, three short-term
 * reference sets of which two are predicted (the first prediction drops the entry its delta brings
 * to zero, so the second reads three entries and four flags), two long-term pictures, and VUI
 * chroma location `type`. FFmpeg's CBS reads its chroma location where this does.
 */
function hevcSps(type, subLayers) {
  const bits = new BitWriter();
  bits.u(4, 0).u(3, subLayers).u(1, 1);
  const profile = (level) =>
    bits
      .u(2, 0)
      .u(1, 0)
      .u(5, 1)
      .u(32, 0x60000000)
      .u(4, 0b1001)
      .u(43, 0)
      .u(1, 0)
      .u(8, level);
  profile(90);
  if (subLayers === 1) {
    bits.flag(1).flag(1).u(14, 0);
    profile(60);
  }
  bits.ue(0).ue(1).ue(64).ue(48).flag(0).ue(0).ue(0).ue(4);
  bits.flag(1);
  for (let layer = 0; layer <= subLayers; layer += 1) bits.ue(3).ue(1).ue(0);
  bits.ue(0).ue(2).ue(0).ue(3).ue(1).ue(1);
  bits.flag(1).flag(1);
  for (let size = 0; size < 4; size += 1) {
    for (let matrix = 0; matrix < 6; matrix += size === 3 ? 3 : 1) {
      if ((size + matrix) % 2 === 0) {
        bits.flag(0).ue(matrix > 0 ? 1 : 0);
        continue;
      }
      bits.flag(1);
      if (size > 1) bits.se(3);
      for (
        let index = 0;
        index < Math.min(64, 1 << (4 + (size << 1)));
        index += 1
      )
        bits.se(index % 3 === 0 ? -2 : 1);
    }
  }
  bits.flag(1).flag(1);
  bits.flag(1).u(4, 7).u(4, 7).ue(0).ue(1).flag(0);
  bits.ue(3);
  // Set 0: S0 = [-1, -3], S1 = [+1].
  bits.ue(2).ue(1).ue(0).flag(1).ue(1).flag(1).ue(0).flag(0);
  // Set 1 from set 0, deltaRps +1: S0 = [-2], S1 = [+1, +2].
  bits.flag(1).flag(0).ue(0).flag(1).flag(0).flag(1).flag(1).flag(0).flag(1);
  // Set 2 from set 1, deltaRps -1: four flags.
  bits.flag(1).flag(1).ue(0).flag(1).flag(1).flag(1).flag(1);
  bits.flag(1).ue(2).u(8, 17).flag(1).u(8, 33).flag(0);
  bits.flag(1).flag(1);
  bits.flag(1);
  bits.flag(1).u(8, 255).u(16, 4).u(16, 3);
  bits.flag(0);
  bits.flag(1).u(3, 5).flag(0).flag(1).u(8, 9).u(8, 16).u(8, 9);
  bits.flag(1).ue(type).ue(type);
  bits.flag(0).flag(0).flag(0).flag(0).flag(0).flag(0);
  bits.flag(0);
  return bits.nal([0x42, 0x01]);
}
