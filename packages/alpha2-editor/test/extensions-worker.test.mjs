// Import probes, opens and thumbnails of extension-provider files are each one synchronous call
// into the provider module, so each runs in a worker of its own that a cancel or stall terminates;
// on the page thread they would hold preview, playback and input. Workers are faked in-thread here.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import * as browser from "../dist/browser/index.js";
// Read off the namespace, so that each test, not the file, fails against a build without them.
const { createMediaAdapter, EXTENSIONS_CALL_TIMEOUT_MS } = browser;
const installExtensionsWorker = browser.installExtensionsWorker ?? (() => {});
const { callExtensions } =
  await import("../dist/browser/extensions-worker.js").catch(() => ({}));
import { refusedProviderFiles } from "../../../scripts/lib/extension-import-fixtures.mjs";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmDirectory = join(root, "packages", "alpha2", "wasm");
const corpus = join(root, "tests", "corpus", "extensions");
const skip =
  !existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")) &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the extension wasm package is not built",
  );

// What a thumbnail needs of the page: a canvas to encode it and a reader for its data URL.
globalThis.ImageData ??= class {
  constructor(data, width, height) {
    Object.assign(this, { data, width, height });
  }
};
globalThis.OffscreenCanvas ??= class {
  constructor(width, height) {
    Object.assign(this, { width, height });
  }
  getContext() {
    return { putImageData() {}, drawImage() {} };
  }
  async convertToBlob() {
    return new Blob([new Uint8Array(this.width * this.height)]);
  }
};
globalThis.FileReader ??= class {
  readAsDataURL(blob) {
    queueMicrotask(() => {
      this.result = `data:image/png;size=${blob.size}`;
      this.onload();
    });
  }
};

/** Counts the provider module's calls made in this realm, outside a fake worker's handler. */
const pageCalls = { probe: 0, open: 0 };
let inWorker = 0;
let glue;
async function providerModule() {
  if (glue) return glue;
  glue = await import("alpha2/extensions-wasm");
  glue.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
  });
  for (const name of ["probe", "open"]) {
    const original = glue.ExtensionRegistry.prototype[name];
    glue.ExtensionRegistry.prototype[name] = function (...args) {
      if (!inWorker) pageCalls[name] += 1;
      return original.apply(this, args);
    };
  }
  return glue;
}

/**
 * A dedicated worker running the extensions worker's handler; with `broken`, one whose script never
 * loads (an error event and nothing else); with `silent`, one that starts and never answers, as a
 * worker inside a provider call that does not end. Every worker made is listed in `made`.
 */
function fakeWorkers({ broken = false, silent = false } = {}) {
  const made = [];
  const factory = () => {
    const worker = {
      posted: [],
      terminated: false,
      onmessage: null,
      onerror: null,
      terminate() {
        worker.terminated = true;
      },
      postMessage(message) {
        worker.posted.push(message.call ?? message.type);
        if (broken)
          setTimeout(() =>
            worker.onerror?.({ preventDefault() {}, message: undefined }),
          );
        else if (!silent)
          setTimeout(async () => {
            inWorker += 1;
            try {
              await scope.onmessage({ data: message });
            } finally {
              inWorker -= 1;
            }
          });
      },
    };
    const scope = {
      onmessage: null,
      postMessage(answer) {
        if (worker.terminated) return;
        setTimeout(() => worker.onmessage?.({ data: answer }));
      },
    };
    if (!broken) installExtensionsWorker(scope);
    made.push(worker);
    return worker;
  };
  return { factory, made };
}

const fileOf = (name) =>
  new File([readFileSync(join(corpus, name))], name, { type: "image/gif" });

test(
  "an animated image is probed, opened and drawn in the worker the factory makes, and none of it on the page's thread",
  { skip },
  async () => {
    await providerModule();
    pageCalls.probe = pageCalls.open = 0;
    const { factory, made } = fakeWorkers();
    const adapter = createMediaAdapter({ extensionsWorkerFactory: factory });
    const staged = await adapter.stage(
      fileOf("animation.gif"),
      new AbortController().signal,
      { fingerprint: false },
    );
    assert.deepEqual(pageCalls, { probe: 0, open: 0 });
    assert.equal(made.length, 1);
    assert.deepEqual(made[0].posted, ["read"]);
    assert.equal(made[0].terminated, true);
    assert.equal(staged.runtime.kind, "video");
    assert.deepEqual(staged.runtime.provider, { format: "animated_image" });
    assert.equal(staged.runtime.report.format, "gif");
    assert.match(staged.runtime.thumbnail.dataUrl, /^data:image\/png/);
    staged.dispose();
  },
);

