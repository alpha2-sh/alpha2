import assert from "node:assert/strict";
import test from "node:test";

import { classifyBrowserFailure } from "../dist/export-failure.js";
import { decideRetry, RETRY_PROFILES } from "../dist/export-retry.js";
import { Alpha2Player } from "../dist/player.js";
import {
  Alpha2WasmError,
  createWasmRenderWorkerHost,
  installRenderWorker,
  WorkerRenderBackend,
} from "../dist/worker.js";

test("render worker preserves structured wasm error metadata", async () => {
  const responses = [];
  const scope = {
    onmessage: null,
    postMessage(message) {
      responses.push(message);
    },
  };
  installRenderWorker(scope, async () => {
    const error = new Error("device failed");
    error.code = "GPU_DEVICE_LOST";
    error.fatal = true;
    throw error;
  });

  scope.onmessage({
    data: { id: 7, type: "initialize", canvas: {} },
  });
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.deepEqual(responses, [
    {
      id: 7,
      ok: false,
      error: {
        code: "GPU_DEVICE_LOST",
        message: "device failed",
        fatal: true,
      },
    },
  ]);
});

test("Alpha2WasmError exposes code and fatality", () => {
  const error = new Alpha2WasmError({
    code: "INVALID_INPUT",
    message: "bad frame",
    fatal: false,
  });
  assert.equal(error.name, "Alpha2WasmError");
  assert.equal(error.code, "INVALID_INPUT");
  assert.equal(error.fatal, false);
  assert.equal(error.message, "bad frame");
});

test("a wasm trap is reported as fatal rather than as a retryable failure", async () => {
  // A trapped module cannot serve another call, and the raw trap has no `fatal`: unclassified it
  // would be retried forever.
  for (const [construct, code] of [
    [() => new WebAssembly.RuntimeError("unreachable"), "WASM_TRAP"],
    [() => new WebAssembly.CompileError("bad magic"), "WASM_COMPILE_ERROR"],
    [() => new WebAssembly.LinkError("missing import"), "WASM_LINK_ERROR"],
  ]) {
    const responses = [];
    const scope = {
      onmessage: null,
      postMessage(message) {
        responses.push(message);
      },
    };
    installRenderWorker(scope, async () => {
      throw construct();
    });

    scope.onmessage({ data: { id: 1, type: "initialize", canvas: {} } });
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.equal(responses.length, 1, code);
    assert.equal(responses[0].ok, false, code);
    assert.equal(responses[0].error.code, code);
    assert.equal(responses[0].error.fatal, true, code);
    assert.match(responses[0].error.message, /no longer usable/);
  }
});

for (const message of [
  "transient decode gap",
  "recursive use of an object detected which would lead to unsafe aliasing in rust",
]) {
  test(`an error before any trap stays retryable: ${message}`, async () => {
    const responses = [];
    const scope = {
      onmessage: null,
      postMessage(message) {
        responses.push(message);
      },
    };
    installRenderWorker(scope, async () => {
      throw new Error(message);
    });

    scope.onmessage({ data: { id: 2, type: "initialize", canvas: {} } });
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.deepEqual(responses[0].error, {
      code: "RENDER_WORKER_ERROR",
      message,
      fatal: false,
    });
  });
}

test("errors after a wasm trap stay fatal even when they are plain errors", async () => {
  // wasm-bindgen reports every call after a trap as a plain "recursive use of an object" Error;
  // the worker latches the trap so the follow-on is not offered as retryable.
  const responses = [];
  const scope = {
    onmessage: null,
    postMessage(message) {
      responses.push(message);
    },
  };
  let call = 0;
  installRenderWorker(scope, async () => {
    call += 1;
    if (call === 1) throw new WebAssembly.RuntimeError("unreachable");
    throw new Error(
      "recursive use of an object detected which would lead to unsafe aliasing in rust",
    );
  });

  for (const id of [1, 2, 3]) {
    scope.onmessage({ data: { id, type: "initialize", canvas: {} } });
    await new Promise((resolve) => setTimeout(resolve, 0));
  }

  assert.equal(responses.length, 3);
  assert.equal(responses[0].error.code, "WASM_TRAP");
  assert.equal(responses[0].error.fatal, true);
  for (const index of [1, 2]) {
    assert.equal(responses[index].error.code, "WASM_TRAPPED");
    assert.equal(responses[index].error.fatal, true);
    assert.match(responses[index].error.message, /after an earlier wasm trap/);
  }
});

