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

- The browser checks need Chrome or Chromium. They look for the usual install locations; `ALPHA2_PARITY_CHROME` names another executable. `--browser edge|firefox|safari` (or `ALPHA2_BROWSER`) runs them in another browser: Edge (`ALPHA2_EDGE`) through the same DevTools runner, Firefox through geckodriver (`ALPHA2_GECKODRIVER`, `ALPHA2_FIREFOX`) and Safari through safaridriver (`ALPHA2_SAFARIDRIVER`).

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

- `alpha2-host` renders on the CPU when the machine has no hardware GPU (`ALPHA2_RENDERER=auto`). To run the host tests on lavapipe, set `ALPHA2_RENDERER=gpu`, as CI does; `ALPHA2_RENDERER=cpu` runs them on the CPU renderer. The CPU renderer's parity tests (`cargo test -p alpha2-render --lib cpu_`) compare every CPU pass with the GPU's; a failure writes both pictures to `target/cpu-parity/`. Their tolerance classes widen by the test device's measured filter and `pow` precision, which the first parity test prints; `ALPHA2_PARITY_REPORT=<file>` appends one JSON line per comparison (class, worst difference, histogram, device and its precision).
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

- `ALPHA2_CHROME_GPU` picks Chrome's (and Edge's) GPU stack: `machine` (the machine's GPU; the default on macOS and Windows), `swiftshader` (the default on Linux) or `none` (no GPU process: `auto` renders on the CPU). `ALPHA2_CHROME_GPU_FLAGS` replaces the GPU flags altogether (for example `"--use-angle=vulkan --enable-features=Vulkan"`).
- On Linux, `machine` needs a headed Chrome on an X display with DRI3; Xvfb has none, and Chrome's GPU process hangs there on Mesa's Vulkan drivers. `scripts/run-with-gpu-display.sh [--gpu amd|nvidia|any] <command>` starts a headless weston with Xwayland and runs the command on it in that mode (needs weston, and bwrap for `--gpu amd`).
- `ALPHA2_BROWSER_HEADED=1` runs Chrome, Edge or Firefox in a window (Safari always has one). Headless Firefox on macOS has no WebGPU adapter.
- `ALPHA2_BROWSER_SETTINGS=stock` starts the browser as a user's would: none of the harness's Chrome flags (sandbox, unsafe WebGPU, 3D API blocking, autoplay, GPU stack) and no Firefox preferences. The default is `harness`.
- Safari needs `safaridriver --enable` run once, Develop > Allow Remote Automation in Safari, and an unlocked console session; a run on a locked Mac fails at once. Without a GUI session (a CI service, some SSH sessions), `scripts/macos-gui-run.sh <log> <command>` runs the command in the logged-in user's session through a LaunchAgent, from a checkout outside `~/Documents`, `~/Desktop` and `~/Downloads`.
- Every browser run starts with one probe page and prints a `browser-run:` line naming the browser, its version, the GPU mode, the flags and the WebGPU adapter; the full report goes to `target/browser-reports/`. A browser that cannot start ends the run before its first case.
- `ALPHA2_REQUIRE=wasm,browser` turns a skip for a missing wasm build or browser into a failure.

### Mobile targets

The crates an iOS or Android app links build for `aarch64-apple-ios`, `aarch64-apple-ios-sim`, `aarch64-linux-android` and `x86_64-linux-android`. Add the targets once with `rustup target add <triple>`.

- iOS builds run on a Mac with Xcode, whose iOS SDKs the build scripts compile against.
- Android builds need the NDK (r30) and its API 29 clang for each triple, plus bindgen's sysroot:

  ```sh
  tc=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64   # darwin-x86_64 on a Mac
  export CC_aarch64_linux_android=$tc/bin/aarch64-linux-android29-clang \
    AR_aarch64_linux_android=$tc/bin/llvm-ar \
    CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$tc/bin/aarch64-linux-android29-clang \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android=--sysroot=$tc/sysroot
  cargo clippy -p alpha2-os -p alpha2-native-gpu -p alpha2-extensions -p alpha2-text \
    --all-targets --target aarch64-linux-android -- -D warnings
  ```

- Never enable `alpha2-native-gpu`'s `hwdevice` feature for a mobile target: it links FFmpeg, which mobile builds do not use.
- Test binaries run on a device or emulator with `adb push` and `adb shell`, and in the iOS simulator with `xcrun simctl spawn <device> <binary>`. The media and session suites read the repository's `tests/` from the directory `ALPHA2_TESTS_DIR` names, so push `tests/corpus`, `tests/cases` and `tests/fonts` there first:

  ```sh
  cargo test --no-run --target x86_64-linux-android -p alpha2-android-media -p alpha2-mobile --lib
  adb shell mkdir -p /data/local/tmp/alpha2-test/tests
  adb push tests/corpus tests/cases tests/fonts /data/local/tmp/alpha2-test/tests/
  adb push <test binary> /data/local/tmp/alpha2-test/
  adb shell "cd /data/local/tmp/alpha2-test && ALPHA2_TESTS_DIR=/data/local/tmp/alpha2-test/tests TMPDIR=/data/local/tmp/alpha2-test ./<test binary> --test-threads 1"
  ```

  The simulator passes environment variables prefixed with `SIMCTL_CHILD_`, so run them there with `SIMCTL_CHILD_ALPHA2_TESTS_DIR=$PWD/tests xcrun simctl spawn <device> <binary>`; macOS runs `alpha2-apple-media`'s tests natively with `cargo test`. Debug builds name GPU objects, which the Android emulator's Vulkan driver crashes on; run them there with `WGPU_DEBUG=0`. A test process has no app context, so audio output and anything else that needs one is tested through the Android library below.

#### The Android library

`scripts/build-android.sh` builds the AAR in `packages/alpha2-android/lib/build/outputs/aar/`: the Rust library for arm64-v8a and x86_64 with 16 KB pages, stripped, its Kotlin bindings generated by uniffi from that library, and the `Alpha2Android` object for what uniffi cannot express (the app context, and the native window of a `Surface`). It needs the NDK (`ANDROID_NDK_HOME`), `cargo-ndk`, Gradle 8.11 or later and the Android SDK (`ANDROID_HOME`). Gradle tasks given as arguments replace the default `:lib:assembleRelease`; the library's instrumentation tests run in an app process, where audio output and windows exist:

```sh
scripts/build-android.sh :lib:assembleDebugAndroidTest
adb install -r -t packages/alpha2-android/lib/build/outputs/apk/androidTest/debug/lib-debug-androidTest.apk
adb shell am instrument -w sh.alpha2.test/androidx.test.runner.AndroidJUnitRunner
```

An app calls `Alpha2Android.init(context)` once, before anything else.

#### The Swift package

On a Mac, `scripts/build-ios.sh` builds `packages/alpha2-swift`: the Rust library as static libraries for iOS devices and the simulator in `Alpha2FFI.xcframework`, with `PrivacyInfo.xcprivacy`, and its Swift bindings generated by uniffi. It refuses a library that imports a required-reason API the manifest does not declare. The package's tests run in the simulator on the CPU renderer (wgpu hits a Metal validation assert in the simulator):

```sh
scripts/build-ios.sh
cd packages/alpha2-swift
xcodebuild test -scheme Alpha2 -destination 'platform=iOS Simulator,id=<device>'
```

Test binaries for an iPhone link a Swift overlay through a dependency, so build them with `RUSTFLAGS="-C link-arg=-Wl,-rpath,/usr/lib/swift"`; an app gets that path from Xcode.

#### App lifecycle

- `Alpha2.pause()` holds exports and previews at their next frame, and `resume()` lets them continue. iOS runs no GPU work in the background, so an iOS app pauses when it leaves the foreground and resumes when it returns.
- `Alpha2.memoryWarning()` halves the decoded frames running work keeps. Call it from `didReceiveMemoryWarning` on iOS, and from `onTrimMemory` on Android at `TRIM_MEMORY_RUNNING_LOW` and above. Work sizes its budget from the memory free when it starts.
- An Android export that should go on in the background runs in a foreground service of type `dataSync` (API 29 and later) or `mediaProcessing` (API 35 and later). An app targeting API 31 or later cannot start one from the background, so it starts the service and the export while visible. Without the service, the app pauses as on iOS.

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
scripts/build-native-release.sh bundled linux-x64     # or linux-arm64, macos-arm64, win-x64
```

It extracts the SDK afresh from its checked download, sets the FFmpeg environment itself, builds `alpha2` and `alpha2-host` in release (`--locked`, for macOS 11.0), and writes the bundled archive to `target/alpha2-native-release/archives/`, its members recorded as root's. [`release/ffmpeg-lgpl/README.md`](../release/ffmpeg-lgpl/README.md) describes what it holds. Keep the SDK cache's `bin/` off `PATH` outside the build: on Windows it holds the FFmpeg DLLs, which every program started from such a shell would then load.

`scripts/packaging/install-journeys.sh <dir>` installs a packaging job's outputs (the linux-x64 archive, the wheels and the `npm pack` tarball of `@alpha2-sh/node`) in containers: Debian as root, through a proxy, Alpine (refused: musl), AlmaLinux 9 (refused: glibc older than the floor) and Ubuntu without a CA store. `scripts/packaging/serve-mirror.mjs` and `forward-proxy.mjs` are the mirror and proxy it uses; `unicode-cache-install.mjs` installs into caches named outside the ANSI code page, which the Windows packaging job runs.

## Versions

`npm run version:set -- <x.y.z>` sets one version everywhere: the Cargo workspace, each crate's dependency on its siblings, and the npm packages and their pins on each other. The release workflow refuses a tag that does not match it.
