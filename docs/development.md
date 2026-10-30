# Development

How to build alpha2 from a checkout and run its checks on one machine.

## Prerequisites

| | Linux (Ubuntu) | macOS (Apple silicon) | Windows (x64) |
| --- | --- | --- | --- |
| Rust | rustup | rustup | rustup |
| C toolchain | `clang` | Xcode Command Line Tools | Visual Studio Build Tools (MSVC) and LLVM |
| Other packages | `pkg-config libasound2-dev` | `brew install pkg-config` | Git for Windows (the commands below run in Git Bash) |
| GPU without a GPU | `mesa-vulkan-drivers libegl1 libegl-mesa0` | not needed | not needed |
| JavaScript | Node.js 22 | Node.js 22 | Node.js 22 |

- rustup installs the pinned toolchain (`rust-toolchain.toml`: Rust 1.99.0 with clippy, rustfmt and the `wasm32-unknown-unknown` target) on the first `cargo` command.
- bindgen needs clang: on Windows, LLVM's `bin/` must be on `PATH` or `LIBCLANG_PATH` must name it.
- The npm build also needs two cargo tools at these exact versions:

  ```sh
  cargo install wasm-bindgen-cli --version 0.2.128 --locked
  cargo install cargo-about --version 0.9.2 --locked --features cli
  ```

- The browser checks need Chrome or Chromium. They look for the usual install locations; `ALPHA2_PARITY_CHROME` names another executable.

## FFmpeg

The native crates (`alpha2-ffmpeg-bridge`, `alpha2-native`, `alpha2-native-gpu`, `alpha2-host`) link FFmpeg 8. Development and CI build against the same LGPL FFmpeg SDK the releases ship, pinned in `release/ffmpeg-lgpl/manifest.toml`. `scripts/fetch-ffmpeg-sdk.sh` downloads it once into `target/ffmpeg-sdk-cache/`, checks it against the manifest and prints its directory.

Set the environment in each shell that builds:

Linux:

```sh
sdk=$(scripts/fetch-ffmpeg-sdk.sh)
export FFMPEG_PKG_CONFIG_PATH=$sdk/lib/pkgconfig LD_LIBRARY_PATH=$sdk/lib
```

macOS:

```sh
export FFMPEG_PKG_CONFIG_PATH=$(scripts/fetch-ffmpeg-sdk.sh)/lib/pkgconfig
```

Windows (Git Bash):

```sh
sdk=$(cygpath -m "$(scripts/fetch-ffmpeg-sdk.sh)")
export FFMPEG_INCLUDE_DIR=$sdk/include FFMPEG_LIBS_DIR=$sdk/lib FFMPEG_LINK_MODE=dynamic
export PATH="$sdk/bin:$PATH"
```

- `FFMPEG_PKG_CONFIG_PATH` matters most on macOS: without it the build finds a Homebrew FFmpeg and links that one without saying so. `otool -L target/debug/alpha2-host` shows which libraries a build loads.
- Linux and macOS link FFmpeg dynamically through pkg-config by default. On Windows the build takes the libraries from `FFMPEG_LIBS_DIR` and links them statically unless `FFMPEG_LINK_MODE=dynamic` says otherwise, which the SDK's import libraries need.
- To run, the test binaries and programs must find the SDK's shared libraries: on Linux through `LD_LIBRARY_PATH`, on Windows through `PATH`. On macOS the build writes the SDK's directory into each binary as an rpath, so nothing is needed.
- The crates that do not link FFmpeg (the engine, the renderer, the wasm bindings) build without any of this.

## Building

```sh
cargo build --workspace                   # every crate, debug
cargo build --release -p alpha2-host      # the project renderer
npm ci
npm run build                             # the wasm engine and the four npm packages
```

`npm run build` compiles the wasm with `wasm-bindgen`, writes the crate license list with `cargo-about`, and builds each package in `packages/`.

## Tests

### Rust

```sh
cargo test --workspace --all-features --no-fail-fast
cargo test --workspace --no-default-features --no-fail-fast
```

- `--no-fail-fast` keeps going after a failing test binary, so every crate reports.
- The tests that time work are meant for release builds: run `cargo test --release --workspace --all-features` as well before trusting a timing failure or a pass.
- `scripts/check-extension-features.sh` and `scripts/check-native-features.sh` build each extension format and each native feature alone, as a consumer that enables only one of them gets it.

### GPU

The renderer and engine tests need a GPU adapter. They run on the device the host renders on: DX12 on Windows, Metal on macOS, Vulkan (then GL) on Linux, the adapter `ALPHA2_GPU_ADAPTER` names (an enumeration index or part of the name) or else a discrete GPU before an integrated one, and lavapipe (software Vulkan) on Linux without a GPU. `WGPU_BACKEND` picks another backend for a second run:

