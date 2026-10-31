import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";

import {
  backendEnvironment,
  backendProblem,
  countReport,
  defaultBackend,
  defaultCrates,
  judgeDevices,
  logDirectory,
  metalAssertions,
  parseDevices,
  summaryLine,
} from "./gpu-validation-summary.mjs";

const LAVAPIPE =
  "render tests on llvmpipe (LLVM 15.0.7, 256 bits) (Vulkan, driver llvmpipe Mesa 23.2.1 (LLVM 15.0.7))";
const NVIDIA =
  "frame tests on NVIDIA GeForce RTX 4090 (Vulkan, driver NVIDIA 580.95.05)";
const WARP =
  "gpu tests on Microsoft Basic Render Driver (Dx12, driver 10.0.26100.1, FXC)";

test("the summary names adapter, backend and driver", () => {
  const output = `running 3 tests\n${NVIDIA}\ntest a ... ok\n${NVIDIA}\n`;
  const { devices, problems } = judgeDevices({
    output,
    backend: "vulkan",
    opensDevice: true,
    manifest: "/usr/share/vulkan/icd.d/nvidia_icd.json",
  });
  assert.deepEqual(problems, []);
  assert.equal(
    summaryLine("alpha2_engine-0123", devices, problems, "2 known"),
    "ok alpha2_engine-0123 on NVIDIA GeForce RTX 4090 (Vulkan, NVIDIA 580.95.05) (2 known)",
  );
});

test("a binary whose adapter is not the requested ICD fails", () => {
  const { problems } = judgeDevices({
    output: NVIDIA,
    backend: "vulkan",
    opensDevice: true,
    manifest: "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json",
  });
  assert.deepEqual(problems, [
    "NVIDIA GeForce RTX 4090 is not the adapter of lvp_icd.x86_64.json",
  ]);
});

test("a binary without an adapter line fails", () => {
  const { problems } = judgeDevices({
    output: "running 4 tests\ntest result: ok. 4 passed\n",
    backend: "metal",
    opensDevice: true,
  });
  assert.deepEqual(problems, [
    "no adapter line: the binary never opened a device",
  ]);
});

test("an integration test binary may open no device", () => {
  const { devices, problems } = judgeDevices({
    output: "test result: ok. 1 passed\n",
    backend: "vulkan",
    opensDevice: false,
  });
  assert.deepEqual(problems, []);
  assert.equal(
    summaryLine("time_rules-4567", devices, problems, "collector: nothing"),
    "ok time_rules-4567 (no device) (collector: nothing)",
  );
});

test("a device on another backend fails", () => {
  const { problems } = judgeDevices({
    output: `${LAVAPIPE}\n${WARP}\n`,
    backend: "dx12",
    opensDevice: true,
  });
  assert.deepEqual(problems, [
    "llvmpipe (LLVM 15.0.7, 256 bits) is on Vulkan, not Dx12",
  ]);
});

test("an adapter name with parentheses and a driver with them parse", () => {
  assert.deepEqual(parseDevices(`${LAVAPIPE}\n${WARP}\n${LAVAPIPE}\n`), [
    {
      adapter: "llvmpipe (LLVM 15.0.7, 256 bits)",
      backend: "Vulkan",
      driver: "llvmpipe Mesa 23.2.1 (LLVM 15.0.7)",
    },
    {
      adapter: "Microsoft Basic Render Driver",
      backend: "Dx12",
      driver: "10.0.26100.1",
    },
  ]);
});

test("an unknown driver manifest is not checked", () => {
  const { problems } = judgeDevices({
    output: NVIDIA,
    backend: "vulkan",
    opensDevice: true,
    manifest: "/opt/vendor/icd.json",
  });
  assert.deepEqual(problems, []);
});

