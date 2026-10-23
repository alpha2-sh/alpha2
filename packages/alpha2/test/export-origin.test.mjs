// An export's zero is its first scheduled frame, as on every host: a range from mid-timeline is
// written as a file starting at 0, where its audio (`audioWindow`) also starts.
import assert from "node:assert/strict";
import test from "node:test";

import {
  ALL_FORMATS,
  BufferSource,
  CustomVideoEncoder,
  EncodedPacket,
  Input,
  registerEncoder,
} from "mediabunny";

import { Alpha2Exporter, exportToSink } from "../dist/exporter.js";
import { MediabunnySink } from "../dist/mediabunny.js";

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

/** Encodes each sample as a three-byte keyframe at the sample's own time. */
class StampEncoder extends CustomVideoEncoder {
  static supports() {
    return true;
  }
  init() {
    this.configured = false;
  }
  encode(sample) {
    const meta = this.configured
      ? undefined
      : {
          decoderConfig: {
            codec: this.config.codec,
            codedWidth: 2,
            codedHeight: 2,
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

globalThis.VideoFrame = TestVideoFrame;
registerEncoder(StampEncoder);

const backend = (requested) => ({
  begin: async (timeTicks) => {
    requested.push(timeTicks);
    return {
      kind: "rendered",
      retainedBytes: 16,
      readback: Promise.resolve(new TestVideoFrame()),
    };
  },
  isDeviceLost: () => false,
  abort() {},
});

test("exported frames count from the output's zero; the backend renders timeline times", async () => {
  const requested = [];
  // 1.0083 s is inside the frame starting at 1 s (frame 30 at 30 fps).
  const exporter = new Alpha2Exporter(
    backend(requested),
    { num: 30n, den: 1n },
    121_000n,
    132_000n,
  );
  assert.deepEqual(exporter.effectiveRange, {
    startTicks: 120_000n,
    endTicks: 132_000n,
  });
  const frames = [];
  for (;;) {
    const step = await exporter.next();
    if (step.kind === "complete") break;
    frames.push(await step.output);
  }
  assert.deepEqual(requested, [120_000n, 124_000n, 128_000n]);
  assert.deepEqual(
    frames.map(({ index, timeTicks }) => [index, timeTicks]),
    [
      [0n, 0n],
      [1n, 4_000n],
      [2n, 8_000n],
    ],
  );
});

test("a range from the middle of the timeline is a file that starts at 0", async () => {
  const sink = new MediabunnySink({
    container: "mp4",
    videoCodec: "vp9",
    videoRateControl: { bitrate: 1_000_000, bitrateMode: "variable" },
    frameRate: { num: 30, den: 1 },
    destination: { kind: "memory", maxBytes: 1 << 20 },
  });
  const exporter = new Alpha2Exporter(
    backend([]),
    { num: 30n, den: 1n },
    1_200_000n,
    1_320_000n,
  );
  const result = await exportToSink(exporter, sink, async () => {});
  assert.equal(result.frames, 30);
  const input = new Input({
    source: new BufferSource(sink.output.buffer),
    formats: ALL_FORMATS,
  });
  const track = await input.getPrimaryVideoTrack();
  assert.equal(await track.getFirstTimestamp(), 0);
  assert.equal(await input.computeDuration(), 1);
});