test("render worker starts later exports while earlier readbacks are pending", async () => {
  const responses = [];
  const started = [];
  const readbacks = [];
  const scope = {
    onmessage: null,
    postMessage(message) {
      responses.push(message);
    },
  };
  installRenderWorker(scope, async () => ({
    uploadVideoFrame() {},
    async present() {
      throw new Error("unused");
    },
    reset() {},
    beginExport(timeTicks) {
      started.push(timeTicks);
      let resolve;
      const readback = new Promise((complete) => {
        resolve = complete;
      });
      readbacks.push(() => resolve({ close() {} }));
      return { kind: "rendered", retainedBytes: 24, readback };
    },
    isDeviceLost() {
      return false;
    },
    async recover() {},
    dispose() {},
  }));

  scope.onmessage({ data: { id: 1, type: "initialize", canvas: {} } });
  await new Promise(setImmediate);
  scope.onmessage({
    data: {
      id: 2,
      type: "begin-export-frame",
      timeTicks: 0n,
      codec: "h264",
      outputTimeTicks: 0n,
    },
  });
  scope.onmessage({
    data: {
      id: 3,
      type: "begin-export-frame",
      timeTicks: 4_000n,
      codec: "h264",
      outputTimeTicks: 4_000n,
    },
  });
  await new Promise(setImmediate);

  assert.deepEqual(started, [0n, 4_000n]);
  assert.deepEqual(responses.slice(1), [
    {
      id: 2,
      ok: true,
      result: { kind: "rendered", retainedBytes: 24 },
    },
    {
      id: 3,
      ok: true,
      result: { kind: "rendered", retainedBytes: 24 },
    },
  ]);

  readbacks[1]();
  await new Promise(setImmediate);
  assert.equal(responses.at(-1).type, "export-frame-ready");
  assert.equal(responses.at(-1).id, 3);
  readbacks[0]();
  await new Promise(setImmediate);
  assert.equal(responses.at(-1).id, 2);
});

test("worker backend pairs split export completion with its request", async () => {
  const listeners = new Map();
  const posted = [];
  const worker = {
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener(type) {
      listeners.delete(type);
    },
    postMessage(message) {
      posted.push(message);
    },
    terminate() {},
  };
  const backend = new WorkerRenderBackend(worker);
  const starting = backend.beginExport(8_000n, "h264", 8_000n);
  const request = posted[0];
  assert.equal(request.type, "begin-export-frame");
  assert.equal(request.timeTicks, 8_000n);
  assert.equal(request.codec, "h264");
  assert.equal(request.outputTimeTicks, 8_000n);

  listeners.get("message")({
    data: {
      id: request.id,
      ok: true,
      result: { kind: "rendered", retainedBytes: 24 },
    },
  });
  const ticket = await starting;
  assert.equal(ticket.kind, "rendered");
  assert.equal(ticket.retainedBytes, 24);
  const frame = { close() {} };
  listeners.get("message")({
    data: {
      type: "export-frame-ready",
      id: request.id,
      ok: true,
      frame,
    },
  });
  assert.equal(await ticket.readback, frame);
  backend.dispose();
});

