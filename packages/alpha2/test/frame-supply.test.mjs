import assert from "node:assert/strict";
import test from "node:test";
import { runInNewContext } from "node:vm";

import { FrameSupply, decoderCursorKey } from "../dist/frame-supply.js";
import {
  chromaSitingForCodec,
  copyPlanarYuvFrame,
  NonPlanarVideoFrameError,
  PlanarDecodeUnavailableError,
} from "../dist/hdr-frames.js";

test("independent decoder cursors run concurrently and upload in need order", async () => {
  const needs = [
    { clipId: "a", sourceId: "video", mediaTimeTicks: "0" },
    { clipId: "b", sourceId: "video", mediaTimeTicks: "1000" },
    { clipId: "a", sourceId: "video", mediaTimeTicks: "2000" },
    { clipId: "c", sourceId: "video", mediaTimeTicks: "3000" },
  ];
  const uploads = [];
  const perCursor = new Map();
  let active = 0;
  let peak = 0;
  let generation = 0n;
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () => JSON.stringify(needs),
    uploadRgbaFrame(_sourceId, sourceTicks) {
      uploads.push(sourceTicks);
      generation += 1n;
      return { generation, retainedBytes: 16 };
    },
    aliasVideoFrame() {
      return false;
    },
  };
  const supply = new FrameSupply(engine, { maxDecodeConcurrency: 2 });
  await supply.supply(0n, {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["video"]),
    getLayer: () => ({ id: "video", kind: "video" }),
    pixelsFor: async (_layer, seconds, { consumerId }) => {
      const cursorActive = perCursor.get(consumerId) ?? 0;
      assert.equal(cursorActive, 0, "one mutable cursor must stay sequential");
      perCursor.set(consumerId, cursorActive + 1);
      active += 1;
      peak = Math.max(peak, active);
      await new Promise((resolve) => setTimeout(resolve, 5));
      active -= 1;
      perCursor.set(consumerId, 0);
      return {
        kind: "rgba-frame",
        pixels: new Uint8Array(16),
        physicalTimeMicroseconds: Math.round(seconds * 1_000_000),
        sourceWidth: 2,
        sourceHeight: 2,
      };
    },
    isCurrent: () => true,
  });
  assert.equal(peak, 2);
  assert.deepEqual(uploads, [0n, 1_000n, 2_000n, 3_000n]);
});

test("overlapping clips sharing a source keep independent decoder consumers, each told its source", async () => {
  const needs = [
    { clipId: "clip-late", sourceId: "shared", mediaTimeTicks: "3600000" },
    { clipId: "clip-early", sourceId: "shared", mediaTimeTicks: "0" },
  ];
  const uploads = [];
  const consumers = [];
  const sources = [];
  let retained;
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () => JSON.stringify(needs),
    uploadRgbaFrame(sourceId, sourceTicks) {
      uploads.push([sourceId, sourceTicks]);
      return {
        generation: BigInt(uploads.length),
        retainedBytes: 16,
      };
    },
    aliasVideoFrame() {
      return false;
    },
  };
  const supply = new FrameSupply(engine);

  const result = await supply.supply(0n, {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["shared"]),
    // A stand-in for the source, as a proxy is: the host still learns which source it decodes.
    getLayer: () => ({ id: "proxy:shared", kind: "video" }),
    pixelsFor: async (_layer, seconds, options) => {
      consumers.push(options.consumerId);
      sources.push(options.sourceId);
      return {
        kind: "rgba-frame",
        pixels: new Uint8Array(16),
        physicalTimeMicroseconds: Math.round(seconds * 1_000_000),
        sourceWidth: 2,
        sourceHeight: 2,
      };
    },
    retainVideoConsumers: async (ids) => {
      retained = ids;
    },
    isCurrent: () => true,
  });

  assert.deepEqual(consumers, ["clip-late", "clip-early"]);
  assert.deepEqual(sources, ["shared", "shared"]);
  assert.deepEqual([...retained], ["clip-late", "clip-early"]);
  assert.deepEqual(uploads, [
    ["shared", 3_600_000n],
    ["shared", 0n],
  ]);
  assert.deepEqual(result.uploaded, [
    { generation: 1n, retainedBytes: 16 },
    { generation: 2n, retainedBytes: 16 },
  ]);
});

test("composition steps reading one source keep their own decoder consumers", async () => {
  const needs = [
    { clipId: "clip", readerId: null, sourceId: "shared", mediaTimeTicks: "0" },
    {
      clipId: "clip",
      readerId: "early",
      sourceId: "shared",
      mediaTimeTicks: "120000",
    },
    {
      clipId: "clip",
      readerId: "late",
      sourceId: "shared",
      mediaTimeTicks: "4800000",
    },
  ];
  const consumers = [];
  let retained;
  let generation = 0n;
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () => JSON.stringify(needs),
    uploadRgbaFrame() {
      generation += 1n;
      return { generation, retainedBytes: 16 };
    },
    aliasVideoFrame() {
      return false;
    },
  };
  await new FrameSupply(engine).supply(0n, {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["shared"]),
    getLayer: () => ({ id: "shared", kind: "video" }),
    pixelsFor: async (_layer, seconds, options) => {
      consumers.push(options.consumerId);
      return {
        kind: "rgba-frame",
        pixels: new Uint8Array(16),
        physicalTimeMicroseconds: Math.round(seconds * 1_000_000),
        sourceWidth: 2,
        sourceHeight: 2,
      };
    },
    retainVideoConsumers: async (ids) => {
      retained = ids;
    },
    isCurrent: () => true,
  });
  assert.equal(
    new Set(consumers).size,
    3,
    "clip, early, and late each read alone",
  );
  assert.equal(consumers[0], "clip");
  assert.deepEqual([...retained], consumers);
});

test("distinct exact requests alias one physical decoded frame", async () => {
  let requestedTicks = "0";
  let generation = 0n;
  const uploads = [];
  const aliases = [];
  const released = [];
  const live = new Set();
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([
        { clipId: "clip", sourceId: "video", mediaTimeTicks: requestedTicks },
      ]),
    uploadRgbaFrame(sourceId, sourceTicks) {
      uploads.push([sourceId, sourceTicks]);
      live.add(`${sourceId}:${sourceTicks}`);
      generation += 1n;
      return { generation, retainedBytes: 16 };
    },
    aliasVideoFrame(sourceId, existingTicks, nextTicks) {
      aliases.push([sourceId, existingTicks, nextTicks]);
      if (!live.has(`${sourceId}:${existingTicks}`)) return false;
      live.add(`${sourceId}:${nextTicks}`);
      return true;
    },
    releaseFrameGeneration() {},
    releaseSourceFrames(sourceId) {
      released.push(sourceId);
      for (const key of live)
        if (key.startsWith(`${sourceId}:`)) live.delete(key);
    },
  };
  const supply = new FrameSupply(engine);
  const options = {
    width: 2,
    height: 2,
    contentOf: () => "first media",
    liveSourceIds: new Set(["video"]),
    getLayer: () => ({ id: "video", kind: "video" }),
    pixelsFor: async () => ({
      kind: "rgba-frame",
      pixels: new Uint8Array(16),
      physicalTimeMicroseconds: 33_333,
      durationMicroseconds: 33_333,
      sourceWidth: 2,
      sourceHeight: 2,
    }),
    isCurrent: () => true,
  };

  const first = await supply.supply(0n, options);
  requestedTicks = "2000";
  const second = await supply.supply(2_000n, options);
  requestedTicks = "4000";
  // A relink: the same source ID, other media.
  const relinked = await supply.supply(4_000n, {
    ...options,
    contentOf: () => "relinked media",
  });

  assert.deepEqual(uploads, [
    ["video", 0n],
    ["video", 4_000n],
  ]);
  assert.deepEqual(aliases, [["video", 0n, 2_000n]]);
  assert.equal(first.uploaded.length, 1);
  assert.deepEqual(second.uploaded, []);
  assert.equal(relinked.uploaded.length, 1);
  assert.deepEqual(released, ["video"]);
});

