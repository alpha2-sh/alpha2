import assert from "node:assert/strict";
import test from "node:test";

import {
  createWasmRenderWorkerHost,
  installRenderWorker,
  WorkerRenderBackend,
} from "../dist/worker.js";

const PROJECT = JSON.stringify({ settings: { width: 2, height: 2 } });

/** A canvas that, as a browser's, keeps the first context kind it gives out for good. */
class FakeCanvas {
  kind = null;
  constructor(name = "canvas") {
    this.name = name;
  }
  getContext(kind) {
    if (this.kind !== null && this.kind !== kind) return null;
    this.kind = kind;
    return { kind };
  }
}

/** A binding object standing in for one renderer kind. */
function fakeRenderer(renderer, backend, adapter) {
  return {
    renderer,
    ...(backend === undefined ? {} : { backend, adapter }),
    deviceLost: false,
    loaded: null,
    loadProject(json) {
      this.loaded = json;
    },
    free() {},
  };
}

/**
 * A binding as the wasm one behaves. `gpu` is what the browser offers: `"hardware"` (WebGPU on a
 * GPU), `"software"` (only a fallback adapter), `"device-fails"` (an adapter whose device does not
 * open), `"none"`, or an error code the constructor throws. The device is opened before the canvas
 * is bound, so a failed or refused one leaves the canvas free.
 */
function binding({ gpu = "hardware", cpu = true } = {}) {
  const made = [];
  return {
    made,
    async createForCanvas(canvas, options = {}) {
      made.push(["gpu", canvas.name, options.softwareAdapter]);
      const fail = (code, message) => {
        const error = new Error(message);
        error.code = code;
        throw error;
      };
      if (gpu === "none")
        fail("GPU_INIT_FAILED", "no adapter and no WebGL2 context");
      if (gpu === "device-fails")
        fail("GPU_INIT_FAILED", "the adapter opened no device");
      if (gpu === "software" && options.softwareAdapter === "refuse")
        fail(
          "GPU_INIT_FAILED",
          "the browser offers only the software adapter google swiftshader",
        );
      if (gpu !== "hardware" && gpu !== "software")
        fail(gpu, `no gpu (${gpu})`);
      if (canvas.getContext("webgpu") === null)
        fail("INVALID_INPUT", "the canvas takes no GPU surface");
      return fakeRenderer(
        "gpu",
        "webgpu",
        gpu === "software" ? "google swiftshader" : "nvidia ada",
      );
    },
    ...(cpu
      ? {
          createCpu(canvas) {
            made.push(["cpu", canvas.name]);
            if (canvas.getContext("2d") === null) {
              const error = new Error("the canvas has no 2D context");
              error.code = "INVALID_INPUT";
              throw error;
            }
            return fakeRenderer("cpu");
          },
        }
      : {}),
  };
}

test("auto draws on the GPU when the browser gives one", async () => {
  const fake = binding();
  const host = await createWasmRenderWorkerHost(
    fake,
    PROJECT,
  )(new FakeCanvas());
  assert.deepEqual(host.rendererChoice, {
    renderer: "gpu",
    rendererReason: null,
    backend: "webgpu",
    adapter: "nvidia ada",
  });
  assert.deepEqual(fake.made, [["gpu", "canvas", "refuse"]]);
});

test("auto after a failed gpu device still gets the cpu on the same canvas", async () => {
  for (const gpu of ["device-fails", "none", "INVALID_INPUT"]) {
    const fake = binding({ gpu });
    const canvas = new FakeCanvas();
    const host = await createWasmRenderWorkerHost(fake, PROJECT)(canvas);
    assert.equal(host.rendererChoice.renderer, "cpu", gpu);
    assert.equal(host.rendererChoice.backend, "cpu");
    assert.equal(host.rendererChoice.adapter, null);
    assert.match(
      host.rendererChoice.rendererReason,
      /no WebGPU adapter and no WebGL2 context/,
    );
    assert.deepEqual(fake.made, [
      ["gpu", "canvas", "refuse"],
      ["cpu", "canvas"],
    ]);
    assert.equal(canvas.kind, "2d");
    // A CPU renderer has no device to lose.
    assert.equal(host.isDeviceLost(), false);
  }
});

