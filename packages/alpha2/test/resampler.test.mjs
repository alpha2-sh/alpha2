import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");

// The packaged binding, or null before `npm run build` made it. A missing package is a
// recorded skip, which ALPHA2_REQUIRE=wasm turns into a failure.
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
    // 768 kHz to 1 Hz would build a 94 MiB table in linear memory, and take seconds doing it.
    const started = performance.now();
    assert.throws(
      () => new binding.AudioResampler(768_000, 1),
      /8192 times the target rate/,
    );
    assert.throws(() => new binding.AudioResampler(768_000, 93));
    assert.ok(performance.now() - started < 1_000);
    // A fall of at most 8192 converts.
    new binding.AudioResampler(768_000, 94).free();
    new binding.AudioResampler(44_100, 48_000).free();
  },
);