test("replacing an exact key invalidates its previous physical identity", async () => {
  let requestedTicks = "0";
  let physicalTimeMicroseconds = 1_000;
  let generation = 0n;
  const uploads = [];
  const aliases = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([
        { clipId: "clip", sourceId: "video", mediaTimeTicks: requestedTicks },
      ]),
    uploadRgbaFrame(_sourceId, sourceTicks) {
      uploads.push(sourceTicks);
      generation += 1n;
      return { generation, retainedBytes: 16 };
    },
    aliasVideoFrame(_sourceId, existingTicks, nextTicks) {
      aliases.push([existingTicks, nextTicks]);
      return true;
    },
    releaseFrameGeneration() {},
  };
  const supply = new FrameSupply(engine);
  const options = {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["video"]),
    getLayer: () => ({ id: "video", kind: "video" }),
    pixelsFor: async () => ({
      kind: "rgba-frame",
      pixels: new Uint8Array(16),
      physicalTimeMicroseconds,
      sourceWidth: 2,
      sourceHeight: 2,
    }),
    isCurrent: () => true,
  };

  await supply.supply(0n, options);
  physicalTimeMicroseconds = 2_000;
  await supply.supply(0n, options);
  requestedTicks = "2000";
  physicalTimeMicroseconds = 1_000;
  await supply.supply(2_000n, options);

  assert.deepEqual(uploads, [0n, 0n, 2_000n]);
  assert.deepEqual(aliases, []);
});

test("decoder cursor keys distinguish clips that share one asset", () => {
  assert.notEqual(
    decoderCursorKey("shared", "clip-late"),
    decoderCursorKey("shared", "clip-early"),
  );
  assert.equal(
    decoderCursorKey("shared", "clip-late"),
    decoderCursorKey("shared", "clip-late"),
  );
});

test("retired upload generations are bounded by physical texture bytes", () => {
  const released = [];
  const engine = {
    releaseFrameGeneration(generation) {
      released.push(generation);
    },
  };
  const supply = new FrameSupply(engine, { maxRetiredBytes: 20 });

  supply.retire([
    { generation: 1n, retainedBytes: 10 },
    { generation: 2n, retainedBytes: 10 },
    { generation: 3n, retainedBytes: 10 },
  ]);
  assert.deepEqual(released, [1n]);
  assert.equal(supply.retiredBytes, 20);

  supply.retire([{ generation: 4n, retainedBytes: 30 }]);
  assert.deepEqual(released, [1n, 2n, 3n]);
  assert.equal(
    supply.retiredBytes,
    30,
    "one oversize current generation remains live",
  );
});

test("cached images evict least-recently-used inactive textures by bytes", async () => {
  const released = [];
  const uploaded = [];
  const pixels = new Map([
    ["image-a", new Uint8Array(16)],
    ["image-b", new Uint8Array(16)],
  ]);
  let sourceId = "image-a";
  let generation = 0n;
  const held = new Map();
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([{ clipId: sourceId, sourceId, mediaTimeTicks: "0" }]),
    uploadRgbaImage(id) {
      uploaded.push(id);
      generation += 1n;
      held.set(id, generation);
      return { generation, retainedBytes: 15 };
    },
    holdsImage: (id, value) => held.get(id) === value,
    releaseFrameGeneration(value) {
      released.push(value);
      for (const [id, live] of held) if (live === value) held.delete(id);
    },
  };
  const supply = new FrameSupply(engine, {
    maxCachedImageBytes: 20,
    maxRetiredBytes: 1,
  });
  const options = {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["image-a", "image-b"]),
    getLayer: (id) => ({ id, kind: "image" }),
    pixelsFor: async ({ id }) => pixels.get(id),
    isCurrent: () => true,
  };

  // Each frame's receipts are retired once it is rendered; the stills a supply evicted go too.
  const frame = async () =>
    supply.retire((await supply.supply(0n, options)).uploaded);
  await frame();
  sourceId = "image-b";
  await frame();
  assert.equal(supply.cachedImageBytes, 15);
  sourceId = "image-a";
  await frame();

  assert.deepEqual(uploaded, ["image-a", "image-b", "image-a"]);
  assert.equal(supply.cachedImageBytes, 15);
  assert.deepEqual(released, [1n]);
});

/**
 * An engine that keeps uploads as the wasm frame store does
 * (`bindings/alpha2-wasm/src/frame_store.rs`): every upload is a new generation, stills are limited
 * by count and, with frames, by bytes, and the oldest generation goes first, whether or not it is
 * still shown. Releasing a generation drops it even while it is live.
 */
function frameStoreEngine({ needsAt, maxImages = 128, budget = Infinity }) {
  let next = 0n;
  const frames = new Map();
  const images = new Map();
  const live = () => [...frames.values(), ...images.values()];
  const release = (generation) => {
    for (const map of [frames, images])
      for (const [key, entry] of map)
        if (entry.generation === generation) map.delete(key);
  };
  const insert = (map, key, bytes) => {
    next += 1n;
    const generation = next;
    map.set(key, { generation, bytes });
    for (;;) {
      const sizes = new Map(
        live().map((entry) => [entry.generation, entry.bytes]),
      );
      const total = [...sizes.values()].reduce((sum, value) => sum + value, 0);
      const older = [...sizes.keys()].filter((value) => value !== generation);
      if (total <= budget || older.length === 0) break;
      release(older.reduce((a, b) => (a < b ? a : b)));
    }
    return { generation, retainedBytes: bytes };
  };
  const engine = {
    uploads: [],
    releasedSources: [],
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: (ticks) => JSON.stringify(needsAt(ticks)),
    uploadRgbaImage(id, width, height) {
      engine.uploads.push(id);
      if (!images.has(id) && images.size >= maxImages) {
        const [oldest] = [...images].reduce((a, b) =>
          a[1].generation < b[1].generation ? a : b,
        );
        images.delete(oldest);
      }
      return insert(images, id, width * height * 4);
    },
    uploadRgbaFrame(id, ticks, width, height) {
      engine.uploads.push(id);
      return insert(frames, `${id}@${ticks}`, width * height * 4);
    },
    aliasVideoFrame(id, existing, requested) {
      const entry = frames.get(`${id}@${existing}`);
      if (entry === undefined) return false;
      frames.set(`${id}@${requested}`, entry);
      return true;
    },
    holdsImage: (id, generation) => images.get(id)?.generation === generation,
    releaseFrameGeneration: release,
    releaseSourceFrames(id) {
      engine.releasedSources.push(id);
      images.delete(id);
      for (const key of frames.keys())
        if (key.startsWith(`${id}@`)) frames.delete(key);
    },
    renderAt(ticks) {
      const missingSources = needsAt(ticks)
        .filter(
          ({ sourceId, mediaTimeTicks }) =>
            !images.has(sourceId) &&
            !frames.has(`${sourceId}@${mediaTimeTicks}`),
        )
        .map(({ sourceId }) => sourceId);
      return JSON.stringify({
        requestedTimeTicks: Number(ticks),
        presentedTimeTicks: missingSources.length ? null : Number(ticks),
        missingSources,
        repeated: false,
      });
    },
  };
  return engine;
}

const need = (sourceId, mediaTimeTicks = "0") => ({
  clipId: sourceId,
  sourceId,
  mediaTimeTicks,
});

test("a still the engine dropped by its image limit is uploaded again", async () => {
  // The engine keeps 128 stills and drops the oldest upload, the logo, under 200 photos.
  const engine = frameStoreEngine({
    needsAt: (ticks) => [need("logo"), need(`photo-${ticks}`)],
  });
  const ids = ["logo", ...Array.from({ length: 200 }, (_, i) => `photo-${i}`)];
  // Pixels that stay the same object, as a host that caches its decoded stills returns them.
  const pixels = new Map(ids.map((id) => [id, new Uint8Array(4 * 4 * 4)]));
  const supply = new FrameSupply(engine);
  const options = {
    width: 4,
    height: 4,
    liveSourceIds: new Set(ids),
    getLayer: (id) => ({ id, kind: "image" }),
    pixelsFor: async ({ id }) => pixels.get(id),
    isCurrent: () => true,
  };
  for (let ticks = 0n; ticks < 200n; ticks += 1n) {
    const shown = await supply.render(ticks, options);
    assert.deepEqual(shown.missingSources, [], `timestamp ${ticks}`);
  }
  assert.ok(
    engine.uploads.filter((id) => id === "logo").length > 1,
    "the dropped logo is uploaded again",
  );
});

test("a still the engine dropped by its byte budget is uploaded again", async () => {
  // A budget below what the page retains: new video frames push the logo's generation out.
  const engine = frameStoreEngine({
    budget: 4 * (4 * 4 * 4),
    needsAt: (ticks) => [need("logo"), need("video", String(ticks))],
  });
  const logo = new Uint8Array(4 * 4 * 4);
  const supply = new FrameSupply(engine);
  const options = {
    width: 4,
    height: 4,
    liveSourceIds: new Set(["logo", "video"]),
    getLayer: (id) => ({ id, kind: id === "logo" ? "image" : "video" }),
    pixelsFor: async ({ id }, seconds) =>
      id === "logo"
        ? logo
        : {
            kind: "rgba-frame",
            pixels: new Uint8Array(4 * 4 * 4),
            physicalTimeMicroseconds: Math.round(seconds * 1_000_000),
            sourceWidth: 4,
            sourceHeight: 4,
          },
    isCurrent: () => true,
  };
  for (let ticks = 0n; ticks < 12_000n; ticks += 1_000n) {
    const shown = await supply.render(ticks, options);
    assert.deepEqual(shown.missingSources, [], `timestamp ${ticks}`);
  }
});

