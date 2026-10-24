import test from "node:test";
import assert from "node:assert/strict";
import { createEditor } from "../dist/index.js";
import {
  ResourceRegistry,
  ExportJob,
  createBrowserBridge,
} from "../dist/browser/index.js";
const deferred = () => {
  let resolve;
  const promise = new Promise((r) => {
    resolve = r;
  });
  return { promise, resolve };
};
test("resource leases are bounded, idempotent and independent by revision", () => {
  const r = new ResourceRegistry(2);
  let closed = 0;
  const a = r.put("s", 0, {}, () => closed++);
  const job = r.acquire("s", 0);
  const b = r.put("s", 1, {}, () => closed++);
  assert.throws(() => r.put("x", 0, {}, () => {}), /budget/);
  a.release();
  assert.equal(closed, 0);
  job.release();
  job.release();
  assert.equal(closed, 1);
  r.dispose();
  b.release();
  r.dispose();
  assert.equal(closed, 2);
});
test("export pins its document, waits for cancellation cleanup, and releases once", async () => {
  const e = createEditor(),
    snapshot = e.document,
    gate = deferred();
  let released = 0,
    observed;
  const backend = {
    async run(doc, signal) {
      observed = doc;
      await gate.promise;
      signal.throwIfAborted();
      return { kind: "blob", blob: new Blob(), name: "a.mp4" };
    },
  };
  const job = new ExportJob(e.revision, snapshot, backend, () => released++);
  await Promise.resolve();
  e.dispatch({ type: "project.update", patch: { name: "changed" } });
  assert.equal(observed, snapshot);
  let stopped = false;
  const stopping = job.cancel().then(() => {
    stopped = true;
  });
  await Promise.resolve();
  assert.equal(stopped, false);
  gate.resolve();
  await stopping;
  assert.equal(job.progress.phase, "cancelled");
  assert.equal(released, 1);
  await job.cancel();
  assert.equal(released, 1);
  await assert.rejects(job.result, { code: "cancelled" });
});
test("project replacement rejects late imports without committing staged resources", async () => {
  const editor = createEditor(),
    gate = deferred();
  let closed = 0;
  const bridge = createBrowserBridge({
    editor,
    mediaAdapter: {
      async stage() {
        await gate.promise;
        return {
          runtime: { id: "s", kind: "image" },
          description: {},
          dispose() {
            closed++;
          },
        };
      },
    },
  });
  const importing = bridge.importFiles([
    { name: "a.png", size: 5, lastModified: 1 },
  ]);
  editor.open(editor.document);
  gate.resolve();
  await assert.rejects(importing);
  assert.equal(bridge.resourceCount, 0);
  assert.equal(Object.keys(editor.document.assets).length, 0);
  assert.equal(closed, 1);
  await bridge.dispose();
  editor.dispose();
});
test("staged batch failure rolls back all media; undo keeps previous resource revision", async () => {
  const editor = createEditor();
  let closed = 0;
  const bridge = createBrowserBridge({
    editor,
    mediaAdapter: {
      async stage(file) {
        if (file.name === "bad") throw Error("decode failed");
        return {
          runtime: { id: file.name, kind: "image" },
          description: {},
          dispose() {
            closed++;
          },
        };
      },
    },
  });
  await assert.rejects(
    bridge.importFiles([
      { name: "good", size: 1, lastModified: 2 },
      { name: "bad" },
    ]),
  );
  assert.equal(bridge.resourceCount, 0);
  assert.equal(closed, 1);
  assert.equal(editor.getSnapshot().canUndo, false);
  await bridge.importFiles([{ name: "good", size: 1, lastModified: 2 }]);
  const imported = editor.document.assets.good.contentRevision;
  await bridge.relink("good", { name: "good", size: 1, lastModified: 2 });
  assert.ok(editor.document.assets.good.contentRevision > imported);
  editor.undo();
  assert.equal(editor.document.assets.good.contentRevision, imported);
  assert.ok(bridge.getAsset("good"));
  await bridge.dispose();
  await bridge.dispose();
  assert.equal(closed, 3);
});

test("session disposal drains its bridge and waits for registered cleanup", async () => {
  const editor = createEditor(),
    gate = deferred();
  let closed = 0;
  const bridge = createBrowserBridge({
    editor,
    mediaAdapter: {
      async stage() {
        return {
          runtime: { id: "owned", kind: "image" },
          description: {},
          dispose() {
            closed++;
          },
        };
      },
    },
  });
  await bridge.importFiles([{ name: "owned", size: 1, lastModified: 1 }]);
  editor.own({ dispose: () => gate.promise });
  const a = editor.dispose(),
    b = editor.dispose();
  assert.equal(a, b);
  await Promise.resolve();
  gate.resolve();
  await a;
  assert.equal(bridge.resourceCount, 0);
  assert.equal(closed, 1);
  await bridge.dispose();
  assert.equal(closed, 1);
});
test("a browser export decode mode other than auto or software is refused before the export starts", async () => {
  const editor = createEditor();
  const bridge = createBrowserBridge({ editor });
  for (const videoDecode of ["hardware", "", 1])
    assert.throws(() => bridge.startExport({ videoDecode }), {
      code: "invalid_video_decode",
    });
  await bridge.dispose();
  await editor.dispose();
});
test("a browser export rate the engine cannot read is refused before the export starts", async () => {
  const editor = createEditor();
  const bridge = createBrowserBridge({ editor });
  for (const frameRate of [29.97, Number.NaN, { num: 0, den: 1 }])
    assert.throws(() => bridge.startExport({ frameRate }), {
      code: "invalid_frame_rate",
    });
  await bridge.dispose();
  await editor.dispose();
});