test("wasm worker host retires uploaded source textures by bytes", async () => {
  const released = [];
  let generation = 0n;
  let waiting = true;
  const pendingReadback = new Promise(() => {});
  const wasm = {
    supportsExternalVideoFrame: true,
    deviceLost: false,
    loadProject() {},
    hdrSource: () => false,
    sourceColorMetadata: () => undefined,
    uploadPresentedVideoFrame() {
      generation += 1n;
      return {
        generation,
        retainedBytes: 15,
      };
    },
    aliasVideoFrame() {
      return false;
    },
    beginExportAt() {
      if (waiting) {
        return {
          kind: "waiting",
          timeTicks: "0",
          missingSources: ["source"],
          missingFrames: [],
        };
      }
      return {
        kind: "rendered",
        retainedBytes: 6,
        readback: pendingReadback,
      };
    },
    releaseFrameGeneration(value) {
      released.push(value);
      return true;
    },
    free() {},
  };
  const binding = {
    async createForCanvas() {
      return wasm;
    },
  };
  const createHost = createWasmRenderWorkerHost(
    binding,
    JSON.stringify({ settings: { width: 2, height: 2 } }),
    { uploadRetention: { maxRetiredBytes: 20 } },
  );
  const host = await createHost({});

  host.uploadVideoFrame({
    sourceId: "source",
    requestedTimeTicks: 0n,
    decodedTimeTicks: 0n,
    generation: 0,
    frame: { duration: null, displayWidth: 2, displayHeight: 2 },
  });
  assert.equal(host.beginExport(0n, "h264", 0n).kind, "waiting");
  assert.deepEqual(released, [], "missing-source retry keeps supplied frames");
  waiting = false;
  host.beginExport(0n, "h264", 0n);
  host.uploadVideoFrame({
    sourceId: "source",
    requestedTimeTicks: 4_000n,
    decodedTimeTicks: 4_000n,
    generation: 0,
    frame: { duration: null, displayWidth: 2, displayHeight: 2 },
  });
  host.beginExport(4_000n, "h264", 4_000n);
  assert.deepEqual(released, [1n]);

  host.dispose();
  assert.deepEqual(
    released,
    [1n],
    "disposing the wasm owner drops its remaining frame store directly",
  );
});

test("wasm worker host aliases repeated physical frames before GPU upload", async () => {
  const uploads = [];
  const aliases = [];
  let nextGeneration = 0n;
  const wasm = {
    supportsExternalVideoFrame: true,
    deviceLost: false,
    loadProject() {},
    hdrSource: () => false,
    sourceColorMetadata: () => undefined,
    uploadPresentedVideoFrame(sourceId, requestedTimeTicks) {
      uploads.push([sourceId, requestedTimeTicks]);
      nextGeneration += 1n;
      return { generation: nextGeneration, retainedBytes: 16 };
    },
    aliasVideoFrame(sourceId, existingTimeTicks, requestedTimeTicks) {
      aliases.push([sourceId, existingTimeTicks, requestedTimeTicks]);
      return true;
    },
    releaseFrameGeneration() {
      return true;
    },
    resetPlayer() {},
    free() {},
  };
  const binding = {
    async createForCanvas() {
      return wasm;
    },
  };
  const host = await createWasmRenderWorkerHost(
    binding,
    JSON.stringify({ settings: { width: 2, height: 2 } }),
  )({});
  const makeFrame = (requestedTimeTicks) => ({
    sourceId: "source",
    requestedTimeTicks,
    decodedTimeTicks: 4_000n,
    generation: 2,
    frame: {
      duration: 33_333,
      displayWidth: 2,
      displayHeight: 2,
      closed: false,
      close() {
        this.closed = true;
      },
    },
  });

  const first = makeFrame(4_000n);
  const repeated = makeFrame(6_000n);
  host.uploadVideoFrame(first);
  host.uploadVideoFrame(repeated);

  assert.deepEqual(uploads, [["source", 4_000n]]);
  assert.deepEqual(aliases, [["source", 4_000n, 6_000n]]);
  assert.equal(first.frame.closed, false);
  assert.equal(repeated.frame.closed, true);
  host.dispose();
});