test("a still decoded into another realm's typed array is uploaded as RGBA", async () => {
  const engine = frameStoreEngine({ needsAt: () => [need("logo")] });
  const supply = new FrameSupply(engine);
  const pixels = runInNewContext("new Uint8Array(16)");
  assert.equal(pixels instanceof Uint8Array, false);
  const shown = await supply.render(0n, {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["logo"]),
    getLayer: (id) => ({ id, kind: "image" }),
    pixelsFor: async () => pixels,
    isCurrent: () => true,
  });
  assert.deepEqual(shown.missingSources, []);
  assert.deepEqual(engine.uploads, ["logo"]);
});

test("a source whose content changes loses its uploads; the others keep theirs", async () => {
  const engine = frameStoreEngine({
    needsAt: () => [need("logo"), need("video"), need("relinked")],
  });
  const media = { logo: "logo", video: "video", relinked: "first take" };
  const stills = new Map([
    ["logo", new Uint8Array(16)],
    ["relinked", new Uint8Array(16)],
  ]);
  const supply = new FrameSupply(engine);
  const options = {
    width: 2,
    height: 2,
    contentOf: (id) => media[id],
    liveSourceIds: new Set(Object.keys(media)),
    getLayer: (id) => ({ id, kind: id === "video" ? "video" : "image" }),
    pixelsFor: async ({ id }) =>
      stills.get(id) ?? {
        kind: "rgba-frame",
        pixels: new Uint8Array(16),
        physicalTimeMicroseconds: 0,
        sourceWidth: 2,
        sourceHeight: 2,
      },
    isCurrent: () => true,
  };
  await supply.render(0n, options);
  assert.deepEqual(engine.uploads, ["logo", "video", "relinked"]);
  // An edit that changes no media: nothing is uploaded again.
  await supply.render(0n, options);
  assert.deepEqual(engine.uploads, ["logo", "video", "relinked"]);
  assert.deepEqual(engine.releasedSources, []);
  // A relink keeps the ID; the pixels the host decodes for it are new, and only it goes.
  media.relinked = "second take";
  stills.set("relinked", new Uint8Array(16));
  const shown = await supply.render(0n, options);
  assert.deepEqual(shown.missingSources, []);
  assert.deepEqual(engine.uploads, ["logo", "video", "relinked", "relinked"]);
  assert.deepEqual(engine.releasedSources, ["relinked"]);
});

/** A CPU-backed 10-bit frame in the shape WebCodecs hands out. */
function fakeHdrFrame({ transfer, timestamp, displayWidth = 4 }) {
  const planes = [
    { offset: 0, stride: 8 },
    { offset: 16, stride: 4 },
    { offset: 20, stride: 4 },
  ];
  const frame = {
    format: "I420P10",
    codedWidth: 4,
    codedHeight: 2,
    displayWidth,
    displayHeight: 2,
    timestamp,
    duration: 40_000,
    visibleRect: { x: 0, y: 0, width: 4, height: 2 },
    colorSpace: {
      primaries: "bt2020",
      transfer,
      matrix: "bt2020-ncl",
      fullRange: false,
    },
    closed: false,
    allocationSize: () => 24,
    async copyTo(buffer) {
      buffer.fill(7);
      return planes;
    },
    close() {
      frame.closed = true;
    },
  };
  return frame;
}

/** `frame` as a hardware frame the browser does not map to the CPU: no format, and no planes. */
function opaque(frame) {
  frame.format = null;
  frame.copyTo = async () => {
    throw new Error("an opaque frame has no planes to copy");
  };
  return frame;
}

test("HDR frames reach the engine as planar YUV with their color, never through the browser", async () => {
  const needs = [
    { clipId: "a", sourceId: "hdr", mediaTimeTicks: "0" },
    { clipId: "b", sourceId: "hdr", mediaTimeTicks: "1000" },
  ];
  const yuv = [];
  const frames = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () => JSON.stringify(needs),
    uploadPresentedVideoFrame() {
      throw new Error("an HDR frame must not take the browser's conversion");
    },
    uploadYuvFrame(...args) {
      yuv.push(args);
      return { generation: BigInt(yuv.length), retainedBytes: 32 };
    },
    hdrSource: () => false,
    aliasVideoFrame: () => true,
  };
  const supply = new FrameSupply(engine);
  await supply.supply(0n, {
    width: 4,
    height: 2,
    liveSourceIds: new Set(["hdr"]),
    getLayer: () => ({ id: "hdr", kind: "video" }),
    pixelsFor: async () => {
      // Both requests land on one decoded frame: the second is an alias.
      const frame = fakeHdrFrame({ transfer: "pq", timestamp: 0 });
      frames.push(frame);
      return {
        kind: "video-frame",
        frame,
        rotation: 90,
        mirror: true,
        chromaSiting: "left",
      };
    },
    isCurrent: () => true,
  });
  assert.equal(yuv.length, 1, "the physical frame is uploaded once");
  const [
    sourceId,
    ticks,
    format,
    codedWidth,
    codedHeight,
    data,
    layout,
    visible,
    displayWidth,
    displayHeight,
    colorJson,
    chromaSiting,
    outputWidth,
    outputHeight,
    rotation,
    mirror,
  ] = yuv[0];
  assert.deepEqual(
    [sourceId, ticks, format, codedWidth, codedHeight],
    ["hdr", 0n, "I420P10", 4, 2],
  );
  assert.deepEqual([...data], new Array(24).fill(7));
  assert.deepEqual([...layout], [0, 8, 16, 4, 20, 4]);
  assert.deepEqual([...visible], [0, 0, 4, 2]);
  assert.deepEqual([displayWidth, displayHeight], [4, 2]);
  assert.deepEqual(JSON.parse(colorJson), {
    dynamic_range: "hdr",
    matrix: "bt2020_ncl",
    primaries: "bt2020",
    range: "limited",
    transfer: "pq",
  });
  assert.equal(chromaSiting, "left");
  assert.deepEqual(
    [outputWidth, outputHeight, rotation, mirror],
    [4, 2, 90, true],
  );
  assert.ok(
    frames.every((frame) => frame.closed),
    "copied frames are closed",
  );
  assert.deepEqual(
    supply.sourceTransfer,
    { planarCopyFrames: 1, planarCopyBytes: 24 },
    "only the uploaded picture is copied out planar: the alias needs no copy",
  );
});

test("a planar copy of an opaque frame is refused by class, and the frame is closed", async () => {
  const frame = opaque(fakeHdrFrame({ transfer: "pq", timestamp: 0 }));
  await assert.rejects(
    copyPlanarYuvFrame(frame, null),
    (error) =>
      error instanceof NonPlanarVideoFrameError &&
      error.format === null &&
      /\(opaque\)/.test(error.message),
  );
  assert.ok(frame.closed);
});

test("SDR frames are converted by the engine from their planes, at their stream's siting, unless the supply leaves them to the browser", async () => {
  const run = async (config, format) => {
    const yuv = [];
    const direct = [];
    const engine = {
      deviceLost: false,
      supportsExternalVideoFrame: true,
      frameNeeds: () =>
        JSON.stringify([{ clipId: "a", sourceId: "sdr", mediaTimeTicks: "0" }]),
      uploadPresentedVideoFrame(...args) {
        direct.push(args);
        return { generation: 1n, retainedBytes: 32 };
      },
      uploadYuvFrame(...args) {
        yuv.push(args);
        return { generation: 1n, retainedBytes: 32 };
      },
      hdrSource: () => false,
      aliasVideoFrame: () => true,
    };
    const supply = new FrameSupply(engine, config);
    const frame = fakeHdrFrame({ transfer: "bt709", timestamp: 0 });
    frame.format = format;
    frame.colorSpace = {
      primaries: "bt709",
      transfer: "bt709",
      matrix: "bt709",
      fullRange: false,
    };
    await supply.supply(0n, {
      width: 4,
      height: 2,
      liveSourceIds: new Set(["sdr"]),
      getLayer: () => ({ id: "sdr", kind: "video" }),
      pixelsFor: async (_layer, _seconds, options) => {
        assert.equal(options.preferVideoFrame, true);
        return { kind: "video-frame", frame, chromaSiting: "left" };
      },
      isCurrent: () => true,
    });
    return { yuv, direct, frame, transfer: supply.sourceTransfer };
  };
  const engine = await run(undefined, "I420");
  assert.equal(engine.direct.length, 0, "the browser does not convert it");
  assert.equal(engine.yuv.length, 1);
  assert.equal(engine.yuv[0][11], "left", "the stream's chroma siting");
  assert.deepEqual(JSON.parse(engine.yuv[0][10]), {
    dynamic_range: "sdr",
    matrix: "bt709",
    primaries: "bt709",
    range: "limited",
    transfer: "bt709",
  });
  assert.ok(engine.frame.closed);
  assert.deepEqual(engine.transfer, {
    planarCopyFrames: 1,
    planarCopyBytes: 24,
  });
  const browser = await run({ videoConversion: "browser" }, "I420");
  assert.equal(browser.yuv.length, 0, "left to the browser");
  assert.equal(browser.direct.length, 1);
  assert.deepEqual(browser.transfer, {
    planarCopyFrames: 0,
    planarCopyBytes: 0,
  });
  // An opaque hardware frame the engine cannot read planar still goes through the browser.
  const opaque = await run(undefined, null);
  assert.equal(opaque.yuv.length, 0);
  assert.equal(opaque.direct.length, 1);
  assert.throws(
    () => new FrameSupply({}, { videoConversion: "gpu" }),
    RangeError,
  );
});

