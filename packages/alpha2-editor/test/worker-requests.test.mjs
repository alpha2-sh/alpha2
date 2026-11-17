import assert from "node:assert/strict";
import "./support/preview-stubs.mjs";
import { testUnless, wasmSkip } from "./support/wasm.mjs";

const skip = wasmSkip(import.meta.url);
const test = testUnless(skip);
const { createWorkerPreview, installEditorWorker } = skip
  ? {}
  : await import("../dist/browser/worker.js");

function channel() {
  const scope = { onmessage: null, postMessage: null };
  const worker = {
    onmessage: null,
    onerror: null,
    terminated: false,
    postMessage(data) {
      queueMicrotask(() => scope.onmessage?.({ data }));
    },
    terminate() {
      worker.terminated = true;
    },
  };
  scope.postMessage = (data) =>
    queueMicrotask(() => worker.onmessage?.({ data }));
  return { scope, worker };
}

const project = (name) => ({
  schema_version: 1,
  id: "p",
  name,
  settings: {
    width: 16,
    height: 16,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48000,
  },
  sources: [],
  timeline: {
    tracks: [
      {
        id: "t",
        name: "Captions",
        kind: "caption",
        clips: [],
        captions: {
          cues: [{ timeline_range: { start: 0, end: 120000 }, text: name }],
        },
      },
    ],
  },
});

test("a seek sent after a project replacement renders the new project, however long it waited in the queue", async () => {
  const { scope, worker } = channel();
  const installed = installEditorWorker(scope);
  const preview = createWorkerPreview(worker);
  await preview.ready;
  const [engine] = globalThis.stubEngine.engines.slice(-1);
  await preview.replaceProject(project("first"), 1);
  let release;
  const held = new Promise((resolve) => {
    release = resolve;
  });
  globalThis.stubEngine.render = async () => {
    globalThis.stubEngine.render = null;
    await held;
  };
  const first = preview.seek(0);
  await new Promise((resolve) => setTimeout(resolve, 0));
  const replaced = preview.replaceProject(project("second"), 2);
  const second = preview.seek(60000);
  await new Promise((resolve) => setTimeout(resolve, 0));
  release();
  await replaced;
  assert.equal((await first).rendered, true);
  assert.equal((await second).rendered, true);
  assert.equal(engine.renders.at(-1).project.name, "second");
  globalThis.stubEngine.render = async () => {
    globalThis.stubEngine.render = null;
    await new Promise((resolve) => setTimeout(resolve, 5));
  };
  const busy = preview.seek(0);
  const dropped = preview.seek(1);
  await new Promise((resolve) => setTimeout(resolve, 0));
  await preview.cancel();
  const after = preview.seek(2);
  await busy;
  assert.equal((await dropped).rendered, false);
  assert.equal((await after).rendered, true);
  await preview.dispose();
  await installed.dispose();
});

test("a seek reply carries a new renderer diagnostics sample only, and no bitmap for an unchanged frame", async () => {
  const { scope, worker } = channel();
  const installed = installEditorWorker(scope);
  const preview = createWorkerPreview(worker, { bitmapOutput: true });
  await preview.ready;
  const [engine] = globalThis.stubEngine.engines.slice(-1);
  const loaded = await preview.replaceProject(project("first"), 1);
  // The canvas takes the render size: half the 16x16 document by default.
  assert.deepEqual(loaded.renderSize, [8, 8]);
  assert.deepEqual([engine.canvas.width, engine.canvas.height], [8, 8]);
  engine.lastDiagnostics = { host: { renders: 1 } };
  const first = await preview.seek(0);
  assert.deepEqual(first.engineDiagnostics, { host: { renders: 1 } });
  assert.equal(first.unchanged, false);
  assert.ok(first.bitmap, "a drawn frame comes as a bitmap");
  const second = await preview.seek(4000);
  assert.equal(
    second.engineDiagnostics,
    undefined,
    "the same sample is not sent again",
  );
  globalThis.stubEngine.unchanged = () => true;
  try {
    const same = await preview.seek(8000);
    assert.deepEqual([same.rendered, same.unchanged], [true, true]);
    assert.equal(same.bitmap, undefined);
  } finally {
    globalThis.stubEngine.unchanged = null;
  }
  const scaled = await preview.setPreviewScale(1);
  assert.deepEqual(scaled.renderSize, [16, 16]);
  assert.deepEqual([engine.canvas.width, engine.canvas.height], [16, 16]);
  await preview.setTracing({ passTiming: true, traceUploads: true });
  assert.equal(engine.passTiming, true);
  assert.equal(engine.traceUploads, true);
  await preview.setTracing({ passTiming: false });
  assert.deepEqual([engine.passTiming, engine.traceUploads], [false, true]);
  await preview.dispose();
  await installed.dispose();
});