```sh
WGPU_BACKEND=vulkan cargo test -p alpha2-render -p alpha2-engine -- --test-threads=1   # or metal, dx12, gl
```

- `alpha2-host` renders on the CPU when the machine has no hardware GPU (`ALPHA2_RENDERER=auto`). To run the host tests on lavapipe, set `ALPHA2_RENDERER=gpu`, as CI does; `ALPHA2_RENDERER=cpu` runs them on the CPU renderer. The CPU renderer's parity tests (`cargo test -p alpha2-render --lib cpu_`) compare every CPU pass with the GPU's; a failure writes both pictures to `target/cpu-parity/`.
- On Windows the host compiles its DX12 shaders with DXC and refuses to start without it: set `ALPHA2_DXC_PATH="$(scripts/fetch-dxc.sh)"` for the host and its tests (a release ships `dxcompiler.dll` beside `alpha2-host.exe`). Without it the library and its tests compile with FXC, and the test header line says which one ran.
- `cargo test` builds `alpha2-host` with its test features and copies it to `target/<profile>/alpha2-host`: scripts that run that path build it with `cargo build -p alpha2-host` first.
- Render goldens live in `tests/golden/`. `ALPHA2_UPDATE_GOLDENS=1` rewrites them instead of comparing, and `ALPHA2_GOLDEN_ACTUAL_DIR=<dir>` keeps a mismatching render for inspection.
- `ALPHA2_TEST_INTERMEDIATE=rgba8unorm` runs the renderer on its 8-bit intermediate.
- A test that cannot run on this machine skips and names the capability it lacks (the list, with what each probe checks, is `tests/capabilities.json`). `ALPHA2_REQUIRE=<comma list>` turns the skips of the capabilities it names into failures, for example `ALPHA2_REQUIRE=videotoolbox,gpu-resident` on a Mac or `ALPHA2_REQUIRE=d3d12-video,d3d12-encode,gpu-resident` on Windows; an unknown name fails at once. `ALPHA2_SKIP_LOG=<absolute path>` appends one JSON line per skip from Rust, JavaScript and Python alike, and `node scripts/skip-log-summary.mjs <path>` prints it per capability and suite.

### JavaScript and the browser

After `npm run build`:

```sh
npm run test:js            # the packages' unit tests, the scripts' tests and the editor's
npm run test:wasm-parity   # the packaged wasm against what native code pins
npm run test:host-parity   # alpha2-host against the browser engine in Chrome
npm run test:wasm-browser  # the browser cases in Chrome
npm run test:node-e2e      # @alpha2-sh/node driving a debug alpha2-host
npm run test:package       # the npm tarballs' contents and exports
```

- `ALPHA2_CHROME_GPU` picks Chrome's GPU stack: `machine` (the machine's GPU; the default on macOS and Windows), `swiftshader` (the default on Linux) or `none` (no GPU process: `auto` renders on the CPU). On Linux, `machine` needs a headed Chrome on a DRI3 display. `ALPHA2_CHROME_GPU_FLAGS` replaces the GPU flags altogether (for example `"--use-angle=vulkan --enable-features=Vulkan"`), and `ALPHA2_CHROME_HEADED=1` runs a window.
- Every browser run starts with one probe page and prints a `browser-run:` line naming the browser, its version, the GPU mode, the flags and the WebGPU adapter; the full report goes to `target/browser-reports/`. A browser that cannot start ends the run before its first case.
- `ALPHA2_REQUIRE=wasm,browser` turns a skip for a missing wasm build or browser into a failure.

## Lint and format

```sh
cargo fmt --check
npm run lint:rust          # clippy on every crate and target, warnings as errors
npm run lint
npm run format:check
npm run typecheck
```

`npm run format` and `npm run lint:fix` apply the fixes. Prettier covers the JavaScript and TypeScript files only.

## Release archives

```sh
scripts/build-native-release.sh all linux-x64     # or linux-arm64, macos-arm64, win-x64
```

It fetches the SDK and sets the FFmpeg environment itself, builds `alpha2` and `alpha2-host` in release, and writes the `thin` and `bundled` archives (`bundled` only on Windows) to `target/alpha2-native-release/archives/`. [`release/ffmpeg-lgpl/README.md`](../release/ffmpeg-lgpl/README.md) describes what each archive holds.

## Versions

`npm run version:set -- <x.y.z>` sets one version everywhere: the Cargo workspace, each crate's dependency on its siblings, and the npm packages and their pins on each other. The release workflow refuses a tag that does not match it.
