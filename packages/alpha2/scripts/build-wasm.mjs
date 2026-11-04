// Builds the wasm modules into `wasm/` with pinned wasm-bindgen and Binaryen, plus crate licenses.

import { execFileSync } from "node:child_process";
import {
  appendFileSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
} from "node:fs";
import { join, resolve } from "node:path";

const WASM_BINDGEN_VERSION = "0.2.128";
const BINARYEN_VERSION = "132";

/** `bindgen: null` writes a bare `<name>.wasm`; otherwise glue is named after `name`. */
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
  // A provider panic aborts the instance; `alpha2/extensions` replaces it with
  // `__wbg_reset_state_async`.
  {
    crate: "alpha2-extensions-web",
    artifact: "alpha2_extensions_web",
    name: "alpha2_extensions",
    bindgen: ["--experimental-reset-state-function"],
    asyncReset: true,
  },
  {
    crate: "alpha2-extensions-pdf-web",
    artifact: "alpha2_extensions_pdf_web",
    name: "alpha2_extensions_pdf",
    bindgen: ["--experimental-reset-state-function"],
    asyncReset: true,
    // Its own cargo build, so feature unification does not put PDF in the main extension module.
    separate: true,
  },
];

const SYNC_INSTANCE =
  "wasmInstance = new WebAssembly.Instance(wasmModule, __wbg_get_imports());";

/**
 * Adds `__wbg_reset_state_async` to wasm-bindgen glue: its own `__wbg_reset_state` with the new
 * instance made by `WebAssembly.instantiate`, since browsers refuse to instantiate a large module
 * synchronously on the main thread. It instantiates before resetting, so state changes only once
 * the instance exists.
 */
function addAsyncReset(glue, types) {
  const text = readFileSync(glue, "utf8");
  const reset =
    /^export function __wbg_reset_state \(\) \{\n([\s\S]*?)^\}$/m.exec(text);
  if (reset === null || reset[1].split(SYNC_INSTANCE).length !== 2)
    throw new Error(
      `${glue}: __wbg_reset_state is not the shape wasm-bindgen ${WASM_BINDGEN_VERSION} writes`,
    );
  const body = reset[1].replace(SYNC_INSTANCE, "wasmInstance = instance;");
  appendFileSync(
    glue,
    "\nexport async function __wbg_reset_state_async () {\n" +
      "    const instance = await WebAssembly.instantiate(wasmModule, __wbg_get_imports());\n" +
      body +
      "}\n",
  );
  appendFileSync(
    types,
    "export function __wbg_reset_state_async(): Promise<void>;\n",
  );
}

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
// Binaryen's node script, not the `.bin` shim, which Windows cannot exec.
const wasmOpt = join(workspace, "node_modules/binaryen/bin/wasm-opt");
const runWasmOpt = (args) => run(process.execPath, [wasmOpt, ...args]);
let binaryenVersion;
try {
  binaryenVersion = output(process.execPath, [wasmOpt, "--version"]).split(
    /\s+/,
  )[2];
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
// The size budgets in scripts/check-npm-package.mjs assume the pinned Rust release.
run("sh", [join(workspace, "scripts/lib/check-rust-toolchain.sh")], {
  stdio: ["ignore", "ignore", "inherit"],
});

// CARGO_TARGET_DIR or a cargo config can move cargo's output.
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

rmSync(out, { recursive: true, force: true });
mkdirSync(out);

// Optimize for speed (readback and worklet DSP throughput); SIMD comes from .cargo/config.toml.
const cargoBuild = (crates) =>
  run("cargo", [
    "build",
    "--manifest-path",
    join(workspace, "Cargo.toml"),
    ...crates.flatMap((crate) => ["-p", crate]),
    "--release",
    "--target",
    "wasm32-unknown-unknown",
  ]);
cargoBuild(
  modules.filter((module) => !module.separate).map((module) => module.crate),
);
for (const module of modules.filter((module) => module.separate))
  cargoBuild([module.crate]);

for (const module of modules) {
  const artifact = join(cargoOut, `${module.artifact}.wasm`);
  const { name, bindgen } = module;
  if (bindgen === null) {
    runWasmOpt(["-O3", artifact, "-o", join(out, `${name}.wasm`)]);
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
  if (module.asyncReset)
    addAsyncReset(join(out, `${name}.js`), join(out, `${name}.d.ts`));
  const wasm = join(out, `${name}_bg.wasm`);
  const optimized = join(out, `${name}_bg.opt.wasm`);
  runWasmOpt(["-O3", wasm, "-o", optimized]);
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
