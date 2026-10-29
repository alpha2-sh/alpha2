// How the browser runtime keeps its video decoders: per source the engine asks for (a proxy stands
// in for its source under an ID of its own), and through a decoder's passing failures. The decoders
// are stand-ins for alpha2's mediabunny adapter, which Node cannot run.
import test from "node:test";
import assert from "node:assert/strict";
import { MediabunnyVideoDecoder } from "alpha2/mediabunny";
import { SourcePixels } from "../dist/browser/runtime/sources.js";

/**
 * Replaces the adapter's `open` for one test: each decoder opened is recorded, and `refill` answers
 * as `answer(decoder, request)` says (a frame stand-in by default).
 */
function fakeDecoders(t, answer = () => ({})) {
  const opened = [];
  const open = MediabunnyVideoDecoder.open;
  MediabunnyVideoDecoder.open = async (id, file, options = {}) => {
    const decoder = {
      id,
      file,
      options,
      closed: false,
      refills: 0,
      cachedFrameBytes: 0,
      reverseCacheHits: 0,
      reverseWindowsBuilt: 0,
      async refill(request) {
        decoder.refills += 1;
        const extra = await answer(decoder, request);
        return {
          frame: {
            timestamp: Number(request.mediaTimeTicks) / 0.12,
            duration: 33_333,
            displayWidth: 16,
            displayHeight: 16,
            close() {},
          },
          ...extra,
        };
      },
      close() {
        decoder.closed = true;
      },
    };
    opened.push(decoder);
    return decoder;
  };
  t.after(() => {
    MediabunnyVideoDecoder.open = open;
  });
  return opened;
}

/** One preview or export frame as the engine runs it: release, retain consumers, read. */
async function frame(
  pixels,
  layer,
  seconds,
  { sourceId, consumerId = "clip" },
) {
  await pixels.release(new Set([sourceId]));
  await pixels.retainConsumers(new Set([consumerId]));
  return pixels.pixelsFor(layer, seconds, {
    preferVideoFrame: true,
    consumerId,
    sourceId,
  });
}

test("a source read through its proxy keeps one decoder across frames, and starts over when it reads its original again", async (t) => {
  // Decoders are kept by engine source ID, not the runtime asset's (`proxy:<hash>` for a proxy), or
  // every frame would reopen the proxy's decoder.
  const opened = fakeDecoders(t);
  const pixels = new SourcePixels(null);
  const proxy = { id: "proxy:abc", kind: "video", file: { name: "proxy.mp4" } };
  for (let i = 0; i < 5; i += 1)
    await frame(pixels, proxy, i / 30, { sourceId: "clip-source" });
  assert.equal(opened.length, 1);
  assert.equal(opened[0].closed, false);
  assert.equal(pixels.diagnostics.liveDecoders, 1);
  const original = {
    id: "clip-source",
    kind: "video",
    file: { name: "original.mov" },
  };
  for (let i = 5; i < 8; i += 1)
    await frame(pixels, original, i / 30, { sourceId: "clip-source" });
  assert.equal(opened.length, 2);
  assert.equal(opened[0].closed, true);
  assert.equal(opened[1].file, original.file);
  assert.equal(pixels.diagnostics.liveDecoders, 1);
  await pixels.release(new Set());
  assert.equal(opened[1].closed, true);
  assert.equal(pixels.diagnostics.liveDecoders, 0);
});