test("each OS defaults to its product backend and refuses the others", () => {
  assert.equal(defaultBackend("linux"), "vulkan");
  assert.equal(defaultBackend("darwin"), "metal");
  assert.equal(defaultBackend("win32"), "dx12");
  assert.equal(backendProblem("dx12", "win32"), undefined);
  assert.equal(
    backendProblem("metal", "linux"),
    "metal is not available on linux",
  );
  assert.equal(
    backendProblem("dx11", "win32"),
    "unknown backend dx11: one of vulkan, metal, dx12, gl",
  );
  assert.deepEqual(defaultCrates("vulkan"), ["alpha2-render", "alpha2-engine"]);
  assert.equal(defaultCrates("dx12").length, 5);
});

test("every backend validates and reports to the binary's file", () => {
  const base = { PATH: "/bin", VK_KHRONOS_VALIDATION_LOG_FILENAME: "x" };
  const vulkan = backendEnvironment("vulkan", {
    base,
    platform: "linux",
    report: "r.jsonl",
  });
  assert.equal(vulkan.WGPU_BACKEND, "vulkan");
  assert.equal(vulkan.WGPU_VALIDATION, "1");
  assert.equal(vulkan.ALPHA2_VALIDATION_LOG, "report");
  assert.equal(vulkan.ALPHA2_VALIDATION_REPORT, "r.jsonl");
  assert.equal(vulkan.VK_INSTANCE_LAYERS, "VK_LAYER_KHRONOS_validation");
  assert.equal(vulkan.VK_KHRONOS_VALIDATION_LOG_FILENAME, undefined);
  const metal = backendEnvironment("metal", {
    base: { ALPHA2_VALIDATION_LOG: "fail" },
    platform: "darwin",
    report: "m.jsonl",
    shaderValidation: true,
  });
  assert.equal(metal.ALPHA2_VALIDATION_LOG, "fail");
  assert.equal(metal.MTL_DEBUG_LAYER_ERROR_MODE, "assert");
  assert.equal(metal.MTL_SHADER_VALIDATION, "1");
  const dx12 = backendEnvironment("dx12", {
    base: {},
    platform: "win32",
    report: "d.jsonl",
    gpuBasedValidation: true,
  });
  assert.equal(dx12.WGPU_GPU_BASED_VALIDATION, "1");
  assert.equal(dx12.WGPU_DEBUG, undefined);
  const gl = backendEnvironment("gl", {
    base: {},
    platform: "linux",
    report: "g.jsonl",
  });
  assert.equal(gl.LIBGL_ALWAYS_SOFTWARE, "1");
});

test("the log directory names the backend and the driver or adapter", () => {
  assert.equal(
    logDirectory("/logs", "vulkan", {
      VK_ICD_FILENAMES: "/usr/share/vulkan/icd.d/radeon_icd.x86_64.json",
    }),
    join("/logs", "vulkan-radeon_icd.x86_64"),
  );
  assert.equal(
    logDirectory("/logs", "dx12", {
      ALPHA2_GPU_ADAPTER: "Intel(R) Iris(R) Xe",
    }),
    join("/logs", "dx12-intel-r-iris-r-xe"),
  );
  assert.equal(
    logDirectory("/logs", "metal", {}),
    join("/logs", "metal-default"),
  );
});

test("metal assertions and collector records are counted", () => {
  assert.deepEqual(
    metalAssertions(
      "ok\n-[MTLDebugRenderCommandEncoder draw]:123: failed assertion `no pipeline'\n",
    ),
    [
      "-[MTLDebugRenderCommandEncoder draw]:123: failed assertion `no pipeline'",
    ],
  );
  const report = [
    { test: "a", level: "ERROR", target: "wgpu_hal::vulkan", message: "x" },
    { test: "a", level: "WARN", target: "wgpu_hal::vulkan", message: "y" },
    { test: "b", level: "ERROR", target: "wgpu_hal::dx12", message: "z" },
  ]
    .map((record) => JSON.stringify(record))
    .join("\n");
  assert.deepEqual(countReport(`${report}\n`), {
    errors: 2,
    warnings: 1,
    tests: ["a", "b"],
  });
});