test("a planar frame tells the engine the size it is shown at, which says its pixel aspect ratio", async () => {
  const yuv = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "hdr", mediaTimeTicks: "0" }]),
    uploadPresentedVideoFrame() {
      throw new Error("an HDR frame must not take the browser's conversion");
    },
    uploadYuvFrame(...args) {
      yuv.push(args);
      return { generation: 1n, retainedBytes: 32 };
    },
    hdrSource: () => false,
    aliasVideoFrame: () => false,
  };
  const supply = new FrameSupply(engine);
  await supply.supply(0n, {
    width: 8,
    height: 2,
    liveSourceIds: new Set(["hdr"]),
    getLayer: () => ({ id: "hdr", kind: "video" }),
    // Four stored pixels across, each twice as wide as tall: shown 8x2.
    pixelsFor: async () => ({
      kind: "video-frame",
      frame: fakeHdrFrame({ transfer: "hlg", timestamp: 0, displayWidth: 8 }),
    }),
    isCurrent: () => true,
  });
  assert.equal(yuv.length, 1);
  assert.deepEqual([...yuv[0][7]], [0, 0, 4, 2], "the stored picture");
  assert.deepEqual(
    yuv[0].slice(8, 10),
    [8, 2],
    "the size the stored picture is shown at",
  );
});

test("a source the project declares HDR asks for VideoFrames and keeps its declared color", async () => {
  const declared = {
    dynamic_range: "hdr",
    matrix: "bt2020_ncl",
    primaries: "bt2020",
    range: "limited",
    transfer: "hlg",
  };
  const yuv = [];
  let preferred;
  const engine = {
    deviceLost: false,
    // Without external VideoFrames an SDR source would decode to RGBA.
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "hdr", mediaTimeTicks: "0" }]),
    uploadYuvFrame(...args) {
      yuv.push(args);
      return { generation: 1n, retainedBytes: 32 };
    },
    hdrSource: (id) => id === "hdr",
    sourceColorMetadata: () => JSON.stringify(declared),
    aliasVideoFrame: () => false,
  };
  const supply = new FrameSupply(engine);
  await supply.supply(0n, {
    width: 4,
    height: 2,
    liveSourceIds: new Set(["hdr"]),
    getLayer: () => ({ id: "hdr", kind: "video" }),
    pixelsFor: async (_layer, _seconds, { preferVideoFrame }) => {
      preferred = preferVideoFrame;
      // The stream's own color space is untagged: the declaration wins.
      return {
        kind: "video-frame",
        frame: fakeHdrFrame({ transfer: null, timestamp: 0 }),
      };
    },
    isCurrent: () => true,
  });
  assert.equal(preferred, true);
  assert.equal(yuv.length, 1);
  assert.deepEqual(JSON.parse(yuv[0][10]), declared);
  assert.equal(
    yuv[0][11],
    "unspecified",
    "no siting from the host stays unspecified",
  );
});

test("a source whose color the project declares reaches the engine planar with that color, SDR as well as HDR", async () => {
  // A correction of the stream's tags: full-range BT.601 over frames that say BT.709 limited.
  const declared = {
    dynamic_range: "sdr",
    matrix: "bt601",
    primaries: "bt601",
    range: "full",
    transfer: "bt709",
  };
  const yuv = [];
  let preferred;
  const engine = {
    deviceLost: false,
    // Without external VideoFrames an undeclared SDR source would decode to RGBA.
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([
        { clipId: "a", sourceId: "declared", mediaTimeTicks: "0" },
      ]),
    uploadPresentedVideoFrame() {
      throw new Error(
        "a declared color must not take the browser's conversion of the frame's own",
      );
    },
    uploadYuvFrame(...args) {
      yuv.push(args);
      return { generation: 1n, retainedBytes: 32 };
    },
    hdrSource: () => false,
    sourceColorMetadata: (id) =>
      id === "declared" ? JSON.stringify(declared) : undefined,
    aliasVideoFrame: () => false,
  };
  const frame = fakeHdrFrame({ transfer: "bt709", timestamp: 0 });
  frame.colorSpace = {
    primaries: "bt709",
    transfer: "bt709",
    matrix: "bt709",
    fullRange: false,
  };
  const supply = new FrameSupply(engine);
  await supply.supply(0n, {
    width: 4,
    height: 2,
    liveSourceIds: new Set(["declared"]),
    getLayer: () => ({ id: "declared", kind: "video" }),
    pixelsFor: async (_layer, _seconds, { preferVideoFrame }) => {
      preferred = preferVideoFrame;
      return { kind: "video-frame", frame, chromaSiting: "top-left" };
    },
    isCurrent: () => true,
  });
  assert.equal(preferred, true, "the source decodes to VideoFrames");
  assert.equal(yuv.length, 1, "the frame is converted by the engine");
  assert.deepEqual(JSON.parse(yuv[0][10]), declared);
  assert.equal(yuv[0][11], "top-left");
  assert.ok(frame.closed, "the copied frame is closed");
});

test("a declared HDR source that decodes to RGBA is refused", async () => {
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "hdr", mediaTimeTicks: "0" }]),
    uploadYuvFrame() {
      throw new Error("unreachable");
    },
    hdrSource: () => true,
    aliasVideoFrame: () => false,
  };
  await assert.rejects(
    new FrameSupply(engine).supply(0n, {
      width: 4,
      height: 2,
      liveSourceIds: new Set(["hdr"]),
      getLayer: () => ({ id: "hdr", kind: "video" }),
      pixelsFor: async () => ({
        kind: "rgba-frame",
        pixels: new Uint8Array(32),
        physicalTimeMicroseconds: 0,
        sourceWidth: 4,
        sourceHeight: 2,
      }),
      isCurrent: () => true,
    }),
    /must decode to a VideoFrame, not RGBA/,
  );
});

test("codecs that default chroma to the left say so; the rest stay unspecified", () => {
  for (const codec of [
    "avc1.6E001E",
    "avc",
    "hvc1.2.4.L93.B0",
    "hevc",
    "mp4v.20.9",
  ]) {
    assert.equal(chromaSitingForCodec(codec), "left", codec);
  }
  for (const codec of ["vp09.02.10.10", "vp9", "av01.0.04M.10", "av1", null]) {
    assert.equal(chromaSitingForCodec(codec), "unspecified", String(codec));
  }
});

test("provider frames are presented by the engine at their own size", async () => {
  const frames = [];
  const images = [];
  const aliases = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([
        { clipId: "a", sourceId: "gif", mediaTimeTicks: "0" },
        { clipId: "b", sourceId: "gif", mediaTimeTicks: "1200" },
        { clipId: "c", sourceId: "logo", mediaTimeTicks: "0" },
      ]),
    uploadPresentedRgbaFrame(...args) {
      frames.push(args);
      return { generation: BigInt(frames.length), retainedBytes: 16 };
    },
    uploadPresentedRgbaImage(...args) {
      images.push(args);
      return { generation: 100n, retainedBytes: 16 };
    },
    holdsImage: (id, generation) => id === "logo" && generation === 100n,
    aliasVideoFrame(sourceId, existing, requested) {
      aliases.push([sourceId, existing, requested]);
      return true;
    },
  };
  const gifFrame = {
    kind: "raster-frame",
    pixels: new Uint8Array(48 * 32 * 4),
    width: 48,
    height: 32,
    // Both requests fall inside the first frame: one physical frame.
    physicalTimeMicroseconds: 0,
    durationMicroseconds: 100_000,
  };
  const logo = {
    kind: "raster-frame",
    pixels: new Uint8Array(96 * 48 * 4),
    width: 96,
    height: 48,
    physicalTimeMicroseconds: 0,
  };
  await new FrameSupply(engine).supply(0n, {
    width: 96,
    height: 64,
    liveSourceIds: new Set(["gif", "logo"]),
    getLayer: (id) => ({ id, kind: id === "logo" ? "image" : "video" }),
    pixelsFor: async (layer) => (layer.id === "logo" ? logo : gifFrame),
    isCurrent: () => true,
  });
  assert.equal(frames.length, 1, "the physical frame is presented once");
  assert.deepEqual(
    frames[0].filter((arg) => !(arg instanceof Uint8Array)),
    ["gif", 0n, 48, 32, true, 96, 64],
  );
  assert.deepEqual(aliases, [["gif", 0n, 1200n]]);
  assert.deepEqual(
    images[0].filter((arg) => !(arg instanceof Uint8Array)),
    ["logo", 96, 48, true, 96, 64],
  );
});