test("the wasm worker host converts a frame whose color the project declares from its planes, with that color", async () => {
  const declared = {
    dynamic_range: "sdr",
    matrix: "bt601",
    primaries: "bt601",
    range: "full",
    transfer: "bt709",
  };
  const yuv = [];
  const wasm = {
    supportsExternalVideoFrame: true,
    deviceLost: false,
    loadProject() {},
    hdrSource: () => false,
    sourceColorMetadata: (id) =>
      id === "declared" ? JSON.stringify(declared) : undefined,
    uploadPresentedVideoFrame() {
      throw new Error(
        "a declared color must not take the browser's conversion of the frame's own",
      );
    },
    uploadYuvFrame(...args) {
      yuv.push(args);
      return { generation: 1n, retainedBytes: 24 };
    },
    aliasVideoFrame: () => false,
    releaseFrameGeneration: () => true,
    resetPlayer() {},
    free() {},
  };
  const host = await createWasmRenderWorkerHost(
    { createForCanvas: async () => wasm },
    JSON.stringify({ settings: { width: 4, height: 2 } }),
  )({});
  const frame = {
    format: "I420",
    timestamp: 0,
    duration: 33_333,
    displayWidth: 4,
    displayHeight: 2,
    visibleRect: { x: 0, y: 0, width: 4, height: 2 },
    colorSpace: {
      primaries: "bt709",
      transfer: "bt709",
      matrix: "bt709",
      fullRange: false,
    },
    closed: false,
    allocationSize: () => 12,
    async copyTo(buffer) {
      buffer.fill(9);
      return [
        { offset: 0, stride: 4 },
        { offset: 8, stride: 2 },
        { offset: 10, stride: 2 },
      ];
    },
    close() {
      this.closed = true;
    },
  };
  await host.uploadVideoFrame({
    sourceId: "declared",
    requestedTimeTicks: 0n,
    decodedTimeTicks: 0n,
    generation: 1,
    frame,
    chromaSiting: "top-left",
  });
  assert.equal(yuv.length, 1, "the engine converts the frame");
  assert.deepEqual(JSON.parse(yuv[0][10]), declared);
  assert.equal(yuv[0][11], "top-left");
  assert.ok(frame.closed, "the copied frame is closed");
  host.dispose();
});

test("the wasm worker host converts an SDR frame from its planes at its stream's siting, unless it leaves SDR to the browser", async () => {
  const run = async (options) => {
    const yuv = [];
    const direct = [];
    const wasm = {
      supportsExternalVideoFrame: true,
      deviceLost: false,
      loadProject() {},
      hdrSource: () => false,
      sourceColorMetadata: () => undefined,
      uploadPresentedVideoFrame(...args) {
        direct.push(args);
        return { generation: 1n, retainedBytes: 24 };
      },
      uploadYuvFrame(...args) {
        yuv.push(args);
        return { generation: 1n, retainedBytes: 24 };
      },
      aliasVideoFrame: () => false,
      releaseFrameGeneration: () => true,
      resetPlayer() {},
      free() {},
    };
    const host = await createWasmRenderWorkerHost(
      { createForCanvas: async () => wasm },
      JSON.stringify({ settings: { width: 4, height: 2 } }),
      options,
    )({});
    await host.uploadVideoFrame({
      sourceId: "sdr",
      requestedTimeTicks: 0n,
      decodedTimeTicks: 0n,
      generation: 1,
      frame: {
        format: "NV12",
        timestamp: 0,
        duration: 33_333,
        displayWidth: 4,
        displayHeight: 2,
        visibleRect: { x: 0, y: 0, width: 4, height: 2 },
        colorSpace: {
          primaries: "bt709",
          transfer: "bt709",
          matrix: "bt709",
          fullRange: false,
        },
        allocationSize: () => 12,
        async copyTo(buffer) {
          buffer.fill(9);
          return [
            { offset: 0, stride: 4 },
            { offset: 8, stride: 4 },
          ];
        },
        close() {},
      },
      chromaSiting: "left",
    });
    host.dispose();
    return { yuv, direct };
  };
  const engine = await run(undefined);
  assert.equal(engine.direct.length, 0, "the browser does not convert it");
  assert.equal(engine.yuv.length, 1);
  assert.equal(engine.yuv[0][2], "NV12");
  assert.equal(engine.yuv[0][11], "left");
  assert.equal(JSON.parse(engine.yuv[0][10]).dynamic_range, "sdr");
  const browser = await run({ videoConversion: "browser" });
  assert.deepEqual([browser.yuv.length, browser.direct.length], [0, 1]);
  assert.throws(
    () => createWasmRenderWorkerHost({}, "{}", { videoConversion: "canvas" }),
    RangeError,
  );
});