test(
  "a cancelled import terminates the worker inside the provider call and rejects at once with the reason",
  { skip },
  async () => {
    await providerModule();
    const { factory, made } = fakeWorkers({ silent: true });
    const adapter = createMediaAdapter({ extensionsWorkerFactory: factory });
    const controller = new AbortController();
    const pending = adapter.stage(fileOf("animation.gif"), controller.signal, {
      fingerprint: false,
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    controller.abort(new DOMException("import cancelled", "AbortError"));
    await assert.rejects(pending, {
      name: "AbortError",
      message: "import cancelled",
    });
    assert.equal(made.length, 1);
    assert.equal(made[0].terminated, true);
  },
);

test("a call past its time limit terminates its worker and fails as provider_stalled", async () => {
  const { factory, made } = fakeWorkers({ silent: true });
  assert.equal(EXTENSIONS_CALL_TIMEOUT_MS, 40_000);
  const started = performance.now();
  await assert.rejects(
    callExtensions(
      { factory },
      { call: "read", file: new File([new Uint8Array(8)], "slow.json") },
      { timeoutMs: 50 },
    ),
    (error) =>
      error.name === "EditorError" &&
      error.code === "provider_stalled" &&
      error.failureCode === "RESOURCE_LIMIT" &&
      /^slow\.json took the extension provider more than 0\.05 s to read/.test(
        error.message,
      ),
  );
  // The call's own 50 ms limit, not the 40 s default; the slack is for a busy machine.
  assert.ok(performance.now() - started < 50 + 10_000);
  assert.equal(made[0].terminated, true);
});

test(
  "a file the provider refuses fails its import as unreadable_media with the provider's reason",
  { skip },
  async () => {
    await providerModule();
    for (const extensionsWorkerFactory of [undefined, fakeWorkers().factory]) {
      const adapter = createMediaAdapter({ extensionsWorkerFactory });
      await assert.rejects(
        adapter.stage(
          new File([refusedProviderFiles["morphology.svg"]], "morphology.svg"),
          new AbortController().signal,
          { fingerprint: false },
        ),
        (error) =>
          error.code === "unreadable_media" &&
          /^Cannot read morphology\.svg: too large: drawing the SVG/.test(
            error.message,
          ),
        extensionsWorkerFactory ? "in a worker" : "on the page's thread",
      );
    }
  },
);

test(
  "a worker that cannot start leaves the call to the page's thread",
  { skip },
  async () => {
    await providerModule();
    const { factory } = fakeWorkers({ broken: true });
    const adapter = createMediaAdapter({ extensionsWorkerFactory: factory });
    pageCalls.probe = pageCalls.open = 0;
    const staged = await adapter.stage(
      fileOf("animation.gif"),
      new AbortController().signal,
      { fingerprint: false },
    );
    assert.equal(staged.runtime.report.format, "gif");
    assert.deepEqual(pageCalls, { probe: 1, open: 1 });
  },
);

test(
  "an OpenEXR sequence's first frame is opened in the worker",
  { skip },
  async () => {
    await providerModule();
    pageCalls.probe = pageCalls.open = 0;
    const { factory, made } = fakeWorkers();
    const adapter = createMediaAdapter({ extensionsWorkerFactory: factory });
    const staged = await adapter.stageSequence(
      ["sequence_0001.exr", "sequence_0002.exr"].map(
        (name) => new File([readFileSync(join(corpus, name))], name),
      ),
      { frameRate: { num: 24, den: 1 }, missing: "reject" },
      new AbortController().signal,
    );
    assert.deepEqual(pageCalls, { probe: 0, open: 0 });
    assert.deepEqual(
      made.map((worker) => worker.posted),
      [["exr-first-frame"]],
    );
    assert.equal(staged.runtime.report.format, "exr_sequence");
    assert.match(staged.runtime.thumbnail.dataUrl, /^data:image\/png/);
  },
);