test("a decoded still is presented at its own size in the straight alpha it decoded to", async () => {
  const images = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "photo", mediaTimeTicks: "0" }]),
    uploadPresentedRgbaImage(...args) {
      images.push(args);
      return { generation: 1n, retainedBytes: 16 };
    },
    holdsImage: () => true,
  };
  const photo = {
    kind: "raster-frame",
    pixels: new Uint8Array(48 * 64 * 4),
    width: 48,
    height: 64,
    alpha: "straight",
    physicalTimeMicroseconds: 0,
  };
  await new FrameSupply(engine).supply(0n, {
    width: 96,
    height: 64,
    liveSourceIds: new Set(["photo"]),
    getLayer: (id) => ({ id, kind: "image" }),
    pixelsFor: async () => photo,
    isCurrent: () => true,
  });
  assert.deepEqual(
    images[0].filter((arg) => !(arg instanceof Uint8Array)),
    ["photo", 48, 64, false, 96, 64],
  );
});

test("a frame its stream mirrors reaches the engine with its mirror and turn", async () => {
  const uploads = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "clip", mediaTimeTicks: "0" }]),
    uploadPresentedVideoFrame(...args) {
      uploads.push(args);
      return { generation: 1n, retainedBytes: 16 };
    },
    hdrSource: () => false,
    aliasVideoFrame: () => false,
  };
  const frame = {
    timestamp: 0,
    duration: null,
    displayWidth: 4,
    displayHeight: 2,
    colorSpace: { transfer: "bt709" },
    close() {},
  };
  await new FrameSupply(engine).supply(0n, {
    width: 4,
    height: 2,
    liveSourceIds: new Set(["clip"]),
    getLayer: (id) => ({ id, kind: "video" }),
    pixelsFor: async () => ({
      kind: "video-frame",
      frame,
      rotation: 90,
      mirror: true,
    }),
    isCurrent: () => true,
  });
  assert.equal(uploads.length, 1);
  assert.deepEqual(uploads[0].slice(3), [4, 2, 90, true]);
});

/** A decoded SDR frame that counts its closes, as a hardware decoder's pool would notice them. */
function countedVideoFrame(timestamp, frames) {
  const frame = {
    format: "NV12",
    codedWidth: 2,
    codedHeight: 2,
    displayWidth: 2,
    displayHeight: 2,
    timestamp,
    duration: 33_333,
    colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709" },
    closes: 0,
    close() {
      frame.closes += 1;
    },
  };
  frames.push(frame);
  return frame;
}

/** An engine that takes `VideoFrame`s directly and closes each one it uploads, as wasm does. */
function videoFrameEngine(needs, { failUploadAt = Infinity } = {}) {
  let generation = 0n;
  const live = new Set();
  const engine = {
    uploads: 0,
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () => JSON.stringify(needs),
    uploadPresentedVideoFrame(sourceId, ticks, frame) {
      if (engine.uploads === failUploadAt)
        throw new Error("upload failed: out of memory");
      engine.uploads += 1;
      frame.close();
      live.add(`${sourceId}@${ticks}`);
      generation += 1n;
      return { generation, retainedBytes: 16 };
    },
    aliasVideoFrame(sourceId, existing, requested) {
      if (!live.has(`${sourceId}@${existing}`)) return false;
      live.add(`${sourceId}@${requested}`);
      return true;
    },
    releaseFrameGeneration() {},
    releaseSourceFrames() {},
  };
  return engine;
}

const clipNeeds = (count) =>
  Array.from({ length: count }, (_, index) => ({
    clipId: `clip-${index}`,
    sourceId: "video",
    mediaTimeTicks: String(index * 4_000),
  }));

const videoOptions = (pixelsFor, isCurrent = () => true) => ({
  width: 2,
  height: 2,
  liveSourceIds: new Set(["video"]),
  getLayer: () => ({ id: "video", kind: "video" }),
  pixelsFor,
  isCurrent,
});

test("decoded frames of a request invalidated while it decodes are closed, once each", async () => {
  const frames = [];
  let current = true;
  const supply = new FrameSupply(videoFrameEngine(clipNeeds(2)));
  const result = await supply.supply(
    0n,
    videoOptions(
      async (_layer, seconds) => {
        await new Promise(setImmediate);
        current = false;
        return {
          kind: "video-frame",
          frame: countedVideoFrame(Math.round(seconds * 1e6), frames),
        };
      },
      () => current,
    ),
  );
  assert.equal(result, null);
  assert.equal(frames.length, 2);
  assert.deepEqual(
    frames.map(({ closes }) => closes),
    [1, 1],
  );
});

test("when one decoder cursor fails, the frames the others decoded are closed", async () => {
  const frames = [];
  const supply = new FrameSupply(videoFrameEngine(clipNeeds(3)), {
    maxDecodeConcurrency: 3,
  });
  await assert.rejects(
    supply.supply(
      0n,
      videoOptions(async (_layer, seconds, { consumerId }) => {
        await new Promise(setImmediate);
        if (consumerId === "clip-1") throw new Error("decoder error");
        return {
          kind: "video-frame",
          frame: countedVideoFrame(Math.round(seconds * 1e6), frames),
        };
      }),
    ),
    /decoder error/,
  );
  assert.equal(frames.length, 2);
  assert.deepEqual(
    frames.map(({ closes }) => closes),
    [1, 1],
  );
});

test("when an upload fails, the frames not yet uploaded are closed", async () => {
  const frames = [];
  const engine = videoFrameEngine(clipNeeds(3), { failUploadAt: 1 });
  const supply = new FrameSupply(engine, { maxDecodeConcurrency: 3 });
  await assert.rejects(
    supply.supply(
      0n,
      videoOptions(async (_layer, seconds) => ({
        kind: "video-frame",
        frame: countedVideoFrame(Math.round(seconds * 1e6), frames),
      })),
    ),
    /upload failed/,
  );
  assert.equal(engine.uploads, 1);
  assert.deepEqual(
    frames.map(({ closes }) => closes),
    [1, 1, 1],
    "the uploaded frame by the engine, the others by the supply",
  );
});

test("a decoded frame served by an earlier upload of the same picture is closed", async () => {
  // A 24 fps clip on a 60 fps timeline: two requests decode to one picture.
  const frames = [];
  const needs = [
    { clipId: "clip", sourceId: "video", mediaTimeTicks: "0" },
    { clipId: "clip", sourceId: "video", mediaTimeTicks: "2000" },
  ];
  const engine = videoFrameEngine(needs);
  const supply = new FrameSupply(engine);
  const supplied = await supply.supply(
    0n,
    videoOptions(async () => ({
      kind: "video-frame",
      frame: countedVideoFrame(0, frames),
    })),
  );
  assert.equal(engine.uploads, 1);
  assert.equal(supplied.uploaded.length, 1);
  assert.deepEqual(
    frames.map(({ closes }) => closes),
    [1, 1],
  );
});

test("a request invalidated while it decodes starts no further decodes", async () => {
  // A stale request should stop after the decodes already running, not decode all six readers.
  let decodes = 0;
  let current = true;
  const frames = [];
  const supply = new FrameSupply(videoFrameEngine(clipNeeds(6)), {
    maxDecodeConcurrency: 2,
  });
  const result = await supply.supply(
    0n,
    videoOptions(
      async (_layer, seconds) => {
        decodes += 1;
        current = false;
        await new Promise(setImmediate);
        return {
          kind: "video-frame",
          frame: countedVideoFrame(Math.round(seconds * 1e6), frames),
        };
      },
      () => current,
    ),
  );
  assert.equal(result, null);
  assert.equal(decodes, 2);
  assert.ok(frames.every(({ closes }) => closes === 1));
});

