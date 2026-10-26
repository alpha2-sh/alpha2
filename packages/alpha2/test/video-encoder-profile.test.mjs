// The H.264 profile a browser export opens: High, else Main, else Constrained Baseline, asked of a
// stand-in VideoEncoder that accepts only some codec strings, as a software encoder may.
import assert from "node:assert/strict";
import test from "node:test";

import {
  canEncodeVideo,
  CustomVideoEncoder,
  EncodedPacket,
  registerEncoder,
} from "mediabunny";

/** The codec strings the stand-in encoder accepts, and every one it was asked about. */
let accepts = () => true;
const asked = [];

globalThis.VideoEncoder = class VideoEncoder {
  static async isConfigSupported(config) {
    asked.push(config.codec);
    return { supported: accepts(config.codec), config };
  }
};

const {
  canEncodeVideoTrack,
  MediabunnySink,
  videoEncoderProfiles,
  videoEncoderQuality,
} = await import("../dist/mediabunny.js");

const rate = (bitrate) => ({ bitrate, bitrateMode: "variable" });

/** Each case asks at its own size: mediabunny remembers every answer by its configuration. */
async function probe(codec, width, height, bitrate, accepted) {
  accepts = accepted;
  asked.length = 0;
  return canEncodeVideoTrack(codec, { width, height, rate: rate(bitrate) });
}

test("the High profile string is the one mediabunny builds for the same size and bitrate", async () => {
  accepts = () => true;
  for (const [width, height, bitrate] of [
    [64, 36, 50_000],
    [320, 240, 500_000],
    [1280, 720, 5_000_000],
    [1920, 1080, 12_000_000],
    [1920, 1080, 60_000_000],
    [3840, 2160, 40_000_000],
    [7680, 4320, 100_000_000],
    [8192, 8192, 900_000_000],
  ]) {
    asked.length = 0;
    await canEncodeVideo("avc", {
      width,
      height,
      quality: videoEncoderQuality(rate(bitrate)),
    });
    const profiles = videoEncoderProfiles("avc", {
      width,
      height,
      rate: rate(bitrate),
    });
    assert.equal(asked.length, 1);
    assert.equal(profiles[0].codecString, asked[0], `${width}x${height}`);
    const level = asked[0].slice(-2);
    assert.deepEqual(
      profiles.map((p) => [p.profile, p.codecString]),
      [
        ["High", `avc1.6400${level}`],
        ["Main", `avc1.4d00${level}`],
        ["Constrained Baseline", `avc1.42e0${level}`],
      ],
    );
  }
});

test("a browser that encodes High opens High and reports no fallback", async () => {
  assert.deepEqual(await probe("avc", 1280, 704, 5_000_000, () => true), {
    codecString: "avc1.64001f",
    profile: "High",
    fallbacks: [],
  });
  assert.deepEqual(asked, ["avc1.64001f"]);
});

test("a browser that refuses High opens Main and reports High as a fallback", async () => {
  const encoding = await probe("avc", 1920, 1080, 8_000_000, (codec) =>
    codec.startsWith("avc1.4d"),
  );
  assert.deepEqual(encoding, {
    codecString: "avc1.4d0028",
    profile: "Main",
    fallbacks: [
      {
        encoder: "avc1.640028",
        profile: "High",
        reason: "unsupported",
        detail: "this browser does not encode H.264 High profile at 1920x1080",
      },
    ],
  });
  assert.deepEqual(asked, ["avc1.640028", "avc1.4d0028"]);
});

test("a browser that encodes only Constrained Baseline opens it after High and Main", async () => {
  const encoding = await probe("avc", 640, 360, 1_000_000, (codec) =>
    codec.startsWith("avc1.42e0"),
  );
  assert.equal(encoding.codecString, "avc1.42e016");
  assert.equal(encoding.profile, "Constrained Baseline");
  assert.deepEqual(
    encoding.fallbacks.map((f) => [f.profile, f.encoder]),
    [
      ["High", "avc1.640016"],
      ["Main", "avc1.4d0016"],
    ],
  );
});

