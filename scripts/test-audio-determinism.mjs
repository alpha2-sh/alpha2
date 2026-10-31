// The engine's mix must be bit-identical everywhere: renders the fixed mixes natively and as wasm
// under Node and compares the hashes (native ones pinned in tests/audio_determinism).

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";

import { cargoTargetDirectory } from "./lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..");
const cargo = (args) =>
  execFileSync("cargo", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  });

const [
  native,
  nativeWaveform,
  nativeLoudness,
  nativeTimeMaps,
  nativeResample,
  nativeMono,
  nativeRateAndBlocks,
  nativeStretch,
] = cargo(["run", "--quiet", "--release", "-p", "alpha2-audio-determinism"])
  .trim()
  .split("\n");
cargo([
  "build",
  "--quiet",
  "--release",
  "-p",
  "alpha2-audio-determinism",
  "--target",
  "wasm32-unknown-unknown",
]);
const bytes = await readFile(
  join(
    cargoTargetDirectory(),
    "wasm32-unknown-unknown",
    "release",
    "alpha2_audio_determinism.wasm",
  ),
);
// wgpu's wasm-bindgen imports, which the audio path never calls, are stubbed to fail loudly.
const module = await WebAssembly.compile(bytes);
const imports = {};
for (const { module: name, name: field, kind } of WebAssembly.Module.imports(
  module,
)) {
  if (kind !== "function")
    throw new Error(`unexpected ${kind} import ${name}.${field}`);
  imports[name] ??= {};
  imports[name][field] = () => {
    throw new Error(`the audio path called ${name}.${field}`);
  };
}
const instance = await WebAssembly.instantiate(module, imports);
const wasm = BigInt.asUintN(
  64,
  instance.exports.alpha2_audio_determinism_hash(),
)
  .toString(16)
  .padStart(16, "0");
assert.equal(wasm, native, `wasm ${wasm} != native ${native}`);
console.log(`verified the engine mix hashes to ${native} natively and as wasm`);
const wasmWaveform = BigInt.asUintN(64, instance.exports.alpha2_waveform_hash())
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmWaveform,
  nativeWaveform,
  `wasm waveform ${wasmWaveform} != native ${nativeWaveform}`,
);
console.log(
  `verified the waveform summary hashes to ${nativeWaveform} natively and as wasm`,
);
const wasmLoudness = BigInt.asUintN(64, instance.exports.alpha2_loudness_hash())
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmLoudness,
  nativeLoudness,
  `wasm loudness ${wasmLoudness} != native ${nativeLoudness}`,
);
console.log(
  `verified the loudness report hashes to ${nativeLoudness} natively and as wasm`,
);
const wasmTimeMaps = BigInt.asUintN(64, instance.exports.alpha2_time_map_hash())
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmTimeMaps,
  nativeTimeMaps,
  `wasm time-mapped mix ${wasmTimeMaps} != native ${nativeTimeMaps}`,
);
console.log(
  `verified the time-mapped mix hashes to ${nativeTimeMaps} natively and as wasm`,
);
const wasmResample = BigInt.asUintN(64, instance.exports.alpha2_resample_hash())
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmResample,
  nativeResample,
  `wasm resampled source ${wasmResample} != native ${nativeResample}`,
);
console.log(
  `verified the resampled source hashes to ${nativeResample} natively and as wasm`,
);
const wasmMono = BigInt.asUintN(64, instance.exports.alpha2_mono_hash())
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmMono,
  nativeMono,
  `wasm mono mix ${wasmMono} != native ${nativeMono}`,
);
console.log(
  `verified the mono mix hashes to ${nativeMono} natively and as wasm`,
);
const wasmRateAndBlocks = BigInt.asUintN(
  64,
  instance.exports.alpha2_rate_and_blocks_hash(),
)
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmRateAndBlocks,
  nativeRateAndBlocks,
  `wasm 44.1 kHz mix in uneven blocks ${wasmRateAndBlocks} != native ${nativeRateAndBlocks}`,
);
console.log(
  `verified the 44.1 kHz mix in uneven blocks hashes to ${nativeRateAndBlocks} natively and as wasm`,
);
const wasmStretch = BigInt.asUintN(64, instance.exports.alpha2_stretch_hash())
  .toString(16)
  .padStart(16, "0");
assert.equal(
  wasmStretch,
  nativeStretch,
  `wasm stretched mix ${wasmStretch} != native ${nativeStretch}`,
);
console.log(
  `verified the stretched mix hashes to ${nativeStretch} natively and as wasm`,
);
