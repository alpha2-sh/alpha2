import assert from "node:assert/strict";
import test from "node:test";

import {
  BrowserRobustnessCoordinator,
  detectBrowserCapabilities,
} from "../dist/robustness.js";

function coordinator(budget = { textureBytes: 1024, decoderSessions: 2 }) {
  const diagnostics = [];
  return [
    new BrowserRobustnessCoordinator(budget, (diagnostic) =>
      diagnostics.push(diagnostic),
    ),
    diagnostics,
  ];
}

test("capability detection answers without a browser and reports the gaps", async () => {
  const capabilities = await detectBrowserCapabilities();
  assert.equal(capabilities.webgpu, false);
  assert.equal(capabilities.webgl2, false);
  assert.equal(capabilities.webcodecs, false);
  assert.equal(capabilities.offscreenCanvas, false);
  assert.deepEqual(capabilities.diagnostics, [
    "webgpu-unavailable",
    "webcodecs-unavailable",
    "offscreen-canvas-unavailable",
  ]);
});

/** Runs `body` with `navigator.gpu` set to `gpu`, then takes it away again. */
async function withGpu(gpu, body) {
  Object.defineProperty(globalThis.navigator, "gpu", {
    value: gpu,
    configurable: true,
  });
  try {
    return await body();
  } finally {
    delete globalThis.navigator.gpu;
  }
}

test("WebGPU is reported only when the browser hands out an adapter", async () => {
  // An API with no adapter (no flag, blocklisted GPU, acceleration off) runs on WebGL2.
  for (const [label, requestAdapter, expected] of [
    ["an adapter", async () => ({}), true],
    ["no adapter", async () => null, false],
    [
      "a refused request",
      async () => {
        throw new Error("refused");
      },
      false,
    ],
  ]) {
    const capabilities = await withGpu({ requestAdapter }, () =>
      detectBrowserCapabilities(),
    );
    assert.equal(capabilities.webgpu, expected, label);
    assert.equal(
      capabilities.diagnostics.includes("webgpu-unavailable"),
      !expected,
      label,
    );
  }
});

test("WebAssembly SIMD and reference types are probed before the engine is loaded", async () => {
  const validate = WebAssembly.validate;
  const probes = [];
  try {
    // Chrome 94 and 95 have WebCodecs but no reference types, so the packaged modules cannot compile.
    for (const [accepts, wasm, missing] of [
      [() => true, true, []],
      [
        () => false,
        false,
        ["wasm-simd-unavailable", "wasm-reference-types-unavailable"],
      ],
      [
        (bytes) => bytes.length !== 27,
        false,
        ["wasm-reference-types-unavailable"],
      ],
      [(bytes) => bytes.length !== 31, false, ["wasm-simd-unavailable"]],
    ]) {
      WebAssembly.validate = (bytes) => {
        probes.push(bytes.length);
        return accepts(bytes);
      };
      const capabilities = await detectBrowserCapabilities();
      assert.equal(capabilities.wasm, wasm);
      assert.deepEqual(
        capabilities.diagnostics.filter((kind) => kind.startsWith("wasm-")),
        missing,
      );
    }
    assert.ok(probes.length >= 8, "validate is called for each probe");
  } finally {
    WebAssembly.validate = validate;
  }
  // The probes themselves are what the real validator accepts here (Node has both features).
  const capabilities = await detectBrowserCapabilities();
  assert.equal(capabilities.wasm, true);
});

test("WebGL2 is probed on a canvas of its own, never one the caller passes", async () => {
  const contexts = [];
  const canvas = (owner) => ({
    getContext(kind) {
      contexts.push([owner, kind]);
      return {};
    },
  });
  globalThis.document = { createElement: () => canvas("probe") };
  try {
    const callers = canvas("caller");
    const capabilities = await detectBrowserCapabilities(callers);
    assert.equal(capabilities.webgl2, true);
    assert.deepEqual(contexts, [["probe", "webgl2"]]);
  } finally {
    delete globalThis.document;
  }
});

test("a non-positive resource budget is rejected", () => {
  for (const budget of [
    { textureBytes: 0, decoderSessions: 1 },
    { textureBytes: 1024, decoderSessions: 0 },
    { textureBytes: 1.5, decoderSessions: 1 },
  ]) {
    assert.throws(
      () => new BrowserRobustnessCoordinator(budget, () => {}),
      RangeError,
    );
  }
});

test("decoder sessions are bounded by the budget", () => {
  const [robustness, diagnostics] = coordinator();
  assert.equal(robustness.acquireDecoder(), true);
  assert.equal(robustness.acquireDecoder(), true);
  assert.equal(robustness.acquireDecoder(), false);
  assert.deepEqual(diagnostics, ["decoder-session-limit"]);

  robustness.releaseDecoder();
  assert.equal(robustness.acquireDecoder(), true);
  assert.deepEqual(diagnostics, ["decoder-session-limit"]);
});

test("releasing more decoders than were acquired stays at zero", () => {
  const [robustness] = coordinator();
  robustness.releaseDecoder();
  robustness.releaseDecoder();
  assert.equal(robustness.acquireDecoder(), true);
  assert.equal(robustness.acquireDecoder(), true);
  assert.equal(robustness.acquireDecoder(), false);
});

test("memory pressure halves the texture budget and reports itself", () => {
  const [robustness, diagnostics] = coordinator();
  assert.equal(robustness.memoryPressure(), 512);
  assert.deepEqual(diagnostics, ["memory-pressure"]);
});

test("only the visible-to-hidden edge is reported", () => {
  const [robustness, diagnostics] = coordinator();
  robustness.setVisible(true);
  assert.deepEqual(diagnostics, []);
  robustness.setVisible(false);
  robustness.setVisible(false);
  assert.deepEqual(diagnostics, ["visibility-suspended"]);
  robustness.setVisible(true);
  robustness.setVisible(false);
  assert.deepEqual(diagnostics, [
    "visibility-suspended",
    "visibility-suspended",
  ]);
});

test("context loss is forwarded to the host", () => {
  const [robustness, diagnostics] = coordinator();
  robustness.contextLost();
  assert.deepEqual(diagnostics, ["context-lost"]);
});

test("resize scales CSS pixels and never produces a zero-sized surface", () => {
  const [robustness] = coordinator();
  const canvas = { width: 0, height: 0 };
  robustness.resize(canvas, 640, 360, 2);
  assert.deepEqual(canvas, { width: 1280, height: 720 });
  robustness.resize(canvas, 0.2, 0.2);
  assert.deepEqual(canvas, { width: 1, height: 1 });
  assert.throws(() => robustness.resize(canvas, Number.NaN, 1), RangeError);
});