test("a quantizer with a bitrate fallback is asked for in each profile before the next", async () => {
  accepts = (codec) => codec.startsWith("avc1.4d");
  asked.length = 0;
  const encoding = await canEncodeVideoTrack("avc", {
    width: 1280,
    height: 544,
    rate: { bitrate: 4_000_000, bitrateMode: "variable", quantizer: 23 },
  });
  assert.equal(encoding.profile, "Main");
  // The constant-quantizer configuration first, then its bitrate fallback, per profile.
  assert.deepEqual(asked, ["avc1.64001f", "avc1.64001f", "avc1.4d001f"]);
});

test("a browser that encodes no H.264 profile is refused after asking for all three", async () => {
  assert.equal(await probe("avc", 854, 480, 2_000_000, () => false), null);
  assert.deepEqual(asked, ["avc1.640016", "avc1.4d0016", "avc1.42e016"]);
});

test("odd frame sizes are refused in every profile without asking the browser", async () => {
  assert.equal(await probe("avc", 641, 361, 1_000_000, () => true), null);
  assert.deepEqual(asked, []);
});

test("other codecs open with mediabunny's own codec string and have no profile ladder", async () => {
  assert.deepEqual(
    videoEncoderProfiles("vp9", {
      width: 1280,
      height: 720,
      rate: rate(5_000_000),
    }),
    [],
  );
  assert.deepEqual(
    await probe("vp9", 1280, 722, 5_000_000, (codec) =>
      codec.startsWith("vp09"),
    ),
    { fallbacks: [] },
  );
  assert.equal(asked.length, 1);
  assert.equal(
    await probe("hevc", 1280, 722, 5_000_000, (codec) =>
      codec.startsWith("vp09"),
    ),
    null,
  );
});

/** Records the codec string each encoder was configured with, while `recording`. */
const configured = [];
let recording = false;
class RecordingEncoder extends CustomVideoEncoder {
  static supports(codec) {
    return recording && codec === "avc";
  }
  init() {
    configured.push(this.config.codec);
    this.sent = false;
  }
  encode(sample) {
    // An AVCDecoderConfigurationRecord with a stub Constrained Baseline SPS and PPS.
    const description = new Uint8Array([
      1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0, 0x1e, 1, 0, 2,
      0x68, 0xce,
    ]);
    this.onPacket(
      new EncodedPacket(
        new Uint8Array([1, 2, 3]),
        "key",
        sample.timestamp,
        sample.duration,
      ),
      this.sent
        ? undefined
        : {
            decoderConfig: {
              codec: this.config.codec,
              codedWidth: 2,
              codedHeight: 2,
              description,
            },
          },
    );
    this.sent = true;
  }
  flush() {}
  close() {}
}
registerEncoder(RecordingEncoder);

/** Enough of a `VideoFrame` for mediabunny to wrap it in a sample. */
class TestVideoFrame {
  format = "RGBX";
  codedWidth = 2;
  codedHeight = 2;
  displayWidth = 2;
  displayHeight = 2;
  timestamp = 0;
  duration = null;
  colorSpace = {};
  visibleRect = { x: 0, y: 0, width: 2, height: 2 };
  allocationSize() {
    return 16;
  }
  async copyTo() {
    return [{ offset: 0, stride: 8 }];
  }
  clone() {
    return new TestVideoFrame();
  }
  close() {}
}
globalThis.VideoFrame = TestVideoFrame;

test("the sink opens its encoder with the chosen codec string and reports it", async (t) => {
  recording = true;
  t.after(() => {
    recording = false;
  });
  const sink = new MediabunnySink({
    container: "mp4",
    videoCodec: "avc",
    videoRateControl: rate(1_000_000),
    videoCodecString: "avc1.42e00a",
    videoLatencyMode: "quality",
    frameRate: { num: 30, den: 1 },
    destination: { kind: "memory", maxBytes: 1 << 20 },
  });
  assert.equal(sink.videoCodecString, undefined);
  for (let index = 0; index < 3; index += 1)
    await sink.writeVideo({
      index: BigInt(index),
      timeTicks: BigInt(index * 4_000),
      frame: new TestVideoFrame(),
    });
  await sink.finish();
  assert.deepEqual(configured, ["avc1.42e00a"]);
  assert.equal(sink.videoCodecString, "avc1.42e00a");
  assert.ok(sink.output.buffer.byteLength > 0);
});
