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
  MediabunnyVideoDecoder.open = async (id, file) => {
    const decoder = {
      id,
      file,
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
    { sourceId: "s", reason: "unsupported codec", frames: 2, permanent: true },
  ]);
});
