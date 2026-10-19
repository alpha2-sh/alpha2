// Builds the engine, audio worklet and extension provider wasm modules into `wasm/` with the pinned
// wasm-bindgen and Binaryen, plus their crate licenses (cargo-about).

import { execFileSync } from "node:child_process";
import { mkdirSync, renameSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";

const WASM_BINDGEN_VERSION = "0.2.128";
const BINARYEN_VERSION = "132";

/**
 * The modules, in the order they are built. `artifact` is the cargo output's file name (without
 * `.wasm`). A module whose `bindgen` lists wasm-bindgen's extra flags gets glue named after `name`
 * (`<name>.js`, `<name>.d.ts`, `<name>_bg.wasm`, `<name>_bg.wasm.d.ts`, and any `snippets/`); one
 * with `bindgen: null` is a bare module written as `<name>.wasm`.
 */
const modules = [
  {
    crate: "alpha2-wasm",
    artifact: "alpha2_wasm",
    name: "alpha2_wasm",
    bindgen: [],
  },
  {
    crate: "alpha2-audio-worklet",
    artifact: "alpha2_audio_worklet",
    name: "alpha2_audio_worklet",
    bindgen: null,
  },
  // Loaded only for SVG, animated image or image sequence sources. A provider panic aborts the
  // instance; `__wbg_reset_state` lets `alpha2/extensions` replace it instead of losing every
  // source.
  {
    crate: "alpha2-extensions-web",
    artifact: "alpha2_extensions_web",
    name: "alpha2_extensions",
    bindgen: ["--experimental-reset-state-function"],
  },
];

const packageRoot = resolve(import.meta.dirname, "..");
const workspace = resolve(packageRoot, "../..");
const out = join(packageRoot, "wasm");

const run = (command, args, options = {}) =>
  execFileSync(command, args, { cwd: workspace, stdio: "inherit", ...options });
const output = (command, args) =>
  execFileSync(command, args, { cwd: workspace, encoding: "utf8" });

const bindgenVersion = output("wasm-bindgen", ["--version"]).split(/\s+/)[1];
if (bindgenVersion !== WASM_BINDGEN_VERSION) {
  console.error(
    `wasm-bindgen CLI ${WASM_BINDGEN_VERSION} is required; found ${bindgenVersion}`,
  );
  process.exit(1);
}
const wasmOpt = join(workspace, "node_modules/.bin/wasm-opt");
let binaryenVersion;
try {
  binaryenVersion = output(wasmOpt, ["--version"]).split(/\s+/)[2];
} catch {
  console.error("pinned wasm-opt is required; run npm ci");
  process.exit(1);
}
if (binaryenVersion !== BINARYEN_VERSION) {
  console.error(
    `Binaryen ${BINARYEN_VERSION} is required; found ${binaryenVersion}`,
  );
  process.exit(1);
}
// The size budgets in scripts/check-npm-package.mjs are measured on the pinned Rust release.
run(join(workspace, "scripts/lib/check-rust-toolchain.sh"), [], {
  stdio: ["ignore", "ignore", "inherit"],
});

// Where cargo writes, which CARGO_TARGET_DIR or a cargo config can move.
const cargoOut = join(
  JSON.parse(
    output("cargo", [
      "metadata",
      "--format-version",
      "1",
      "--no-deps",
      "--manifest-path",
      join(workspace, "Cargo.toml"),
    ]),
  ).target_directory,
  "wasm32-unknown-unknown/release",
);

// A failed build leaves no modules of an earlier one behind.
rmSync(out, { recursive: true, force: true });
mkdirSync(out);

// Optimize for speed (wasm-opt -O3 below; SIMD from .cargo/config.toml) for readback and audio
// throughput, including the worklet's DSP.
run("cargo", [
  "build",
  "--manifest-path",
  join(workspace, "Cargo.toml"),
  ...modules.flatMap((module) => ["-p", module.crate]),
  "--release",
  "--target",
  "wasm32-unknown-unknown",
]);

for (const module of modules) {
  const artifact = join(cargoOut, `${module.artifact}.wasm`);
  const { name, bindgen } = module;
  if (bindgen === null) {
    run(wasmOpt, ["-O3", artifact, "-o", join(out, `${name}.wasm`)]);
    continue;
  }
  run("wasm-bindgen", [
    "--target",
    "web",
    "--out-dir",
    out,
    "--out-name",
    name,
    ...bindgen,
    artifact,
  ]);
  const wasm = join(out, `${name}_bg.wasm`);
  const optimized = join(out, `${name}_bg.opt.wasm`);
  run(wasmOpt, ["-O3", wasm, "-o", optimized]);
  renameSync(optimized, wasm);
}

run(process.execPath, [
  join(workspace, "scripts/crate-licenses.mjs"),
  "--target",
  "wasm32-unknown-unknown",
  "--out",
  join(out, "rust-crate-licenses.md"),
  ...modules.map((module) => module.crate),
]);