test("a worker player refills when the engine has presented nothing yet", async () => {
  // What `renderAt` returns before the first frame and after every seek: no presented frame.
  const wasm = {
    supportsExternalVideoFrame: true,
    deviceLost: false,
    loadProject() {},
    hdrSource: () => false,
    sourceColorMetadata: () => undefined,
    resetPlayer() {},
    renderAt(timeTicks) {
      return JSON.stringify({
        requestedTimeTicks: Number(timeTicks),
        presentedTimeTicks: null,
        missingSources: ["clip-source"],
        missingFrames: [
          { clipId: "clip", sourceId: "clip-source", mediaTimeTicks: "4000" },
          // A composition step of another clip reads the same source 3 s later.
          {
            clipId: "pip",
            readerId: "step",
            sourceId: "clip-source",
            mediaTimeTicks: "364000",
          },
        ],
        repeated: false,
      });
    },
    free() {},
  };
  const createHost = createWasmRenderWorkerHost(
    { createForCanvas: async () => wasm },
    JSON.stringify({ settings: { width: 2, height: 2 } }),
  );
  const scope = { onmessage: null, postMessage: (data) => page(data) };
  installRenderWorker(scope, createHost);
  let page = () => {};
  const listeners = new Map();
  const worker = {
    addEventListener(type, listener) {
      listeners.set(type, listener);
      if (type === "message") page = (data) => listener({ data });
    },
    removeEventListener(type) {
      listeners.delete(type);
    },
    postMessage(data) {
      queueMicrotask(() => scope.onmessage({ data }));
    },
    terminate() {},
  };
  const refills = [];
  const player = new Alpha2Player(new WorkerRenderBackend(worker), (requests) =>
    refills.push(requests),
  );
  await player.initialize({});
  const presented = await player.seek(4_000n);
  assert.equal(presented.presentedTimeTicks, undefined);
  assert.deepEqual(presented.missingSources, ["clip-source"]);
  assert.equal(refills.length, 1, "the seek asks for the frames it lacks");
  assert.equal(refills[0][0].sourceId, "clip-source");
  assert.deepEqual(
    presented.missingFrames.map(({ clipId, readerId }) => [clipId, readerId]),
    [
      ["clip", undefined],
      ["pip", "step"],
    ],
  );
  assert.deepEqual(
    refills[0].map(({ mediaTimeTicks, consumerId }) => [
      mediaTimeTicks,
      consumerId,
    ]),
    [
      [4_000n, "clip"],
      [364_000n, "pip\u0000step"],
    ],
    "each reader decodes through its own cursor",
  );
  await player.present(4_000n);
  assert.equal(refills.length, 2);
  player.dispose();
});

function fakeWorker() {
  const listeners = new Map();
  const posted = [];
  return {
    listeners,
    posted,
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener(type) {
      listeners.delete(type);
    },
    postMessage(message) {
      posted.push(message);
    },
    terminate() {},
  };
}

test("a render worker that failed stays failed: pending and later requests reject as fatal, and nothing more is posted", async () => {
  // An uncaught worker error leaves its serial queue behind an await that never settles.
  const worker = fakeWorker();
  const backend = new WorkerRenderBackend(worker);
  const pending = backend.present(0n);
  worker.listeners.get("error")({
    message: "Uncaught RuntimeError: unreachable",
  });
  await assert.rejects(pending, (error) => {
    assert.equal(error.code, "RENDER_WORKER_FAILED");
    assert.equal(error.fatal, true);
    assert.match(error.message, /unreachable/);
    return true;
  });
  const posted = worker.posted.length;
  for (const later of [
    // A worker whose script failed to load or evaluate raises this before its first answer: the
    // host's first request is then initialize().
    () => backend.initialize({}),
    () => backend.present(0n),
    () => backend.recover(),
    () => backend.reset(),
    () => backend.beginExport(0n, "h264", 0n),
  ]) {
    await assert.rejects(later(), (error) => {
      assert.equal(error.code, "RENDER_WORKER_FAILED");
      assert.equal(error.fatal, true);
      return true;
    });
  }
  assert.equal(
    worker.posted.length,
    posted,
    "a failed worker is not posted to",
  );
  backend.dispose();
});