test("auto refuses a fallback adapter and draws on the cpu", async () => {
  const fake = binding({ gpu: "software" });
  const canvas = new FakeCanvas();
  const host = await createWasmRenderWorkerHost(fake, PROJECT)(canvas);
  assert.equal(host.rendererChoice.renderer, "cpu");
  assert.match(
    host.rendererChoice.rendererReason,
    /only the software adapter google swiftshader, which renderer "auto" does not draw on/,
  );
  assert.equal(canvas.kind, "2d");
});

test("gpu mode accepts a fallback adapter", async () => {
  const fake = binding({ gpu: "software" });
  const host = await createWasmRenderWorkerHost(fake, PROJECT, {
    renderer: "gpu",
  })(new FakeCanvas());
  assert.deepEqual(host.rendererChoice, {
    renderer: "gpu",
    rendererReason: null,
    backend: "webgpu",
    adapter: "google swiftshader",
  });
  assert.deepEqual(fake.made, [["gpu", "canvas", "allow"]]);
});

test("auto does not hide other failures, nor fall back without a CPU constructor", async () => {
  await assert.rejects(
    createWasmRenderWorkerHost(
      binding({ gpu: "GPU_DEVICE_LOST" }),
      PROJECT,
    )(new FakeCanvas()),
    { code: "GPU_DEVICE_LOST" },
  );
  await assert.rejects(
    createWasmRenderWorkerHost(
      binding({ gpu: "none", cpu: false }),
      PROJECT,
    )(new FakeCanvas()),
    { code: "GPU_INIT_FAILED" },
  );
});

test("a canvas the GPU bound before failing reports the GPU's error, not the CPU's", async () => {
  const canvas = new FakeCanvas();
  canvas.getContext("webgl2");
  await assert.rejects(createWasmRenderWorkerHost(binding(), PROJECT)(canvas), {
    code: "INVALID_INPUT",
    message: /takes no GPU surface/,
  });
});

test("gpu never falls back, and cpu opens no GPU context", async () => {
  await assert.rejects(
    createWasmRenderWorkerHost(binding({ gpu: "none" }), PROJECT, {
      renderer: "gpu",
    })(new FakeCanvas()),
    { code: "GPU_INIT_FAILED" },
  );
  const fake = binding();
  const host = await createWasmRenderWorkerHost(fake, PROJECT, {
    renderer: "cpu",
  })(new FakeCanvas());
  assert.equal(host.rendererChoice.renderer, "cpu");
  assert.match(host.rendererChoice.rendererReason, /asked for/);
  assert.deepEqual(fake.made, [["cpu", "canvas"]]);
  await assert.rejects(
    createWasmRenderWorkerHost(binding({ cpu: false }), PROJECT, {
      renderer: "cpu",
    })(new FakeCanvas()),
    /createCpu/,
  );
  assert.throws(
    () =>
      createWasmRenderWorkerHost(binding(), PROJECT, { renderer: "software" }),
    RangeError,
  );
});

/** A worker whose messages run `installRenderWorker`'s loop in this thread. */
function inProcessWorker(createHost) {
  const scope = {
    onmessage: null,
    postMessage(message) {
      queueMicrotask(() => listeners.get("message")?.({ data: message }));
    },
  };
  installRenderWorker(scope, createHost);
  const listeners = new Map();
  return {
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener() {},
    postMessage(message) {
      scope.onmessage({ data: message });
    },
    terminate() {},
  };
}

test("the worker answers initialize with the renderer, which the backend reports", async () => {
  const backend = new WorkerRenderBackend(
    inProcessWorker(
      createWasmRenderWorkerHost(binding({ gpu: "none" }), PROJECT),
    ),
  );
  assert.equal(backend.rendererChoice, undefined);
  await backend.initialize(new FakeCanvas());
  assert.equal(backend.rendererChoice.renderer, "cpu");
  assert.match(backend.rendererChoice.rendererReason, /no WebGPU adapter/);
  backend.dispose();
});

test("the worker answers the capability probe from its own context", async () => {
  const backend = new WorkerRenderBackend(
    inProcessWorker(createWasmRenderWorkerHost(binding(), PROJECT)),
  );
  // Answered before initialize too: it needs no host.
  const capabilities = await backend.detectCapabilities();
  assert.equal(capabilities.renderer, "cpu");
  assert.equal(capabilities.webgpuFallbackAdapter, null);
  backend.dispose();
});