test("a still evicted in a frame does not release the upload an alias in that frame relies on", async () => {
  // A held video picture under a slideshow: retiring the evicted still at once would push the
  // video's generation past the retired budget before frame 2 rendered.
  const engine = frameStoreEngine({
    needsAt: (ticks) =>
      ticks === 0n
        ? [need("video", "0"), need("still-1")]
        : [need("video", "2000"), need("still-2")],
  });
  const stills = new Map([
    ["still-1", new Uint8Array(16)],
    ["still-2", new Uint8Array(16)],
  ]);
  const supply = new FrameSupply(engine, {
    maxRetiredBytes: 16,
    maxCachedImageBytes: 16,
  });
  const options = {
    width: 2,
    height: 2,
    liveSourceIds: new Set(["video", "still-1", "still-2"]),
    getLayer: (id) => ({ id, kind: id === "video" ? "video" : "image" }),
    pixelsFor: async ({ id }) =>
      stills.get(id) ?? {
        kind: "rgba-frame",
        pixels: new Uint8Array(16),
        physicalTimeMicroseconds: 0,
        durationMicroseconds: 41_667,
        sourceWidth: 2,
        sourceHeight: 2,
      },
    isCurrent: () => true,
  };
  assert.deepEqual((await supply.render(0n, options)).missingSources, []);
  const second = await supply.render(1n, options);
  assert.deepEqual(second.missingSources, []);
  assert.deepEqual(engine.uploads, ["video", "still-1", "still-2"]);
});

/** An engine that logs uploads, releases and presentations, and presents what it still holds. */
function loggingEngine(needs) {
  let generation = 0n;
  const held = new Map();
  const engine = {
    log: [],
    renderError: null,
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () => JSON.stringify(needs),
    uploadRgbaFrame(sourceId, ticks) {
      generation += 1n;
      held.set(`${sourceId}@${ticks}`, generation);
      engine.log.push(`upload ${generation}`);
      return { generation, retainedBytes: 16 };
    },
    aliasVideoFrame: () => false,
    releaseFrameGeneration(value) {
      engine.log.push(`release ${value}`);
      for (const [key, live] of held) if (live === value) held.delete(key);
    },
    releaseSourceFrames() {},
    renderAt(ticks) {
      engine.log.push("renderAt");
      if (engine.renderError) throw engine.renderError;
      const missingSources = needs
        .filter(
          ({ sourceId, mediaTimeTicks }) =>
            !held.has(`${sourceId}@${mediaTimeTicks}`),
        )
        .map(({ sourceId }) => sourceId);
      return JSON.stringify({
        requestedTimeTicks: Number(ticks),
        presentedTimeTicks: missingSources.length ? null : Number(ticks),
        missingSources,
        repeated: false,
      });
    },
  };
  return engine;
}

const twoSources = [need("a", "0"), need("b", "4000")];
const rgbaOptions = (isCurrent = () => true) => ({
  width: 2,
  height: 2,
  liveSourceIds: new Set(["a", "b"]),
  getLayer: (id) => ({ id, kind: "video" }),
  pixelsFor: async (_layer, seconds) => ({
    kind: "rgba-frame",
    pixels: new Uint8Array(16),
    physicalTimeMicroseconds: Math.round(seconds * 1e6),
    sourceWidth: 2,
    sourceHeight: 2,
  }),
  isCurrent,
});

test("render presents a frame's uploads before it retires them", async () => {
  // A retired budget smaller than the frame: retiring before presenting would release the first
  // upload and present it missing.
  const engine = loggingEngine(twoSources);
  const supply = new FrameSupply(engine, { maxRetiredBytes: 1 });
  const shown = await supply.render(0n, rgbaOptions());
  assert.deepEqual(shown.missingSources, []);
  assert.deepEqual(engine.log, [
    "upload 1",
    "upload 2",
    "renderAt",
    "release 1",
  ]);
});

test("render retires a frame's uploads when presenting throws", async () => {
  const engine = loggingEngine(twoSources);
  engine.renderError = new Error("presentation failed");
  const supply = new FrameSupply(engine);
  await assert.rejects(supply.render(0n, rgbaOptions()), /presentation failed/);
  assert.equal(supply.retiredBytes, 32);
});

test("render resolves null without presenting when the request went stale after its supply", async () => {
  const engine = loggingEngine(twoSources);
  let current = true;
  const upload = engine.uploadRgbaFrame;
  engine.uploadRgbaFrame = (...args) => {
    const receipt = upload(...args);
    if (receipt.generation === 2n) current = false;
    return receipt;
  };
  const supply = new FrameSupply(engine);
  const shown = await supply.render(
    0n,
    rgbaOptions(() => current),
  );
  assert.equal(shown, null);
  assert.equal(engine.log.includes("renderAt"), false);
  assert.equal(supply.retiredBytes, 32, "its uploads are retired");
});

test("render names each source the engine still misses, with the time it was asked for", async () => {
  const engine = loggingEngine(twoSources);
  // The engine drops what it was given before presenting.
  engine.uploadRgbaFrame = function (sourceId, ticks) {
    return { generation: BigInt(ticks) + 1n, retainedBytes: 16 };
  };
  const supply = new FrameSupply(engine);
  await assert.rejects(
    supply.render(7n, rgbaOptions()),
    /^Error: missing frames at timeline 7: a@0, b@4000$/,
  );
  assert.equal(supply.retiredBytes, 32);
});

test("a held HDR picture is copied out planar once, not on every frame that shows it", async () => {
  // A freeze over an HDR clip: every frame asks for a new source time that decodes to one picture.
  let requested = "0";
  const frames = [];
  const live = new Set();
  let generation = 0n;
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () =>
      JSON.stringify([
        { clipId: "freeze", sourceId: "hdr", mediaTimeTicks: requested },
      ]),
    uploadYuvFrame(sourceId, ticks) {
      live.add(`${sourceId}@${ticks}`);
      generation += 1n;
      return { generation, retainedBytes: 32 };
    },
    hdrSource: () => true,
    aliasVideoFrame(sourceId, existing, next) {
      if (!live.has(`${sourceId}@${existing}`)) return false;
      live.add(`${sourceId}@${next}`);
      return true;
    },
    releaseFrameGeneration() {},
    releaseSourceFrames() {},
  };
  const supply = new FrameSupply(engine);
  for (const ticks of ["0", "2000", "4000"]) {
    requested = ticks;
    await supply.supply(BigInt(ticks), {
      width: 4,
      height: 2,
      liveSourceIds: new Set(["hdr"]),
      getLayer: () => ({ id: "hdr", kind: "video" }),
      pixelsFor: async () => {
        const frame = fakeHdrFrame({ transfer: "hlg", timestamp: 0 });
        frames.push(frame);
        return { kind: "video-frame", frame };
      },
      isCurrent: () => true,
    });
  }
  assert.equal(generation, 1n, "one upload");
  assert.deepEqual(supply.sourceTransfer, {
    planarCopyFrames: 1,
    planarCopyBytes: 24,
  });
  assert.ok(frames.every((frame) => frame.closed));
});

test("a provider still answered as a new array each time is uploaded once", async () => {
  // `ExtensionSource.render` copies the frame out of the module on every call: a new array each time.
  const images = [];
  let generation = 0n;
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: false,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "c", sourceId: "logo", mediaTimeTicks: "0" }]),
    uploadPresentedRgbaImage(...args) {
      images.push(args);
      generation += 1n;
      return { generation, retainedBytes: 16 };
    },
    uploadPresentedLinearImage(...args) {
      images.push(args);
      generation += 1n;
      return { generation, retainedBytes: 32 };
    },
    holdsImage: (_id, value) => value === generation,
    aliasVideoFrame: () => false,
    releaseFrameGeneration() {},
    releaseSourceFrames() {},
  };
  let still = {
    kind: "raster-frame",
    width: 4,
    height: 2,
    physicalTimeMicroseconds: 0,
  };
  const supply = new FrameSupply(engine);
  const options = (width) => ({
    width,
    height: 64,
    liveSourceIds: new Set(["logo"]),
    getLayer: (id) => ({ id, kind: "image" }),
    pixelsFor: async () => ({ ...still, pixels: new Uint8Array(32) }),
    isCurrent: () => true,
  });
  for (let frame = 0; frame < 5; frame += 1)
    supply.retire((await supply.supply(0n, options(96))).uploaded);
  assert.equal(images.length, 1, "the same picture is not uploaded again");
  // Another output size, another raster, another precision: each is a new upload.
  await supply.supply(0n, options(128));
  assert.equal(images.length, 2);
  still = { ...still, width: 8, height: 4 };
  await supply.supply(0n, options(128));
  assert.equal(images.length, 3);
  const pixelsFor = async () => ({
    ...still,
    precision: "linear_f16",
    pixels: new Uint16Array(32),
  });
  await supply.supply(0n, { ...options(128), pixelsFor });
  await supply.supply(0n, { ...options(128), pixelsFor });
  assert.equal(images.length, 4);
});