test("the render worker reports a device loss before the answer that found it", async () => {
  // A player checks isDeviceLost() as soon as its request settles, so the loss must arrive first.
  const responses = [];
  const scope = {
    onmessage: null,
    postMessage(message) {
      responses.push(message);
    },
  };
  let lost = false;
  installRenderWorker(scope, async () => ({
    present: async () => {
      lost = true;
      return {
        requestedTimeTicks: 0n,
        missingSources: [],
        missingFrames: [],
        repeated: false,
      };
    },
    isDeviceLost: () => lost,
    dispose() {},
  }));
  scope.onmessage({ data: { id: 1, type: "initialize", canvas: {} } });
  scope.onmessage({ data: { id: 2, type: "present", timeTicks: 0n } });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(
    responses.map((response) => response.type ?? `reply ${response.id}`),
    ["reply 1", "device-lost", "reply 2"],
  );
});

test("the render worker reports a device loss before an export frame's answer, waiting or rendered", async () => {
  // An all-resident export sends no upload to carry the loss; the exporter checks isDeviceLost().
  for (const kind of ["waiting", "rendered"]) {
    const responses = [];
    const scope = {
      onmessage: null,
      postMessage(message) {
        responses.push(message);
      },
    };
    let lost = false;
    installRenderWorker(scope, async () => ({
      beginExport() {
        lost = true;
        return kind === "waiting"
          ? { kind, missingSources: ["source"], missingFrames: [] }
          : { kind, retainedBytes: 6, readback: new Promise(() => {}) };
      },
      isDeviceLost: () => lost,
      dispose() {},
    }));
    scope.onmessage({ data: { id: 1, type: "initialize", canvas: {} } });
    scope.onmessage({
      data: {
        id: 2,
        type: "begin-export-frame",
        timeTicks: 0n,
        codec: "h264",
        outputTimeTicks: 0n,
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.deepEqual(
      responses.map((response) => response.type ?? `reply ${response.id}`),
      ["reply 1", "device-lost", "reply 2"],
      kind,
    );
  }
});

test("a wasm worker host whose recovery failed answers as lost, never through the freed binding, and recovers later", async () => {
  const freed = [];
  const made = [];
  const registry = (name) => {
    const wasm = {
      name,
      deviceLost: false,
      loadProject() {},
      renderAt() {
        if (freed.includes(name))
          throw new Error("null pointer passed to rust");
        return JSON.stringify({
          requestedTimeTicks: 0,
          presentedTimeTicks: 0,
          missingSources: [],
          missingFrames: [],
          repeated: false,
        });
      },
      free() {
        freed.push(name);
      },
    };
    made.push(wasm);
    return wasm;
  };
  let failNext = false;
  const binding = {
    async createForCanvas() {
      if (failNext) {
        failNext = false;
        throw Object.assign(new Error("the WebGL context was not restored"), {
          code: "GPU_DEVICE_LOST",
          fatal: true,
        });
      }
      return registry(`registry ${made.length + 1}`);
    },
  };
  const host = await createWasmRenderWorkerHost(
    binding,
    JSON.stringify({ settings: { width: 2, height: 2 } }),
  )({});
  made[0].deviceLost = true;
  assert.equal(host.isDeviceLost(), true);
  failNext = true;
  await assert.rejects(host.recover(), /not restored/);
  assert.deepEqual(freed, ["registry 1"]);
  assert.equal(host.isDeviceLost(), true, "no binding is a lost one");
  await assert.rejects(host.present(0n), (error) => {
    assert.equal(error.code, "GPU_DEVICE_LOST");
    assert.equal(error.fatal, true);
    return true;
  });
  assert.throws(
    () => host.beginExport(0n, "h264", 0n),
    (error) => error.code === "GPU_DEVICE_LOST",
  );
  await host.recover();
  assert.equal(host.isDeviceLost(), false);
  assert.equal((await host.present(0n)).requestedTimeTicks, 0n);
  host.dispose();
  assert.deepEqual(freed, ["registry 1", "registry 2"]);
});

/** A render worker and its backend joined by structured clone, as a real Worker joins them. */
async function clonedWorker(host) {
  const listeners = new Map();
  const scope = {
    onmessage: null,
    postMessage(message) {
      const data = structuredClone(message);
      setImmediate(() => listeners.get("message")?.({ data }));
    },
  };
  installRenderWorker(scope, async () => host);
  const worker = {
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener(type) {
      listeners.delete(type);
    },
    postMessage(message) {
      const data = structuredClone(message);
      setImmediate(() => scope.onmessage({ data }));
    },
    terminate() {},
  };
  const backend = new WorkerRenderBackend(worker);
  await backend.initialize({});
  return backend;
}

function hostThrowing({ present, readback }) {
  return {
    uploadVideoFrame() {},
    async present() {
      throw present();
    },
    reset() {},
    beginExport() {
      return {
        kind: "rendered",
        retainedBytes: 24,
        readback: Promise.reject(readback()),
      };
    },
    isDeviceLost() {
      return false;
    },
    async recover() {},
    dispose() {},
  };
}

test("a readback failure keeps the engine's failure, name, and class across the render worker", async () => {
  const engineError = () => {
    // The binding's readback rejection: its code says nothing of memory, its `failure` does.
    const error = new Error(
      "export frame readback failed to map: GPU texture readback failed: map-failed",
    );
    error.name = "Alpha2Error";
    error.code = "ALPHA2_ERROR";
    error.fatal = false;
    error.failure = {
      code: "OUT_OF_MEMORY",
      phase: "readback",
      stage: "map-failed",
      resource: "gpu",
      frameIndex: 7,
      timeTicks: "8000",
      gpu: { events: [{ kind: "out-of-memory", message: "staging" }] },
    };
    return error;
  };
  const backend = await clonedWorker(
    hostThrowing({
      present: () => new Error("unused"),
      readback: engineError,
    }),
  );
  const ticket = await backend.beginExport(8_000n, "h264", 8_000n);
  const error = await ticket.readback.then(
    () => assert.fail("the readback resolved"),
    (cause) => cause,
  );
  assert.equal(error.code, "ALPHA2_ERROR");
  assert.equal(error.name, "Alpha2Error");
  assert.equal(error.failure?.code, "OUT_OF_MEMORY");
  const failure = classifyBrowserFailure(error, {
    phase: "render",
    backend: "browser-webgpu",
  });
  assert.equal(failure.code, "OUT_OF_MEMORY");
  assert.equal(failure.phase, "readback");
  assert.equal(failure.stage, "map-failed");
  assert.equal(failure.frameIndex, 7);
  assert.equal(failure.resource, "gpu");
  assert.deepEqual(failure.diagnostics.gpu, {
    events: [{ kind: "out-of-memory", message: "staging" }],
  });
  const decision = decideRetry([{ failure, profile: RETRY_PROFILES[0] }], {
    backoffMs: () => 0,
  });
  assert.equal(decision.retry, true);
  assert.equal(decision.profile.name, "reduced");
  backend.dispose();
});

test("a platform error keeps its name and type evidence across the render worker", async () => {
  for (const [make, phase, expected] of [
    [
      () =>
        new DOMException("The quota has been exceeded.", "QuotaExceededError"),
      "publish",
      ["STORAGE_QUOTA_EXCEEDED", "unknown"],
    ],
    [
      () => new RangeError("Array buffer allocation failed"),
      "render",
      ["ALLOCATION_FAILED", "js"],
    ],
  ]) {
    const backend = await clonedWorker(
      hostThrowing({ present: make, readback: make }),
    );
    const error = await backend.present(0n).then(
      () => assert.fail("present resolved"),
      (cause) => cause,
    );
    const failure = classifyBrowserFailure(error, { phase });
    assert.deepEqual([failure.code, failure.resource], expected, error.name);
    backend.dispose();
  }
});

test("a failure that cannot be cloned does not stop the render worker answering", async () => {
  const backend = await clonedWorker(
    hostThrowing({
      present: () =>
        Object.assign(new Error("odd"), {
          code: "ALPHA2_ERROR",
          failure: { code: "UNKNOWN", phase: "render", hook() {} },
        }),
      readback: () => new Error("unused"),
    }),
  );
  const error = await backend.present(0n).then(
    () => assert.fail("present resolved"),
    (cause) => cause,
  );
  assert.equal(error.code, "ALPHA2_ERROR");
  assert.equal(error.message, "odd");
  backend.dispose();
});

test("the render worker recovers only a host that lost its device", async () => {
  const responses = [];
  const scope = {
    onmessage: null,
    postMessage(message) {
      responses.push(message);
    },
  };
  let lost = true;
  let recoveries = 0;
  installRenderWorker(scope, async () => ({
    isDeviceLost: () => lost,
    async recover() {
      recoveries += 1;
      lost = false;
    },
    dispose() {},
  }));
  scope.onmessage({ data: { id: 1, type: "initialize", canvas: {} } });
  // Two players, or one player twice, ask for a recovery from the same loss.
  scope.onmessage({ data: { id: 2, type: "recover" } });
  scope.onmessage({ data: { id: 3, type: "recover" } });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(recoveries, 1);
  assert.deepEqual(
    responses
      .filter((message) => "id" in message)
      .map(({ id, ok }) => [id, ok]),
    [
      [1, true],
      [2, true],
      [3, true],
    ],
  );
});

function closableFrame() {
  return {
    sourceId: "source",
    requestedTimeTicks: 0n,
    decodedTimeTicks: 0n,
    generation: 0,
    frame: {
      duration: 33_333,
      displayWidth: 2,
      displayHeight: 2,
      closed: false,
      close() {
        this.closed = true;
      },
    },
  };
}

test("a frame the render backend cannot post, disposed or failed, is closed", async () => {
  const disposed = new WorkerRenderBackend(fakeWorker());
  disposed.dispose();
  const first = closableFrame();
  await assert.rejects(disposed.uploadVideoFrame(first), /disposed/);
  assert.equal(first.frame.closed, true);

  const worker = fakeWorker();
  const failed = new WorkerRenderBackend(worker);
  worker.listeners.get("error")({ message: "trap" });
  const second = closableFrame();
  await assert.rejects(failed.uploadVideoFrame(second), /must be replaced/);
  assert.equal(second.frame.closed, true);
  failed.dispose();
});

test("a frame sent to a render worker with no host is closed there", async () => {
  const responses = [];
  const scope = {
    onmessage: null,
    postMessage(message) {
      responses.push(message);
    },
  };
  installRenderWorker(scope, async () => {
    throw new Error("no adapter");
  });
  scope.onmessage({ data: { id: 1, type: "initialize", canvas: {} } });
  const sent = closableFrame();
  scope.onmessage({
    data: { id: 2, type: "upload-video-frame", ...sent },
  });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(responses.at(-1).ok, false);
  assert.match(responses.at(-1).error.message, /not initialized/);
  assert.equal(sent.frame.closed, true);
});

test("a wasm worker host that refuses a frame before uploading it closes it", async () => {
  const wasm = {
    supportsExternalVideoFrame: true,
    deviceLost: false,
    loadProject() {},
    hdrSource: () => false,
    sourceColorMetadata() {
      throw new Error("unreachable");
    },
    aliasVideoFrame: () => false,
    releaseFrameGeneration: () => true,
    resetPlayer() {},
    free() {},
  };
  const host = await createWasmRenderWorkerHost(
    { createForCanvas: async () => wasm },
    JSON.stringify({ settings: { width: 2, height: 2 } }),
  )({});
  const refused = closableFrame();
  await assert.rejects(host.uploadVideoFrame(refused), /unreachable/);
  assert.equal(refused.frame.closed, true);
  host.dispose();
});