test("a decoder read that fails is read again by a decoder opened again, and only frames that keep failing fall back for good", async (t) => {
  // One failed refill (a stall or decoder error) must not mark the cursor failed for the clip.
  let failing = 0;
  const opened = fakeDecoders(t, () => {
    if (failing > 0) {
      failing -= 1;
      throw new Error("the decoder stalled");
    }
    return {};
  });
  const pixels = new SourcePixels(null);
  // A worker's asset has no element to seek: a frame that falls back fails.
  const asset = { id: "s", kind: "video", file: { name: "s.mp4" } };
  const read = (seconds) => frame(pixels, asset, seconds, { sourceId: "s" });
  await read(0);
  failing = 1;
  const retried = await read(1 / 30);
  assert.equal(retried.kind, "video-frame");
  assert.equal(opened.length, 2);
  assert.equal(opened[0].closed, true);
  assert.deepEqual(pixels.decodeFallbacks, []);
  // A frame whose second read fails too falls back, and the next frame decodes again.
  failing = 2;
  await assert.rejects(read(2 / 30), /No worker decoder/);
  assert.equal((await read(3 / 30)).kind, "video-frame");
  assert.deepEqual(pixels.decodeFallbacks, [
    {
      kind: "element-seek",
      sourceId: "s",
      reason: "the decoder stalled",
      frames: 1,
      permanent: false,
    },
  ]);
  failing = 6;
  for (const seconds of [4, 5, 6])
    await assert.rejects(read(seconds / 30), /No worker decoder/);
  const opens = opened.length;
  await assert.rejects(read(7 / 30), /No worker decoder/);
  assert.equal(opened.length, opens, "no decoder opened once it gave up");
  assert.deepEqual(pixels.decodeFallbacks, [
    {
      kind: "element-seek",
      sourceId: "s",
      reason: "the decoder stalled",
      frames: 5,
      permanent: true,
    },
  ]);
  assert.equal(pixels.diagnostics.decodeFallbackFrames, 5);
});

test("a decoder that cannot open falls back for good at once", async (t) => {
  const open = MediabunnyVideoDecoder.open;
  let opens = 0;
  MediabunnyVideoDecoder.open = async () => {
    opens += 1;
    throw new Error("unsupported codec");
  };
  t.after(() => {
    MediabunnyVideoDecoder.open = open;
  });
  const pixels = new SourcePixels(null);
  const asset = { id: "s", kind: "video", file: { name: "s.mkv" } };
  for (const seconds of [0, 1 / 30])
    await assert.rejects(
      frame(pixels, asset, seconds, { sourceId: "s" }),
      /No worker decoder/,
    );
  assert.equal(opens, 1);
  assert.deepEqual(pixels.decodeFallbacks, [
    {
      kind: "element-seek",
      sourceId: "s",
      reason: "unsupported codec",
      frames: 2,
      permanent: true,
    },
  ]);
});

test("a frame asked for in software decodes on a cursor of its own, opened for software, beside the consumer's", async (t) => {
  const opened = fakeDecoders(t);
  const pixels = new SourcePixels(null);
  const asset = { id: "s", kind: "video", file: { name: "s.mp4" } };
  for (let i = 0; i < 3; i += 1)
    await frame(pixels, asset, i / 30, { sourceId: "s" });
  const software = (seconds) =>
    pixels.pixelsFor(asset, seconds, {
      preferVideoFrame: true,
      consumerId: "clip",
      sourceId: "s",
      decode: "software",
    });
  const decoded = await software(1 / 30);
  assert.equal(decoded.kind, "video-frame");
  assert.equal(decoded.physicalTimeMicroseconds, decoded.frame.timestamp);
  assert.equal(opened.length, 2);
  assert.deepEqual(opened[0].options, {});
  assert.deepEqual(opened[1].options, {
    hardwareAcceleration: "prefer-software",
  });
  assert.equal(opened[0].refills, 3, "the consumer's cursor did not move");
  await software(2 / 30);
  assert.equal(opened.length, 2, "the software cursor stays open");
  assert.equal(opened[1].refills, 2);
  await frame(pixels, asset, 3 / 30, { sourceId: "s" });
  assert.equal(opened[0].refills, 4);
  // The software cursor goes with its consumer.
  await pixels.retainConsumers(new Set());
  assert.ok(opened[0].closed && opened[1].closed);
  assert.equal(pixels.diagnostics.liveDecoders, 0);
});