test("HDR frames are copied out planar into one buffer reused from frame to frame", async () => {
  const buffers = [];
  const engine = {
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "hdr", mediaTimeTicks: "0" }]),
    uploadYuvFrame(_sourceId, _ticks, _format, _width, _height, data) {
      // The engine copies the planes into wasm memory before it returns.
      buffers.push([data.buffer, data.byteLength, data[0]]);
      return { generation: BigInt(buffers.length), retainedBytes: 32 };
    },
    hdrSource: () => false,
    aliasVideoFrame: () => false,
    releaseFrameGeneration: () => true,
  };
  const supply = new FrameSupply(engine);
  for (let frame = 0; frame < 3; frame += 1) {
    await supply.supply(BigInt(frame) * 4_800n, {
      width: 4,
      height: 2,
      liveSourceIds: new Set(["hdr"]),
      getLayer: () => ({ id: "hdr", kind: "video" }),
      pixelsFor: async () => ({
        kind: "video-frame",
        frame: fakeHdrFrame({ transfer: "hlg", timestamp: frame * 40_000 }),
      }),
      isCurrent: () => true,
    });
  }
  assert.equal(buffers.length, 3);
  assert.deepEqual(
    buffers.map(([, length, first]) => [length, first]),
    [
      [24, 7],
      [24, 7],
      [24, 7],
    ],
  );
  assert.ok(
    buffers.every(([buffer]) => buffer === buffers[0][0]),
    "every copy reuses the first one's buffer",
  );
});

const unreadableError = () =>
  new DOMException("Failed to read VideoFrame data.", "InvalidStateError");

/** `frame`, whose first `failures` reads fail as Chrome's hardware frames do now and then. */
function failingReads(frame, failures = Infinity) {
  const read = frame.copyTo;
  let failed = 0;
  frame.copyTo = async (buffer) => {
    if (failed < failures) {
      failed += 1;
      throw unreadableError();
    }
    return read(buffer);
  };
  return frame;
}

function sdrFrame(format = "NV12") {
  const frame = fakeHdrFrame({ transfer: "bt709", timestamp: 0 });
  frame.format = format;
  frame.colorSpace = {
    primaries: "bt709",
    transfer: "bt709",
    matrix: "bt709",
    fullRange: false,
  };
  return frame;
}

function recoveryEngine() {
  const yuv = [];
  const direct = [];
  return {
    yuv,
    direct,
    deviceLost: false,
    supportsExternalVideoFrame: true,
    frameNeeds: () =>
      JSON.stringify([{ clipId: "a", sourceId: "v", mediaTimeTicks: "0" }]),
    uploadPresentedVideoFrame(...args) {
      direct.push(args);
      return { generation: 1n, retainedBytes: 32 };
    },
    uploadYuvFrame(...args) {
      yuv.push(args);
      return { generation: 1n, retainedBytes: 32 };
    },
    hdrSource: () => false,
    aliasVideoFrame: () => true,
  };
}

/** Supplies one frame: `hardware` first, and `software()` for a request to decode in software. */
function supplyOne(supply, hardware, software = () => null) {
  const requests = [];
  const done = supply.supply(0n, {
    width: 4,
    height: 2,
    liveSourceIds: new Set(["v"]),
    getLayer: () => ({ id: "v", kind: "video" }),
    pixelsFor: async (_layer, _seconds, options) => {
      requests.push(options);
      if (options.decode === "software") {
        const frame = software();
        return frame && { kind: "video-frame", frame, chromaSiting: "left" };
      }
      return { kind: "video-frame", frame: hardware, chromaSiting: "left" };
    },
    isCurrent: () => true,
  });
  return { done, requests };
}

test("a frame the browser fails to read back once is read again", async () => {
  const engine = recoveryEngine();
  const supply = new FrameSupply(engine);
  const frame = failingReads(sdrFrame(), 1);
  const { done, requests } = supplyOne(supply, frame);
  await done;
  assert.equal(engine.yuv.length, 1);
  assert.equal(requests.length, 1, "no software decode");
  assert.ok(frame.closed);
  assert.deepEqual(supply.readbackRecovery, {
    unreadableFrames: 0,
    readbackRetries: 1,
    redecodedFrames: 0,
    presentedFrames: 0,
  });
  assert.equal(supply.sourceTransfer.planarCopyFrames, 1);
});

test("a frame the browser cannot read back is decoded again in software and converted by the engine", async () => {
  const engine = recoveryEngine();
  const supply = new FrameSupply(engine);
  const hardware = failingReads(sdrFrame("NV12"));
  const software = sdrFrame("I420");
  const { done, requests } = supplyOne(supply, hardware, () => software);
  await done;
  assert.equal(engine.direct.length, 0, "never the browser's conversion");
  assert.equal(engine.yuv.length, 1);
  assert.equal(engine.yuv[0][2], "I420", "the software decode's planes");
  assert.equal(requests.length, 2);
  assert.equal(requests[1].decode, "software");
  assert.equal(requests[1].consumerId, requests[0].consumerId);
  assert.equal(requests[1].sourceId, "v");
  assert.equal(requests[0].decode, undefined);
  assert.ok(hardware.closed && software.closed);
  assert.deepEqual(supply.readbackRecovery, {
    unreadableFrames: 1,
    readbackRetries: 2,
    redecodedFrames: 1,
    presentedFrames: 0,
  });
  assert.equal(supply.sourceTransfer.planarCopyFrames, 1);
});

test("a software decode of another picture is refused", async () => {
  const engine = recoveryEngine();
  const supply = new FrameSupply(engine);
  const later = sdrFrame("I420");
  later.timestamp = 40_000;
  const { done } = supplyOne(supply, failingReads(sdrFrame()), () => later);
  await assert.rejects(
    done,
    (error) =>
      /gave the frame at 40000 us/.test(error.message) &&
      error.cause?.name === "InvalidStateError",
  );
  assert.ok(later.closed);
  assert.equal(engine.yuv.length, 0);
  assert.equal(supply.readbackRecovery.redecodedFrames, 0);
});

test("a source the host cannot decode in software fails the supply, with why", async () => {
  const supply = new FrameSupply(recoveryEngine());
  const hardware = failingReads(sdrFrame());
  const { done } = supplyOne(supply, hardware, () => {
    throw new Error("this browser has no software decoder for hvc1");
  });
  await assert.rejects(
    done,
    (error) =>
      /decoding it again in software failed/.test(error.message) &&
      /no software decoder/.test(error.cause?.message),
  );
  assert.ok(hardware.closed);
});

test("a software decode that gives no frame, or one that cannot be read either, fails the supply", async () => {
  const none = new FrameSupply(recoveryEngine());
  await assert.rejects(
    supplyOne(none, failingReads(sdrFrame()), () => null).done,
    /gave no VideoFrame/,
  );
  const unreadable = new FrameSupply(recoveryEngine());
  const software = failingReads(sdrFrame("I420"));
  await assert.rejects(
    supplyOne(unreadable, failingReads(sdrFrame()), () => software).done,
    (error) =>
      /could not be copied either/.test(error.message) &&
      error.cause?.name === "InvalidStateError",
  );
  assert.ok(software.closed);
});

test("a preview gives an SDR frame it cannot read back to the browser, but never an HDR one", async () => {
  const sdr = recoveryEngine();
  const preview = new FrameSupply(sdr, { unreadableFrames: "present" });
  const frame = failingReads(sdrFrame());
  const { done, requests } = supplyOne(preview, frame);
  await done;
  assert.equal(sdr.direct.length, 1);
  assert.equal(sdr.direct[0][2], frame, "the frame itself, to the browser");
  assert.equal(sdr.yuv.length, 0);
  assert.equal(requests.length, 1, "no software decode in a preview");
  assert.equal(preview.readbackRecovery.presentedFrames, 1);
  assert.equal(preview.sourceTransfer.planarCopyFrames, 0);

  const hdr = recoveryEngine();
  hdr.uploadPresentedVideoFrame = () => {
    throw new Error("an HDR frame must not take the browser's conversion");
  };
  const strict = new FrameSupply(hdr, { unreadableFrames: "present" });
  const pq = failingReads(fakeHdrFrame({ transfer: "pq", timestamp: 0 }));
  const software = fakeHdrFrame({ transfer: "pq", timestamp: 0 });
  await supplyOne(strict, pq, () => software).done;
  assert.equal(hdr.yuv.length, 1, "decoded again in software instead");
  assert.ok(pq.closed && software.closed);
  assert.equal(strict.readbackRecovery.redecodedFrames, 1);
  assert.equal(strict.readbackRecovery.presentedFrames, 0);
});

