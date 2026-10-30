// The probe's adapter line, and a probe whose browser cannot start.
import assert from "node:assert/strict";
import test from "node:test";

import { BrowserStartError } from "./browser-errors.mjs";
import { adapterLine, probeBrowser } from "./browser-probe.mjs";

const browser = {
  name: "chrome",
  version: "156.0.7300.10",
  path: "/usr/bin/google-chrome",
  headed: false,
  gpu: "swiftshader",
  settings: "harness",
  flags: ["--enable-unsafe-swiftshader", "--use-angle=swiftshader"],
};

test("the adapter line names every field in order", () => {
  const line = adapterLine({
    browser,
    webgpu: {
      present: true,
      adapter: {
        vendor: "google",
        architecture: "swiftshader",
        device: "",
        description: "SwiftShader Device (Subzero)",
        isFallbackAdapter: true,
      },
    },
    webgl2: { renderer: "ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device))" },
    engine: { backend: "BrowserWebGpu", name: "SwiftShader", driver: "" },
  });
  assert.equal(
    line,
    'browser-run: browser=chrome version=156.0.7300.10 mode=headless gpu=swiftshader settings=harness flags="--enable-unsafe-swiftshader --use-angle=swiftshader" webgpu="google swiftshader SwiftShader Device (Subzero)" fallback=true webgl2="ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device))" engine=BrowserWebGpu engine-adapter="SwiftShader ()"',
  );
});

test("a report without WebGPU prints webgpu=none and fallback=n/a", () => {
  const line = adapterLine({
    browser: { ...browser, gpu: "none", flags: [], version: "" },
    webgpu: { present: false, adapter: null },
    webgl2: { renderer: null },
    engine: "not-loaded",
  });
  assert.equal(
    line,
    'browser-run: browser=chrome version="" mode=headless gpu=none settings=harness flags="" webgpu=none fallback=n/a webgl2="none" engine=not-loaded engine-adapter=""',
  );
});

test("a probe whose browser cannot start throws a BrowserStartError", async () => {
  // Node stands in for Chrome: it refuses Chrome's flags and exits.
  await assert.rejects(
    probeBrowser(
      { kind: "chrome", name: "chrome", command: process.execPath },
      { env: { ALPHA2_CHROME_GPU: "swiftshader" } },
    ),
    (error) => error instanceof BrowserStartError,
  );
});