test("the decoder budget counts consumers: a software cursor beside its consumer's is not counted again, one alone is", async (t) => {
  const opened = fakeDecoders(t);
  const pixels = new SourcePixels(null, { decoderLimit: 1 });
  const asset = { id: "s", kind: "video", file: { name: "s.mp4" } };
  const software = (consumerId) =>
    pixels.pixelsFor(asset, 0, {
      preferVideoFrame: true,
      consumerId,
      sourceId: "s",
      decode: "software",
    });
  await frame(pixels, asset, 0, { sourceId: "s" });
  await software("clip");
  await frame(pixels, asset, 1 / 30, { sourceId: "s" });
  assert.equal(opened.length, 2);
  // A second consumer whose frames all decode in software (its source decodes to opaque frames)
  // opens a cursor of its own, which the budget holds.
  await assert.rejects(
    software("other"),
    (error) => error.code === "resource_budget",
  );
  assert.equal(opened.length, 2);
});

test("a software decode that fails rejects, without an element seek, and opens again next time", async (t) => {
  let failing = 1;
  const opened = fakeDecoders(t, (decoder) => {
    if (decoder.options.hardwareAcceleration === "prefer-software" && failing) {
      failing -= 1;
      throw new Error("software decoding failed");
    }
    return {};
  });
  const pixels = new SourcePixels(null);
  const asset = {
    id: "s",
    kind: "video",
    file: { name: "s.mp4" },
    // An element to seek, which a software request must not use.
    video: {
      cloneNode() {
        throw new Error("no element seek for a software decode");
      },
    },
  };
  const software = () =>
    pixels.pixelsFor(asset, 0, {
      preferVideoFrame: true,
      consumerId: "clip",
      sourceId: "s",
      decode: "software",
    });
  await assert.rejects(software(), /software decoding failed/);
  assert.equal(opened[0].closed, true);
  assert.equal((await software()).kind, "video-frame");
  assert.equal(opened.length, 2);
  assert.deepEqual(pixels.decodeFallbacks, []);
});

test("videoDecode software opens every source for software, and one the browser cannot decode so opens as auto, reported once", async (t) => {
  const open = MediabunnyVideoDecoder.open;
  const opens = [];
  MediabunnyVideoDecoder.open = async (id, file, options = {}) => {
    opens.push({ id, options });
    if (id === "hevc" && options.hardwareAcceleration === "prefer-software")
      throw Object.assign(
        new Error("this browser has no software decoder for hvc1"),
        { code: "software_decode_unavailable" },
      );
    return {
      async refill(request) {
        return {
          frame: {
            timestamp: Number(request.mediaTimeTicks) / 0.12,
            duration: 33_333,
            displayWidth: 16,
            displayHeight: 16,
            close() {},
          },
        };
      },
      close() {},
    };
  };
  t.after(() => {
    MediabunnyVideoDecoder.open = open;
  });
  assert.throws(
    () => new SourcePixels(null, { videoDecode: "hardware" }),
    RangeError,
  );
  const pixels = new SourcePixels(null, { videoDecode: "software" });
  const avc = { id: "avc", kind: "video", file: { name: "a.mp4" } };
  const hevc = { id: "hevc", kind: "video", file: { name: "h.mp4" } };
  await frame(pixels, avc, 0, { sourceId: "avc", consumerId: "a" });
  for (const consumerId of ["h1", "h2"])
    await pixels.pixelsFor(hevc, 0, {
      preferVideoFrame: true,
      consumerId,
      sourceId: "hevc",
    });
  assert.deepEqual(
    opens.map(({ id, options }) => [id, options.hardwareAcceleration]),
    [
      ["avc", "prefer-software"],
      ["hevc", "prefer-software"],
      ["hevc", undefined],
      ["hevc", "prefer-software"],
      ["hevc", undefined],
    ],
  );
  assert.deepEqual(pixels.decodeFallbacks, [
    {
      kind: "software-unavailable",
      sourceId: "hevc",
      reason: "this browser has no software decoder for hvc1",
      frames: 0,
      permanent: false,
    },
  ]);
  assert.equal(pixels.diagnostics.decodeFallbackFrames, 0);
});
