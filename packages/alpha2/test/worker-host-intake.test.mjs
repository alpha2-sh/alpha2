import assert from "node:assert/strict";
import test from "node:test";

import { createWasmRenderWorkerHost } from "../dist/worker.js";
import { Alpha2Player } from "../dist/player.js";
import { DecoderRefillCoordinator } from "../dist/media.js";

const PROJECT = JSON.stringify({ settings: { width: 4, height: 2 } });

/** A CPU-backed 10-bit HLG frame; `opaque` makes it a hardware frame with no planes to copy. */
function hlgFrame({ opaque = false, timestamp = 0, unreadable = 0 } = {}) {
  let failed = 0;
  const frame = {
    format: opaque ? null : "I420P10",
    codedWidth: 4,
    codedHeight: 2,
    displayWidth: 4,
    displayHeight: 2,
    timestamp,
    duration: 40_000,
    visibleRect: { x: 0, y: 0, width: 4, height: 2 },
    colorSpace: {
      primaries: "bt2020",
      transfer: "hlg",
      matrix: "bt2020-ncl",
      fullRange: false,
    },
    closed: false,
    allocationSize: () => 24,
    async copyTo(buffer) {
      if (opaque) throw new Error("an opaque frame has no planes to copy");
      if (failed < unreadable) {
        failed += 1;
        throw new DOMException(
          "Failed to read VideoFrame data.",
          "InvalidStateError",
        );
      }
      buffer.fill(7);
      return [
        { offset: 0, stride: 8 },
        { offset: 16, stride: 4 },
        { offset: 20, stride: 4 },
      ];
    },
    close() {
      frame.closed = true;
    },
  };
  return frame;
}

/** A binding that misses source `hdr` at 0 until a frame of it is uploaded. */
function binding() {
  const engine = {
    deviceLost: false,
    yuv: [],
    presented: [],
    loadProject() {},
    free() {},
    sourceColorMetadata: () => undefined,
    hdrSource: () => false,
    aliasVideoFrame: () => false,
    uploadYuvFrame(...args) {
      engine.yuv.push(args);
      return { generation: 1n, retainedBytes: 32 };
    },
    uploadPresentedVideoFrame(...args) {
      engine.presented.push(args);
      args[2].close();
      return { generation: 1n, retainedBytes: 32 };
    },
    releaseFrameGeneration() {},
    resetPlayer() {},
    renderAt(ticks) {
      const held = engine.yuv.length + engine.presented.length > 0;
      return JSON.stringify({
        requestedTimeTicks: Number(ticks),
        presentedTimeTicks: held ? Number(ticks) : null,
        missingSources: held ? [] : ["hdr"],
        missingFrames: held
          ? []
          : [{ clipId: "clip", sourceId: "hdr", mediaTimeTicks: "0" }],
        repeated: false,
        unchanged: false,
      });
    },
  };
  return {
    engine,
    async createForCanvas() {
      return engine;
    },
  };
}

/** The host as an in-process `RenderBackend`, as a page drives the worker's one. */
function backendOf(host) {
  return {
    initialize: async () => {},
    uploadVideoFrame: (frame) => host.uploadVideoFrame(frame),
    present: (ticks) => host.present(ticks),
    reset: async () => host.reset(),
    isDeviceLost: () => false,
    lossReport: () => undefined,
    recover: async () => undefined,
    dispose: () => host.dispose(),
  };
}

/** A decoder answering every request with `make(request)`'s frame. */
function decoder(make, requests) {
  return {
    async refill(request) {
      requests.push(request);
      return {
        sourceId: request.sourceId,
        requestedTimeTicks: request.mediaTimeTicks,
        decodedTimeTicks: request.mediaTimeTicks,
        generation: request.generation,
        frame: make(request),
      };
    },
    async flush() {},
    close() {},
  };
}

async function playerWith(host, open) {
  const requests = [];
  const opened = [];
  let player;
  const coordinator = new DecoderRefillCoordinator(
    async (sourceId, options) => {
      opened.push(options.decode);
      return open(options, requests);
    },
    (frame) => player.uploadVideoFrame(frame),
  );
  player = new Alpha2Player(backendOf(host), {
    onRefill: (batch) =>
      Promise.all(batch.map((request) => coordinator.refill(request))).then(
        () => {},
      ),
  });
  await player.initialize("canvas");
  return { player, requests, opened };
}

