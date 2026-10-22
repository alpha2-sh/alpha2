// Where the engine copies frames through a canvas, frames go to it as bitmaps instead.
import assert from "node:assert/strict";
import test from "node:test";

import { FrameSupply } from "../dist/frame-supply.js";
import { uploadPresentedFrame } from "../dist/presented-frames.js";

/** Enough of a decoded `VideoFrame` for the upload. */
function fakeFrame({
  format = "BGRX",
  displayWidth = 8,
  displayHeight = 4,
} = {}) {
  const frame = {
    format,
    codedWidth: 8,
    codedHeight: 4,
    displayWidth,
    displayHeight,
    timestamp: 0,
    duration: 33_333,
    visibleRect: { x: 0, y: 0, width: 8, height: 4 },
    colorSpace: {},
    closed: false,
    close() {
      frame.closed = true;
    },
  };
  return frame;
}

/** An engine that records which upload each frame took. */
function fakeEngine(videoFrameCopy, { bitmaps = true } = {}) {
  const calls = [];
  const receipt = () => ({
    generation: BigInt(calls.length),
    retainedBytes: 32,
  });
  return {
    calls,
    deviceLost: false,
    supportsExternalVideoFrame: true,
    videoFrameCopy,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "c", sourceId: "video", mediaTimeTicks: "0" }]),
    uploadPresentedVideoFrame(sourceId, ticks, frame, ...placement) {
      calls.push({ kind: "frame", sourceId, ticks, frame, placement });
      frame.close();
      return receipt();
    },
    ...(bitmaps
      ? {
          uploadPresentedImageBitmap(sourceId, ticks, bitmap, ...placement) {
            calls.push({ kind: "bitmap", sourceId, ticks, bitmap, placement });
            bitmap.close();
            return receipt();
          },
        }
      : {}),
    aliasVideoFrame: () => false,
  };
}

/** Replaces `createImageBitmap` for the length of `run`, recording what it was asked. */
async function withImageBitmaps(make, run) {
  const asked = [];
  const original = globalThis.createImageBitmap;
  globalThis.createImageBitmap = async (source, options) => {
    asked.push({ source, options });
    return make(source, options);
  };
  try {
    return await run(asked);
  } finally {
    if (original === undefined) delete globalThis.createImageBitmap;
    else globalThis.createImageBitmap = original;
  }
}

const bitmapOf = (source, options) => ({
  width: options.resizeWidth,
  height: options.resizeHeight,
  closed: false,
  close() {
    this.closed = true;
  },
});

test("an engine that draws frames on a canvas is sent a bitmap of each, at its display size", async () => {
  await withImageBitmaps(bitmapOf, async (asked) => {
    const engine = fakeEngine("bitmap");
    const frame = fakeFrame({ displayWidth: 10 });
    await uploadPresentedFrame(engine, "video", 5n, frame, 64, 48, 90, true);
    assert.equal(asked.length, 1);
    assert.equal(asked[0].source, frame);
    assert.deepEqual(asked[0].options, {
      premultiplyAlpha: "none",
      resizeWidth: 10,
      resizeHeight: 4,
    });
    assert.equal(
      frame.closed,
      true,
      "the frame is let go once its bitmap is made",
    );
    assert.deepEqual(
      engine.calls.map(({ kind, sourceId, ticks, placement }) => [
        kind,
        sourceId,
        ticks,
        placement,
      ]),
      [["bitmap", "video", 5n, [64, 48, 90, true]]],
    );
    assert.equal(engine.calls[0].bitmap.closed, true);
  });
});

test("an engine that takes frames directly, or has not tried one, is sent the frame", async () => {
  await withImageBitmaps(bitmapOf, async (asked) => {
    for (const mode of ["direct", "untried", undefined]) {
      const engine = fakeEngine(mode);
      const frame = fakeFrame();
      await uploadPresentedFrame(engine, "video", 0n, frame, 64, 48, 0, false);
      assert.deepEqual(
        engine.calls.map(({ kind }) => kind),
        ["frame"],
        String(mode),
      );
      assert.equal(engine.calls[0].frame, frame);
    }
    assert.equal(asked.length, 0);
  });
});

test("a frame goes as it is when no bitmap can be made or the engine takes none", async () => {
  await withImageBitmaps(
    () => {
      throw new DOMException("the frame cannot be read", "InvalidStateError");
    },
    async () => {
      const engine = fakeEngine("bitmap");
      const frame = fakeFrame();
      await uploadPresentedFrame(engine, "video", 0n, frame, 64, 48, 0, false);
      assert.deepEqual(
        engine.calls.map(({ kind }) => kind),
        ["frame"],
      );
      assert.equal(engine.calls[0].frame, frame);
    },
  );
  await withImageBitmaps(bitmapOf, async (asked) => {
    const engine = fakeEngine("bitmap", { bitmaps: false });
    await uploadPresentedFrame(
      engine,
      "video",
      0n,
      fakeFrame(),
      64,
      48,
      0,
      false,
    );
    assert.deepEqual(
      engine.calls.map(({ kind }) => kind),
      ["frame"],
    );
    assert.equal(asked.length, 0);
  });
});

test("the frame supply sends bitmaps where the engine draws frames on a canvas", async () => {
  await withImageBitmaps(bitmapOf, async (asked) => {
    const engine = fakeEngine("bitmap");
    const frame = fakeFrame();
    const supply = new FrameSupply(engine);
    const supplied = await supply.supply(0n, {
      width: 64,
      height: 48,
      liveSourceIds: new Set(["video"]),
      getLayer: () => ({ id: "video", kind: "video" }),
      pixelsFor: async () => ({
        kind: "video-frame",
        frame,
        rotation: 180,
        physicalTimeMicroseconds: 0,
      }),
      isCurrent: () => true,
    });
    assert.equal(asked.length, 1);
    assert.equal(frame.closed, true);
    assert.deepEqual(
      engine.calls.map(({ kind, placement }) => [kind, placement]),
      [["bitmap", [64, 48, 180, false]]],
    );
    assert.equal(supplied.uploaded.length, 1);
  });
});
