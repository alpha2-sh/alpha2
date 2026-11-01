import assert from "node:assert/strict";
import test from "node:test";

import { Alpha2Exporter, exportToSink } from "../dist/exporter.js";
import {
  createWasmRenderWorkerHost,
  installRenderWorker,
  WorkerExportBackend,
  WorkerRenderBackend,
} from "../dist/worker.js";

/** Reads planes packed from offset 0 whatever `layout` says, as Safari 26 does. */
class LayoutIgnoringVideoFrame {
  constructor(pixels, init) {
    this.codedWidth = init.codedWidth;
    this.codedHeight = init.codedHeight;
    this.timestamp = init.timestamp;
    this.pixels = pixels.slice();
  }
  allocationSize() {
    return (this.codedWidth * this.codedHeight * 3) / 2;
  }
  async copyTo(out) {
    out.set(this.pixels.subarray(0, out.byteLength));
    const width = this.codedWidth;
    const lumaBytes = width * this.codedHeight;
    return [
      { offset: 0, stride: width },
      { offset: lumaBytes, stride: width / 2 },
      { offset: lumaBytes + lumaBytes / 4, stride: width / 2 },
    ];
  }
  close() {}
}
globalThis.VideoFrame = LayoutIgnoringVideoFrame;

const WIDTH = 6;
const HEIGHT = 2;
// The engine's readback pads rows to the GPU copy alignment.
const STRIDE = 16;
const LAYOUT = [
  { offset: 0, stride: STRIDE },
  { offset: STRIDE * HEIGHT, stride: STRIDE },
  { offset: STRIDE * HEIGHT + STRIDE, stride: STRIDE },
];
const TIGHT_BYTES = (WIDTH * HEIGHT * 3) / 2;

function exportBinding(needs) {
  const uploaded = new Set();
  const wasm = {
    deviceLost: false,
    loadProject() {},
    beginExportAt(timeTicks) {
      const missing = (needs.get(timeTicks) ?? []).filter(
        (need) => !uploaded.has(`${need.sourceId}@${need.mediaTimeTicks}`),
      );
      if (missing.length > 0) {
        return {
          kind: "waiting",
          timeTicks: String(timeTicks),
          missingSources: [...new Set(missing.map((need) => need.sourceId))],
          missingFrames: missing,
        };
      }
      return {
        kind: "rendered",
        timeTicks: String(timeTicks),
        retainedBytes: STRIDE * HEIGHT * 2,
        readback: Promise.resolve({
          kind: "rendered",
          timeTicks: String(timeTicks),
          width: WIDTH,
          height: HEIGHT,
          pixels: new Uint8Array(STRIDE * HEIGHT * 2),
          layout: LAYOUT,
          colorSpace: {},
        }),
      };
    },
    free() {},
  };
  return {
    binding: { createForCanvas: async () => wasm },
    upload: (sourceId, mediaTimeTicks) =>
      uploaded.add(`${sourceId}@${mediaTimeTicks}`),
  };
}

function received(message) {
  if (message.frame === undefined) return structuredClone(message);
  const { frame, ...rest } = message;
  const moved = Object.assign(
    Object.create(Object.getPrototypeOf(frame)),
    structuredClone({ ...frame }),
  );
  return { ...structuredClone(rest), frame: moved };
}

async function clonedWorker(createHost) {
  const listeners = new Map();
  const scope = {
    onmessage: null,
    postMessage(message) {
      const data = received(message);
      setImmediate(() => listeners.get("message")?.({ data }));
    },
  };
  installRenderWorker(scope, createHost);
  const worker = {
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener(type) {
      listeners.delete(type);
    },
    postMessage(message) {
      const data = received(message);
      setImmediate(() => scope.onmessage({ data }));
    },
    terminate() {},
  };
  const backend = new WorkerRenderBackend(worker);
  await backend.initialize({});
  return backend;
}

function closingSink() {
  return {
    async writeVideo({ frame }) {
      frame.close();
    },
    async finish() {},
    abort() {},
  };
}

const PROJECT = JSON.stringify({ settings: { width: WIDTH, height: HEIGHT } });

test("a worker export that waits names the source frames the engine needs, so the refill can upload them", async () => {
  const needs = new Map([
    [
      4_000n,
      [
        {
          clipId: "clip",
          readerId: null,
          sourceId: "video",
          mediaTimeTicks: "64000",
        },
        {
          clipId: "pip",
          readerId: "step",
          sourceId: "video",
          mediaTimeTicks: "9000",
        },
      ],
    ],
  ]);
  const { binding, upload } = exportBinding(needs);
  const renderer = await clonedWorker(
    createWasmRenderWorkerHost(binding, PROJECT),
  );
  const exporter = new Alpha2Exporter(
    new WorkerExportBackend(renderer, "h264"),
    { num: 30n, den: 1n },
    0n,
    8_000n,
  );
  const refills = [];
  const result = await exportToSink(exporter, closingSink(), async (step) => {
    refills.push(step);
    for (const frame of step.missingFrames)
      upload(frame.sourceId, String(frame.mediaTimeTicks));
  });
  assert.equal(result.frames, 2);
  assert.equal(refills.length, 1, "one refill uploads what the frame needs");
  const [step] = refills;
  assert.equal(step.index, 1n);
  assert.equal(step.timelineTicks, 4_000n);
  assert.deepEqual(step.missingSources, ["video"]);
  assert.deepEqual(step.missingFrames, [
    { clipId: "clip", sourceId: "video", mediaTimeTicks: 64_000n },
    {
      clipId: "pip",
      readerId: "step",
      sourceId: "video",
      mediaTimeTicks: 9_000n,
    },
  ]);
  renderer.dispose();
});

test("a worker export reports the bytes the worker repacked for a browser that ignores layout", async () => {
  const { binding } = exportBinding(new Map());
  const renderer = await clonedWorker(
    createWasmRenderWorkerHost(binding, PROJECT),
  );
  const exporter = new Alpha2Exporter(
    new WorkerExportBackend(renderer, "h264"),
    { num: 30n, den: 1n },
    0n,
    12_000n,
  );
  const result = await exportToSink(exporter, closingSink(), async () => {});
  assert.equal(result.frames, 3);
  assert.equal(
    result.performance.outputRepackBytes,
    3 * TIGHT_BYTES,
    "each frame was packed tight in the worker",
  );
  renderer.dispose();
});

test("a worker export stamps each VideoFrame with its output time, from the effective range's start", async () => {
  const { binding } = exportBinding(new Map());
  const renderer = await clonedWorker(
    createWasmRenderWorkerHost(binding, PROJECT),
  );
  const exporter = new Alpha2Exporter(
    new WorkerExportBackend(renderer, "h264"),
    { num: 30n, den: 1n },
    1_200_000n,
    1_212_000n,
  );
  const written = [];
  await exportToSink(
    exporter,
    {
      async writeVideo({ timeTicks, frame }) {
        written.push({ timeTicks, timestamp: frame.timestamp });
        frame.close();
      },
      async finish() {},
      abort() {},
    },
    async () => {},
  );
  assert.deepEqual(written, [
    { timeTicks: 0n, timestamp: 0 },
    { timeTicks: 4_000n, timestamp: 33_333 },
    { timeTicks: 8_000n, timestamp: 66_666 },
  ]);
  renderer.dispose();
});
