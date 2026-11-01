import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";
import { minOf } from "../../../scripts/lib/timing.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");

// Null before `npm run build`; a skip that ALPHA2_REQUIRE=wasm turns into a failure.
async function loadBinding() {
  if (!existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"))) return null;
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}
const binding = await loadBinding();
const needsWasm =
  binding === null &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

test(
  "the wasm resampler refuses a fall its filter would not fit",
  { skip: needsWasm },
  () => {
    // 768 kHz to 1 Hz would build a 94 MiB table.
    assert.throws(
      () => new binding.AudioResampler(768_000, 1),
      /8192 times the target rate/,
    );
    assert.throws(() => new binding.AudioResampler(768_000, 93));
    new binding.AudioResampler(768_000, 94).free();
    new binding.AudioResampler(44_100, 48_000).free();
    // A refusal must cost a small fraction of building the largest table that fits.
    const refusal = minOf(5, () =>
      assert.throws(() => new binding.AudioResampler(768_000, 1)),
    );
    const largest = minOf(5, () =>
      new binding.AudioResampler(768_000, 94).free(),
    );
    assert.ok(
      refusal < largest / 20,
      `refused in ${refusal.toFixed(3)} ms, built the largest table in ${largest.toFixed(3)} ms`,
    );
  },
);