test("an opaque hdr frame asks the page for a software decode", async () => {
  const fake = binding();
  const host = await createWasmRenderWorkerHost(fake, PROJECT)("canvas");
  const hardware = [];
  const { player, requests, opened } = await playerWith(host, (options, log) =>
    decoder(() => {
      const frame = hlgFrame({ opaque: options.decode !== "software" });
      if (options.decode !== "software") hardware.push(frame);
      return frame;
    }, log),
  );
  const shown = await player.seek(0n);
  assert.deepEqual(shown.missingFrames, []);
  assert.deepEqual(opened, ["default", "software"]);
  assert.deepEqual(
    requests.map((request) => request.decode ?? "default"),
    ["default", "software"],
  );
  assert.equal(
    fake.engine.yuv.length,
    1,
    "the software frame's planes reach the engine",
  );
  assert.equal(
    fake.engine.presented.length,
    0,
    "an HDR frame never goes to the browser's conversion",
  );
  assert.ok(hardware.every((frame) => frame.closed));
  assert.deepEqual(host.readbackRecovery, {
    unreadableFrames: 1,
    readbackRetries: 0,
    redecodedFrames: 1,
    presentedFrames: 0,
  });
  assert.equal(host.sourceTransfer.planarCopyFrames, 1);
});

test("no software decoder gives planar decode unavailable", async () => {
  const host = await createWasmRenderWorkerHost(binding(), PROJECT)("canvas");
  const { player } = await playerWith(host, (options, log) => {
    if (options.decode === "software")
      throw Object.assign(
        new Error("this browser has no software decoder for hvc1"),
        {
          code: "software_decode_unavailable",
        },
      );
    return decoder(() => hlgFrame({ opaque: true }), log);
  });
  await assert.rejects(player.seek(0n), (error) => {
    assert.equal(error.code, "PLANAR_DECODE_UNAVAILABLE");
    assert.equal(error.facts.softwareDecoder, "none");
    assert.equal(error.cause.code, "software_decode_unavailable");
    return true;
  });
});

test("a software frame that still has no planes refuses the source", async () => {
  const host = await createWasmRenderWorkerHost(binding(), PROJECT)("canvas");
  const { player } = await playerWith(host, (_options, log) =>
    decoder(() => hlgFrame({ opaque: true }), log),
  );
  await assert.rejects(player.seek(0n), (error) => {
    assert.equal(error.code, "PLANAR_DECODE_UNAVAILABLE");
    assert.equal(error.facts.softwareDecoder, "opaque");
    assert.equal(error.facts.transfer, "hlg");
    return true;
  });
});

test("an unreadable sdr frame is presented by a host that allows it, and decoded again by default", async () => {
  const sdr = () => {
    const frame = hlgFrame({ unreadable: Infinity });
    frame.format = "NV12";
    frame.colorSpace = {
      primaries: "bt709",
      transfer: "bt709",
      matrix: "bt709",
      fullRange: false,
    };
    return frame;
  };
  const presenting = binding();
  const preview = await createWasmRenderWorkerHost(presenting, PROJECT, {
    unreadableFrames: "present",
  })("canvas");
  await preview.uploadVideoFrame({
    sourceId: "hdr",
    requestedTimeTicks: 0n,
    decodedTimeTicks: 0n,
    generation: 0,
    frame: sdr(),
  });
  assert.equal(presenting.engine.presented.length, 1);
  assert.equal(preview.readbackRecovery.presentedFrames, 1);

  const fake = binding();
  const host = await createWasmRenderWorkerHost(fake, PROJECT)("canvas");
  const { player, requests } = await playerWith(host, (options, log) =>
    decoder(() => {
      if (options.decode !== "software") return sdr();
      const frame = hlgFrame();
      frame.format = "NV12";
      return frame;
    }, log),
  );
  await player.seek(0n);
  assert.deepEqual(
    requests.map((request) => request.decode ?? "default"),
    ["default", "software"],
  );
  assert.equal(fake.engine.yuv.length, 1);
  // Only the frame was unreadable: the next one asks the browser's decoder again.
  assert.equal(host.readbackRecovery.readbackRetries, 2);
});

test("unreadableFrames takes only redecode or present", () => {
  assert.throws(
    () =>
      createWasmRenderWorkerHost(binding(), PROJECT, {
        unreadableFrames: "skip",
      }),
    RangeError,
  );
});