test("a worker entry evaluated again while init loads the engine keeps the first handler", async () => {
  const { scope, worker } = channel();
  const installed = installEditorWorker(scope);
  let again;
  globalThis.stubEngine.start = async () => {
    globalThis.stubEngine.start = null;
    again = installEditorWorker(scope);
  };
  const preview = createWorkerPreview(worker);
  await preview.ready;
  assert.equal(again, installed);
  const reply = await preview.replaceProject(project("after"), 1);
  assert.deepEqual(reply.renderSize, [8, 8]);
  await preview.dispose();
  await installed.dispose();
  assert.notEqual(installEditorWorker(scope), installed);
});

test("an error reply that cannot be posted is replaced by a plain one and later requests are still answered", async () => {
  const posted = [];
  let failNextError = true;
  const scope = {
    onmessage: null,
    postMessage(message) {
      if (!message.ok && failNextError) {
        failNextError = false;
        throw new DOMException(
          "the reply could not be cloned",
          "DataCloneError",
        );
      }
      posted.push([message.requestId, message.ok, message.error?.code]);
    },
  };
  const installed = installEditorWorker(scope);
  const send = (type, requestId) =>
    scope.onmessage({
      data: {
        type,
        sessionId: "s",
        revision: 0,
        requestId,
        runtimeAssets: {},
        width: 16,
        height: 16,
      },
    });
  send("init", 1);
  send("no-such-request", 2);
  send("cancel", 3);
  send("set-preview-scale", 4);
  for (let i = 0; i < 100 && posted.length < 4; i++)
    await new Promise((resolve) => setTimeout(resolve, 2));
  assert.deepEqual(posted, [
    [1, true, undefined],
    [2, false, "worker_error"],
    [3, true, undefined],
    [4, true, undefined],
  ]);
  await installed.dispose();
});

/** A worker that answers nothing, recording what it was sent. */
function silentWorker() {
  const worker = {
    requests: [],
    onmessage: null,
    onerror: null,
    onmessageerror: null,
    terminated: false,
    postMessage(message) {
      worker.requests.push(message.type);
    },
    terminate() {
      worker.terminated = true;
    },
  };
  return worker;
}

test("disposing a preview whose worker never answers terminates it within a second", async () => {
  const worker = silentWorker();
  const preview = createWorkerPreview(worker);
  preview.ready.catch(() => {});
  const seeking = preview.seek(0);
  seeking.catch(() => {});
  const started = performance.now();
  await preview.dispose();
  assert.ok(performance.now() - started < 1500);
  assert.ok(worker.terminated);
  assert.deepEqual(worker.requests, ["init", "dispose"]);
  await assert.rejects(seeking);
  await assert.rejects(preview.seek(0));
});

test("a worker error fails pending and later calls at once", async () => {
  for (const event of ["onerror", "onmessageerror"]) {
    const worker = silentWorker();
    const preview = createWorkerPreview(worker);
    preview.ready.catch(() => {});
    worker[event]({ message: "the worker script failed" });
    await assert.rejects(preview.ready);
    const before = worker.requests.length;
    await assert.rejects(
      preview.cancel(),
      /the worker script failed|could not/,
    );
    assert.equal(worker.requests.length, before, event);
    await preview.dispose();
  }
});

test("a request unanswered past its active-time deadline fails the worker with a fatal timeout", async () => {
  const worker = silentWorker();
  const preview = createWorkerPreview(worker, { requestTimeoutMs: 300 });
  await assert.rejects(
    preview.ready,
    (error) => error.code === "worker_timeout" && error.fatal === true,
  );
  const before = worker.requests.length;
  await assert.rejects(
    preview.cancel(),
    (error) => error.code === "worker_timeout",
  );
  assert.equal(worker.requests.length, before);
  await preview.dispose();
  assert.ok(worker.terminated);
});