test("a preview frame given back to the browser is closed when the browser's upload fails", async () => {
  const engine = recoveryEngine();
  engine.uploadPresentedVideoFrame = () => {
    throw new Error("the external texture failed");
  };
  const supply = new FrameSupply(engine, { unreadableFrames: "present" });
  const frame = failingReads(sdrFrame());
  await assert.rejects(
    supplyOne(supply, frame).done,
    /the external texture failed/,
  );
  assert.ok(frame.closed);
});

test("a planar copy that fails otherwise is neither retried nor decoded again", async () => {
  const engine = recoveryEngine();
  const supply = new FrameSupply(engine);
  const frame = sdrFrame("I420");
  frame.copyTo = async () => {
    throw new RangeError("destination is too small");
  };
  const { done, requests } = supplyOne(supply, frame);
  await assert.rejects(done, RangeError);
  assert.ok(frame.closed);
  assert.equal(requests.length, 1);
  assert.equal(supply.readbackRecovery.readbackRetries, 0);
});

test("an HDR frame with no planar format is decoded again in software and converted by the engine", async () => {
  const engine = recoveryEngine();
  const supply = new FrameSupply(engine);
  const hardware = opaque(fakeHdrFrame({ transfer: "hlg", timestamp: 0 }));
  const software = fakeHdrFrame({ transfer: "hlg", timestamp: 0 });
  const { done, requests } = supplyOne(supply, hardware, () => software);
  await done;
  assert.equal(engine.yuv.length, 1);
  assert.equal(engine.yuv[0][2], "I420P10", "the software decode's planes");
  assert.equal(engine.direct.length, 0, "never the browser's conversion");
  assert.equal(requests.length, 2);
  assert.equal(requests[1].decode, "software");
  assert.ok(hardware.closed && software.closed);
  assert.deepEqual(supply.readbackRecovery, {
    unreadableFrames: 1,
    readbackRetries: 0,
    redecodedFrames: 1,
    presentedFrames: 0,
  });
});

test("a source whose frames come back opaque decodes in software from then on, until its content changes", async () => {
  const engine = recoveryEngine();
  let at = 0n;
  engine.releaseSourceFrames = () => {};
  engine.frameNeeds = () =>
    JSON.stringify([
      { clipId: "a", sourceId: "v", mediaTimeTicks: String(at) },
    ]);
  const supply = new FrameSupply(engine);
  let content = "original";
  const requests = [];
  const supplyAt = (ticks) => {
    at = ticks;
    const timestamp = Number(ticks / 120n);
    return supply.supply(ticks, {
      width: 4,
      height: 2,
      contentOf: () => content,
      liveSourceIds: new Set(["v"]),
      getLayer: () => ({ id: "v", kind: "video" }),
      pixelsFor: async (_layer, _seconds, options) => {
        requests.push(options.decode ?? "auto");
        const frame =
          options.decode === "software"
            ? fakeHdrFrame({ transfer: "hlg", timestamp })
            : opaque(fakeHdrFrame({ transfer: "hlg", timestamp }));
        return { kind: "video-frame", frame, chromaSiting: "left" };
      },
      isCurrent: () => true,
    });
  };
  await supplyAt(0n);
  await supplyAt(4_000n);
  assert.deepEqual(requests, ["auto", "software", "software"]);
  assert.equal(engine.yuv.length, 2);
  assert.equal(supply.readbackRecovery.unreadableFrames, 1);
  // Other media (a proxy, or the original again) may decode otherwise: it is tried as it comes.
  content = "proxy";
  requests.length = 0;
  await supplyAt(8_000n);
  assert.deepEqual(requests, ["auto", "software"]);
});

test("a preview decodes an opaque HDR frame again too and never presents it", async () => {
  const engine = recoveryEngine();
  engine.uploadPresentedVideoFrame = () => {
    throw new Error("an HDR frame must not take the browser's conversion");
  };
  const supply = new FrameSupply(engine, { unreadableFrames: "present" });
  const hardware = opaque(fakeHdrFrame({ transfer: "hlg", timestamp: 0 }));
  const software = fakeHdrFrame({ transfer: "hlg", timestamp: 0 });
  const { done, requests } = supplyOne(supply, hardware, () => software);
  await done;
  assert.equal(engine.yuv.length, 1);
  assert.equal(engine.direct.length, 0);
  assert.equal(requests.length, 2);
  assert.equal(requests[1].decode, "software");
  assert.ok(hardware.closed && software.closed);
  assert.deepEqual(supply.readbackRecovery, {
    unreadableFrames: 1,
    readbackRetries: 0,
    redecodedFrames: 1,
    presentedFrames: 0,
  });
});

/** What a decoder adapter's software open throws when the browser has no software decoder. */
const noSoftwareDecoder = () => {
  throw Object.assign(
    new Error("this browser has no software decoder for hev1"),
    { code: "software_decode_unavailable" },
  );
};

test("opaque hdr frame without software decoder rejects with planar decode unavailable", async () => {
  const supply = new FrameSupply(recoveryEngine());
  const hardware = opaque(fakeHdrFrame({ transfer: "pq", timestamp: 0 }));
  const { done } = supplyOne(supply, hardware, noSoftwareDecoder);
  await assert.rejects(done, (error) => {
    assert.ok(error instanceof PlanarDecodeUnavailableError, error.stack);
    assert.equal(error.code, "PLANAR_DECODE_UNAVAILABLE");
    assert.equal(error.facts.transfer, "pq");
    assert.equal(error.facts.frameFormat, null);
    assert.equal(error.facts.softwareDecoder, "none");
    assert.match(error.facts.browserError, /no software decoder/);
    // The user's words come from the facts, not from the browser.
    assert.doesNotMatch(error.message, /hev1/);
    assert.equal(error.cause.code, "software_decode_unavailable");
    assert.equal(error.failure.code, "INVALID_INPUT");
    return true;
  });
  assert.ok(hardware.closed);
  assert.deepEqual(supply.readbackRecovery, {
    unreadableFrames: 1,
    readbackRetries: 0,
    redecodedFrames: 0,
    presentedFrames: 0,
  });
});

test("later frames of a refused source reject with the same typed error", async () => {
  const supply = new FrameSupply(recoveryEngine());
  const first = opaque(fakeHdrFrame({ transfer: "hlg", timestamp: 0 }));
  const refusal = await supplyOne(supply, first, noSoftwareDecoder).done.then(
    () => assert.fail("the first frame was not refused"),
    (error) => error,
  );
  assert.ok(refusal instanceof PlanarDecodeUnavailableError);
  let decodes = 0;
  const later = supply.supply(4_000n, {
    width: 4,
    height: 2,
    liveSourceIds: new Set(["v"]),
    getLayer: () => ({ id: "v", kind: "video" }),
    pixelsFor: async () => {
      decodes += 1;
      return { kind: "video-frame", frame: sdrFrame(), chromaSiting: "left" };
    },
    isCurrent: () => true,
  });
  await assert.rejects(later, (error) => error === refusal);
  assert.equal(decodes, 0, "a refused source is not decoded again");
});

test("an opaque frame whose software decode fails otherwise is a frame-supply failure", async () => {
  const supply = new FrameSupply(recoveryEngine());
  const hardware = opaque(fakeHdrFrame({ transfer: "pq", timestamp: 0 }));
  const { done } = supplyOne(supply, hardware, () => {
    throw new Error("the decoder crashed");
  });
  await assert.rejects(
    done,
    (error) =>
      error.code === "FRAME_SUPPLY_FAILED" &&
      /an opaque frame/.test(error.message) &&
      /decoding it again in software failed/.test(error.message) &&
      /crashed/.test(error.cause?.message),
  );
});

test("an opaque frame of a source whose color the project declares is decoded again in software", async () => {
  const declared = {
    dynamic_range: "sdr",
    matrix: "bt601",
    primaries: "bt601",
    range: "full",
    transfer: "bt709",
  };
  const engine = recoveryEngine();
  engine.sourceColorMetadata = () => JSON.stringify(declared);
  const supply = new FrameSupply(engine);
  const hardware = opaque(sdrFrame("NV12"));
  const software = sdrFrame("I420");
  const { done, requests } = supplyOne(supply, hardware, () => software);
  await done;
  assert.equal(engine.yuv.length, 1);
  assert.equal(engine.yuv[0][2], "I420");
  assert.deepEqual(JSON.parse(engine.yuv[0][10]), declared);
  assert.equal(engine.direct.length, 0);
  assert.equal(requests[1].decode, "software");
  assert.ok(hardware.closed && software.closed);
  assert.equal(supply.readbackRecovery.redecodedFrames, 1);
});

test("unreadableFrames takes only redecode or present", () => {
  assert.throws(
    () => new FrameSupply(recoveryEngine(), { unreadableFrames: "skip" }),
    RangeError,
  );
});
