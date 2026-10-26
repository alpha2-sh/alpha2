import assert from "node:assert/strict";
import test from "node:test";

import {
  createWasmRenderWorkerHost,
  installRenderWorker,
  WorkerRenderBackend,
} from "../dist/worker.js";

const PROJECT = JSON.stringify({ settings: { width: 2, height: 2 } });

/** A binding object standing in for one renderer kind. */
function fakeRenderer(renderer) {
  return {
    renderer,
    deviceLost: false,
    loaded: null,
    loadProject(json) {
      this.loaded = json;
    },
    free() {},
  };
}

/** A binding whose GPU constructor fails with `gpuCode` (none: it succeeds). */
function binding({ gpuCode, cpu = true } = {}) {
  const made = [];
  return {
    made,
    async createForCanvas(canvas) {
      made.push(["gpu", canvas]);
      if (gpuCode !== undefined) {
        const error = new Error(`no gpu (${gpuCode})`);
        error.code = gpuCode;
        throw error;
      }
      return fakeRenderer("gpu");
    },
    ...(cpu
      ? {
          createCpu(canvas) {
            made.push(["cpu", canvas]);
            return fakeRenderer("cpu");
          },
        }
      : {}),
  };
}

test("auto draws on the GPU when the browser gives one", async () => {
  const fake = binding();
  const host = await createWasmRenderWorkerHost(fake, PROJECT)("canvas");
  assert.deepEqual(host.rendererChoice, {
    renderer: "gpu",
    rendererReason: null,
  });
  assert.deepEqual(fake.made, [["gpu", "canvas"]]);
});

test("auto falls back to the CPU renderer on the same canvas when no GPU renderer can be made", async () => {
  for (const code of ["GPU_INIT_FAILED", "INVALID_INPUT"]) {
    const fake = binding({ gpuCode: code });
    const host = await createWasmRenderWorkerHost(fake, PROJECT)("canvas");
    assert.equal(host.rendererChoice.renderer, "cpu", code);
    assert.match(
      host.rendererChoice.rendererReason,
      /no WebGPU adapter and no WebGL2 context/,
    );
    assert.deepEqual(fake.made, [
      ["gpu", "canvas"],
      ["cpu", "canvas"],
    ]);
    // A CPU renderer has no device to lose.
    assert.equal(host.isDeviceLost(), false);
  }
});

test("auto does not hide other failures, nor fall back without a CPU constructor", async () => {
  await assert.rejects(
    createWasmRenderWorkerHost(
      binding({ gpuCode: "GPU_DEVICE_LOST" }),
      PROJECT,
    )("canvas"),
    { code: "GPU_DEVICE_LOST" },
  );
  await assert.rejects(
    createWasmRenderWorkerHost(
      binding({ gpuCode: "GPU_INIT_FAILED", cpu: false }),
      PROJECT,
    )("canvas"),
    { code: "GPU_INIT_FAILED" },
  );
});

test("gpu never falls back, and cpu opens no GPU context", async () => {
  await assert.rejects(
    createWasmRenderWorkerHost(
      binding({ gpuCode: "GPU_INIT_FAILED" }),
      PROJECT,
      {
        renderer: "gpu",
      },
    )("canvas"),
    { code: "GPU_INIT_FAILED" },
  );
  const fake = binding();
  const host = await createWasmRenderWorkerHost(fake, PROJECT, {
    renderer: "cpu",
  })("canvas");
  assert.equal(host.rendererChoice.renderer, "cpu");
  assert.match(host.rendererChoice.rendererReason, /asked for/);
  assert.deepEqual(fake.made, [["cpu", "canvas"]]);
  await assert.rejects(
    createWasmRenderWorkerHost(binding({ cpu: false }), PROJECT, {
      renderer: "cpu",
    })("canvas"),
    /createCpu/,
  );
  assert.throws(
    () =>
      createWasmRenderWorkerHost(binding(), PROJECT, { renderer: "software" }),
    RangeError,
  );
});

test("the worker answers initialize with the renderer, which the backend reports", async () => {
  const scope = {
    onmessage: null,
    posted: [],
    postMessage(message) {
      this.posted.push(message);
    },
  };
  installRenderWorker(
    scope,
    createWasmRenderWorkerHost(
      binding({ gpuCode: "GPU_INIT_FAILED" }),
      PROJECT,
    ),
  );
  const listeners = new Map();
  const worker = {
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener() {},
    postMessage(message) {
      scope.onmessage({ data: message });
    },
    terminate() {},
  };
  const backend = new WorkerRenderBackend(worker);
  assert.equal(backend.rendererChoice, undefined);
  const initialized = backend.initialize("canvas");
  // Deliver the worker's answers as a worker's messages arrive.
  await new Promise((resolve) => setTimeout(resolve, 0));
  for (const message of scope.posted)
    listeners.get("message")({ data: message });
  await initialized;
  assert.equal(backend.rendererChoice.renderer, "cpu");
  assert.match(backend.rendererChoice.rendererReason, /no WebGPU adapter/);
  backend.dispose();
});
