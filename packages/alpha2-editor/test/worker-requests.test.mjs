// The preview worker's request queue (`installEditorWorker` driven by `createWorkerPreview`), over
// an in-process message channel, with its renderer replaced by the recording stub.
import test from "node:test";
import assert from "node:assert/strict";
import "./support/preview-stubs.mjs";
import {
  createWorkerPreview,
  installEditorWorker,
} from "../dist/browser/worker.js";

/** A worker and the scope it runs in, joined as a dedicated worker's ports are. */
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
  // A seek queued behind a busy request, after a replacement, must still be drawn.
  const { scope, worker } = channel();
  const installed = installEditorWorker(scope);
  const preview = createWorkerPreview(worker);
  await preview.ready;
  const [engine] = globalThis.stubEngine.engines.slice(-1);
  await preview.replaceProject(project("first"), 1);
  // The first seek's render is held, as a slow GPU frame is.
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
  // Sent without waiting: the replacement, then a seek.
  const replaced = preview.replaceProject(project("second"), 2);
  const second = preview.seek(60000);
  await new Promise((resolve) => setTimeout(resolve, 0));
  release();
  await replaced;
  // The held render was for what the replacement replaced; the later seek is drawn.
  assert.equal((await first).rendered, true);
  assert.equal((await second).rendered, true);
  assert.equal(engine.renders.at(-1).project.name, "second");
  globalThis.stubEngine.render = async () => {
    globalThis.stubEngine.render = null;
    await new Promise((resolve) => setTimeout(resolve, 5));
  };
  const busy = preview.seek(0);
  const dropped = preview.seek(1);
  // Both seeks are posted (each waits for `ready` first) before the cancel is.
  await new Promise((resolve) => setTimeout(resolve, 0));
  await preview.cancel();
  const after = preview.seek(2);
  await busy;
  assert.equal((await dropped).rendered, false);
  assert.equal((await after).rendered, true);
  await preview.dispose();
  await installed.dispose();
});
