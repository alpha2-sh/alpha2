import test from "node:test";
import assert from "node:assert/strict";

import {
  BufferTarget,
  CustomVideoDecoder,
  EncodedPacket,
  EncodedVideoPacketSource,
  Output,
  VideoSample,
  WebMOutputFormat,
  registerDecoder,
} from "mediabunny";

import { runExtractor } from "../dist/extract.js";

const FPS = 25;
const FRAMES = 6;
const WIDTH = 16;
const HEIGHT = 8;
const TICKS_PER_FRAME = 120_000 / FPS;
const LUMA_BYTES = WIDTH * HEIGHT;

const script = {};
function reset() {
  Object.assign(script, {
    custom: true,
    softwareListed: true,
    browserListed: true,
    softwareFailsFrom: undefined,
    browserFails: false,
    formats: { "prefer-software": "I420", "no-preference": "NV12" },
    copyFailures: 0,
    copyAttempts: 0,
    openFrames: 0,
    configs: [],
    asked: [],
  });
}

const PLANES = {
  I420: [
    { offset: 0, stride: WIDTH },
    { offset: LUMA_BYTES, stride: WIDTH / 2 },
    { offset: LUMA_BYTES * 1.25, stride: WIDTH / 2 },
  ],
  NV12: [
    { offset: 0, stride: WIDTH },
    { offset: LUMA_BYTES, stride: WIDTH },
  ],
};

class TestVideoFrame {
  #closed = false;

  constructor(source, init = {}) {
    const from = source instanceof TestVideoFrame ? source : init;
    this.format = from.format;
    this.codedWidth = from.codedWidth;
    this.codedHeight = from.codedHeight;
    this.timestamp = init.timestamp ?? source.timestamp;
    this.duration = init.duration ?? source.duration ?? null;
    this.displayWidth = this.codedWidth;
    this.displayHeight = this.codedHeight;
    this.visibleRect = {
      x: 0,
      y: 0,
      width: this.codedWidth,
      height: this.codedHeight,
    };
    this.colorSpace = {
      primaries: "bt709",
      transfer: "bt709",
      matrix: "bt709",
      fullRange: false,
      toJSON() {
        return { ...this };
      },
    };
    script.openFrames += 1;
  }

  clone() {
    return new TestVideoFrame(this);
  }

  allocationSize() {
    return (WIDTH * HEIGHT * 3) / 2;
  }

  async copyTo() {
    script.copyAttempts += 1;
    if (script.copyFailures > 0) {
      script.copyFailures -= 1;
      throw new DOMException("Cannot read the frame.", "InvalidStateError");
    }
    return PLANES[this.format];
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    script.openFrames -= 1;
  }
}

class ScriptedDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return script.custom && codec === "vp8";
  }
  init() {}
  decode(packet) {
    const acceleration = this.config.hardwareAcceleration;
    script.configs.push(acceleration);
    const software = acceleration === "prefer-software";
    if (
      software
        ? script.softwareFailsFrom !== undefined &&
          packet.data[1] >= script.softwareFailsFrom
        : script.browserFails
    ) {
      this.onError(
        new DOMException("Unsupported configuration.", "OperationError"),
      );
      return;
    }
    this.onSample(
      new VideoSample(
        new TestVideoFrame(
          { timestamp: 0 },
          {
            format: script.formats[acceleration],
            codedWidth: WIDTH,
            codedHeight: HEIGHT,
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
  isConfigSupported: async (config) => {
    script.asked.push(config.hardwareAcceleration);
    return {
      supported:
        config.hardwareAcceleration === "prefer-software"
          ? script.softwareListed
          : script.browserListed,
      config,
    };
  },
};
registerDecoder(ScriptedDecoder);

async function stream() {
  const output = new Output({
    format: new WebMOutputFormat(),
    target: new BufferTarget(),
  });
  const source = new EncodedVideoPacketSource("vp8");
  output.addVideoTrack(source, { frameRate: FPS });
  await output.start();
  for (let index = 0; index < FRAMES; index += 1) {
    const key = index === 0;
    const data = new Uint8Array([key ? 0 : 1, index, 0, 0]);
    await source.add(
      new EncodedPacket(data, key ? "key" : "delta", index / FPS, 1 / FPS),
      key
        ? {
            decoderConfig: {
              codec: "vp8",
              codedWidth: WIDTH,
              codedHeight: HEIGHT,
            },
          }
        : undefined,
    );
  }
  await output.finalize();
  return new Blob([output.target.buffer], { type: "video/webm" });
}

const blob = await stream();

function binding(needs, supplied) {
  return {
    ExtractSession: class {
      #needs = [...needs];
      context() {
        return JSON.stringify({
          range: { start: 0, end: 1 },
          passes: [],
          from: "source",
          video: null,
          audio: null,
        });
      }
      next() {
        const need = this.#needs.shift();
        return JSON.stringify(
          need === undefined ? { step: "done" } : { step: "need", need },
        );
      }
      supplyYuvFrame(pts, format) {
        supplied.push({ pts: Number(pts), format });
      }
      supplyEnd() {
        supplied.push("end");
      }
      emit() {}
      progress() {
        return "[]";
      }
      finish() {
        return "[]";
      }
      free() {}
    },
  };
}

const need = (from, frame, direction = "forward") => ({
  need: "source_frame",
  source: "v",
  from,
  time: frame * TICKS_PER_FRAME,
  direction,
});
const forwardPass = [
  need("start", 0),
  ...Array.from({ length: FRAMES }, (_, frame) => need("after", frame)),
];
const backwardPass = [
  need("start", FRAMES - 1, "backward"),
  ...Array.from({ length: FRAMES }, (_, index) =>
    need("after", FRAMES - 1 - index, "backward"),
  ),
];

async function run(needs = forwardPass) {
  const supplied = [];
  const result = await runExtractor(
    {
      binding: binding(needs, supplied),
      project: {},
      request: {
        extractor: "t",
        version: 1,
        input: { from: "source", source_id: "v", range: { start: 0, end: 1 } },
        outputs: {},
      },
      media: { v: blob },
    },
    {
      describe: () => ({
        id: "t",
        version: 1,
        input: { media: "video", from: ["source"], format: "luma8" },
        outputs: [],
      }),
    },
  );
  return { result, supplied };
}

const frames = (order, format) => [
  ...order.map((frame) => ({ pts: frame * TICKS_PER_FRAME, format })),
  "end",
];
const ascending = Array.from({ length: FRAMES }, (_, frame) => frame);

test("a source is decoded in software where software decodes it", async () => {
  reset();
  const { result, supplied } = await run();
  assert.deepEqual(supplied, frames(ascending, "I420"));
  assert.ok(script.configs.length > 0);
  assert.ok(script.configs.every((config) => config === "prefer-software"));
  assert.deepEqual(result.sourceDecode, {
    v: { hardwareAcceleration: "prefer-software", frameFormat: "I420" },
  });
  assert.equal(script.openFrames, 0);
});

test("a software decoder that fails its first frame is replaced by the browser's own, from the same frame", async () => {
  reset();
  script.softwareFailsFrom = 0;
  const { result, supplied } = await run();
  assert.deepEqual(supplied, frames(ascending, "NV12"));
  assert.equal(script.configs[0], "prefer-software");
  assert.ok(
    script.configs.slice(1).every((config) => config === "no-preference"),
  );
  assert.deepEqual(result.sourceDecode.v, {
    hardwareAcceleration: "no-preference",
    softwareRefusal: "OperationError: Unsupported configuration.",
    frameFormat: "NV12",
  });
  assert.equal(script.openFrames, 0);
});

test("a codec with no software decoder listed opens with the browser's own decoder", async () => {
  reset();
  script.softwareListed = false;
  const { result, supplied } = await run();
  assert.deepEqual(supplied, frames(ascending, "NV12"));
  assert.ok(script.asked.includes("prefer-software"));
  assert.ok(!script.configs.includes("prefer-software"));
  assert.equal(
    result.sourceDecode.v.softwareRefusal,
    "no software decoder for VP8",
  );
  assert.equal(script.openFrames, 0);
});

test("a source neither decoder reads is refused as undecodable, with the browser's error", async () => {
  reset();
  script.softwareFailsFrom = 0;
  script.browserFails = true;
  await assert.rejects(run(), (error) => {
    assert.equal(error.code, "DECODE_UNAVAILABLE");
    assert.equal(error.facts.kind, "video");
    assert.equal(
      error.facts.browserError,
      "OperationError: Unsupported configuration.",
    );
    assert.equal(error.cause.name, "OperationError");
    assert.ok(error.message.startsWith("source v"), error.message);
    return true;
  });
  assert.deepEqual(
    new Set(script.configs),
    new Set(["prefer-software", "no-preference"]),
  );
  assert.equal(script.openFrames, 0);
});

test("a track this browser cannot decode is refused before a decoder opens", async () => {
  reset();
  script.custom = false;
  script.browserListed = false;
  await assert.rejects(run(), { code: "DECODE_UNAVAILABLE" });
  assert.deepEqual(script.configs, []);
  assert.equal(script.openFrames, 0);
});

test("a backward pass falls back the same way", async () => {
  reset();
  script.softwareFailsFrom = 0;
  const { supplied } = await run(backwardPass);
  assert.deepEqual(supplied, frames(ascending.toReversed(), "NV12"));
  assert.equal(script.openFrames, 0);
});

test("a software decoder that fails after its first frame fails the run with the browser's error", async () => {
  reset();
  script.softwareFailsFrom = 3;
  await assert.rejects(run(), (error) => {
    assert.ok(error instanceof DOMException);
    assert.equal(error.name, "OperationError");
    assert.notEqual(typeof error.code, "string");
    return true;
  });
  assert.ok(!script.configs.includes("no-preference"));
  assert.equal(script.openFrames, 0);
});

test("a frame the browser cannot read back at first is read again", async () => {
  reset();
  script.copyFailures = 2;
  const { supplied } = await run();
  assert.deepEqual(supplied, frames(ascending, "I420"));
  assert.equal(script.copyAttempts, FRAMES + 2);
  assert.equal(script.openFrames, 0);
});

test("a frame the browser can never read back fails the run, coded, with the cause", async () => {
  reset();
  script.copyFailures = 3;
  await assert.rejects(run(), (error) => {
    assert.equal(error.code, "FRAME_UNREADABLE");
    assert.equal(error.cause.name, "InvalidStateError");
    assert.match(error.message, /source v at 0$/);
    return true;
  });
  assert.equal(script.openFrames, 0);
});

test("frames without planes are refused, naming what the decoder gave", async () => {
  reset();
  script.softwareListed = false;
  script.formats["no-preference"] = "RGBA";
  await assert.rejects(run(), (error) => {
    assert.equal(error.code, "PLANAR_DECODE_UNAVAILABLE");
    assert.equal(error.facts.frameFormat, "RGBA");
    assert.equal(error.facts.softwareDecoder, "none");
    assert.equal(error.cause.code, "NON_PLANAR_FRAME");
    return true;
  });
  assert.equal(script.openFrames, 0);
});
