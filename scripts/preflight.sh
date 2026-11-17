#!/usr/bin/env bash
# usage: preflight.sh quick | full | list | <check>...
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

QUICK=(
  fmt fuzz-fmt plugins-fmt metadata
  clippy clippy-native fuzz-clippy plugins-clippy plugins-clippy-wasm wasm-clippy
  clippy-windows clippy-macos clippy-android check-no-default-features
  schema extension-features native-features deny
  js-format js-lint js-typecheck npm-package python-lint
)
FULL=(
  "${QUICK[@]}"
  rust-tests-all-features rust-tests-no-default-features plugins-test determinism
  js-build js-tests pytest browser-chrome browser-firefox
)
SKIPPED=77
MOBILE_PACKAGES=(
  -p alpha2-os -p alpha2-native-gpu -p alpha2-extensions -p alpha2-text -p alpha2-host-core
  -p alpha2-container -p alpha2-backend-conformance -p alpha2-android-media -p alpha2-apple-media
  -p alpha2-mobile
)

skip() {
  echo "skip: $*"
  exit "$SKIPPED"
}

ffmpeg_env() {
  [ -n "${FFMPEG_PKG_CONFIG_PATH:-}${FFMPEG_LIBS_DIR:-}" ] && return
  local sdk
  sdk="$(scripts/fetch-ffmpeg-sdk.sh)"
  export PKG_CONFIG_PATH="$sdk/lib/pkgconfig" FFMPEG_PKG_CONFIG_PATH="$sdk/lib/pkgconfig"
  export FFMPEG_LINK_MODE=dynamic LD_LIBRARY_PATH="$sdk/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
}

gpu_env() {
  export ALPHA2_RENDERER="${ALPHA2_RENDERER:-gpu}"
  if [ -z "${VK_ICD_FILENAMES:-}" ]; then
    local icd
    for icd in /usr/share/vulkan/icd.d/lvp_icd*.json; do
      [ -f "$icd" ] && export VK_ICD_FILENAMES="$icd" && break
    done
  fi
}

plugin_fixtures() {
  scripts/pack-plugins.sh --fixtures "$root/target/preflight-plugins" "$@"
}

target_installed() {
  rustup target list --installed | grep -qx "$1" ||
    skip "rustup target add $1"
}

packages_built() {
  [ -d packages/alpha2/wasm ] && [ -d packages/alpha2/dist ] ||
    skip "needs the npm packages built: npm run build (or preflight.sh full)"
}

python_env() {
  [ -x python/alpha2/.venv/bin/pytest ] ||
    skip "needs python/alpha2/.venv: cd python/alpha2 && uv venv -p 3.11 .venv && uv pip install --group dev && uv pip install --no-deps -e ."
}

check_fmt() { cargo fmt --check; }
check_fuzz_fmt() { cargo fmt --manifest-path fuzz/Cargo.toml --check; }
check_plugins_fmt() { cargo fmt --manifest-path plugins/Cargo.toml --all --check; }
check_metadata() { cargo metadata --locked --format-version 1 > /dev/null; }
check_clippy() { ffmpeg_env && npm run lint:rust; }
check_clippy_native() { ffmpeg_env && cargo clippy -p alpha2-native --all-targets -- -D warnings; }
check_fuzz_clippy() { cargo clippy --manifest-path fuzz/Cargo.toml --locked --bins -- -D warnings; }
check_plugins_clippy() {
  cargo clippy --manifest-path plugins/Cargo.toml --locked --all-targets -- -D warnings
}
check_plugins_clippy_wasm() {
  cargo clippy --manifest-path plugins/Cargo.toml --locked --target wasm32-unknown-unknown \
    -p alpha2-plugin-sdk -p alpha2-plugin-fixture-digest -p alpha2-plugin-fixture-scripted \
    -- -D warnings
}
check_wasm_clippy() {
  cargo clippy -p alpha2-wasm -p alpha2-audio-worklet --target wasm32-unknown-unknown -- -D warnings
}
check_clippy_windows() {
  [ "$(uname -s)" = Linux ] || skip "the cross clippy runs on Linux; Windows runs platform-lint natively"
  target_installed x86_64-pc-windows-gnu
  DOCS_RS=1 CARGO_TARGET_DIR=target/cross-check cargo clippy --workspace --exclude alpha2-python \
    --all-features --all-targets --target x86_64-pc-windows-gnu -- -D warnings
}
check_clippy_macos() {
  [ "$(uname -s)" = Linux ] || skip "the cross clippy runs on Linux; a Mac runs platform-lint natively"
  target_installed aarch64-apple-darwin
  local excluded=(--exclude alpha2-python) crate
  for crate in $(cargo tree -i wasmtime --target aarch64-apple-darwin --all-features \
    -e normal,build,dev --prefix none | awk '$1 ~ /^alpha2/ { print $1 }' | sort -u); do
    excluded+=(--exclude "$crate")
  done
  echo "without the crates that build wasmtime's C runtime, which needs the macOS SDK: ${excluded[*]}"
  DOCS_RS=1 CARGO_TARGET_DIR=target/cross-check \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_darwin="-I/usr/include/x86_64-linux-gnu -D__x86_64__" \
    cargo clippy --workspace "${excluded[@]}" --all-features --all-targets \
    --target aarch64-apple-darwin -- -D warnings
}
check_clippy_android() {
  local target=aarch64-linux-android var=aarch64_linux_android ndk="${ANDROID_NDK_HOME:-}"
  [ -n "$ndk" ] && [ -d "$ndk" ] || skip "set ANDROID_NDK_HOME to an NDK"
  target_installed "$target"
  local tc
  tc="$(echo "$ndk"/toolchains/llvm/prebuilt/*)"
  export "CC_$var=$tc/bin/${target}29-clang" "CXX_$var=$tc/bin/${target}29-clang++"
  export "AR_$var=$tc/bin/llvm-ar" "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$tc/bin/${target}29-clang"
  export "BINDGEN_EXTRA_CLANG_ARGS_$var=--sysroot=$tc/sysroot"
  cargo clippy --locked "${MOBILE_PACKAGES[@]}" --all-targets --target "$target" -- -D warnings
  cargo clippy --locked -p alpha2 -p alpha2-compose -p alpha2-host-core --all-features \
    --all-targets --target "$target" -- -D warnings
}
check_check_no_default_features() {
  ffmpeg_env && cargo check --workspace --no-default-features --all-targets
}
check_schema() { scripts/check-schema-fresh.sh; }
check_extension_features() { scripts/check-extension-features.sh; }
check_native_features() { ffmpeg_env && scripts/check-native-features.sh; }
check_deny() {
  command -v cargo-deny > /dev/null || skip "cargo install cargo-deny --locked"
  cargo deny check
  cargo deny --manifest-path plugins/Cargo.toml check
}
check_js_format() { npm run format:check; }
check_js_lint() { npm run lint; }
check_js_typecheck() { packages_built && npm run typecheck; }
check_npm_package() { packages_built && npm run test:package; }
check_python_lint() {
  python_env
  cd python/alpha2
  .venv/bin/ruff check src tests ../alpha2-host
  .venv/bin/ruff format --check src tests ../alpha2-host
  .venv/bin/mypy
  .venv/bin/mypy --platform win32
  .venv/bin/mypy --platform darwin
}
check_rust_tests_all_features() {
  ffmpeg_env && gpu_env && plugin_fixtures
  cargo test --workspace --all-features --no-fail-fast
}
check_rust_tests_no_default_features() {
  ffmpeg_env && gpu_env && plugin_fixtures
  cargo test --workspace --no-default-features --no-fail-fast
}
check_plugins_test() { cargo test --manifest-path plugins/Cargo.toml --locked; }
check_determinism() {
  cargo test -p alpha2-audio -p alpha2-audio-determinism -p alpha2-eval -p alpha2-text --no-fail-fast
  cargo test -p alpha2-render --lib cpu_determinism --no-fail-fast
  cargo test -p alpha2-render-determinism -p alpha2-extension-raster-pins --no-fail-fast
}
check_js_build() { npm run build; }
check_js_tests() { packages_built && npm run test:js; }
check_pytest() {
  python_env
  ffmpeg_env
  cd python/alpha2
  .venv/bin/pytest
}
check_browser_chrome() {
  packages_built
  gpu_env && plugin_fixtures --no-native
  ALPHA2_CHROME_GPU="${ALPHA2_CHROME_GPU:-swiftshader}" node scripts/test-wasm-browser.mjs
}
check_browser_firefox() {
  packages_built
  gpu_env && plugin_fixtures --no-native
  node scripts/test-wasm-browser.mjs --browser firefox
}

usage() {
  echo "usage: preflight.sh quick | full | list | <check>..." >&2
  exit 2
}

[ $# -gt 0 ] || usage
case $1 in
  quick) checks=("${QUICK[@]}") ;;
  full) checks=("${FULL[@]}") ;;
  list)
    echo "quick: ${QUICK[*]}"
    echo "full adds: ${FULL[*]:${#QUICK[@]}}"
    exit 0
    ;;
  *) checks=("$@") ;;
esac
for check in "${checks[@]}"; do
  declare -F "check_${check//-/_}" > /dev/null || {
    echo "no check named $check (preflight.sh list)" >&2
    exit 2
  }
done

failed=()
skipped=()
report=()
for check in "${checks[@]}"; do
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::group::preflight $check"; else echo "== $check"; fi
  started=$SECONDS
  (set -e; "check_${check//-/_}")
  status=$?
  [ -z "${GITHUB_ACTIONS:-}" ] || echo "::endgroup::"
  took=$((SECONDS - started))
  case $status in
    0) report+=("ok    $check (${took}s)") ;;
    "$SKIPPED")
      if [ -n "${GITHUB_ACTIONS:-}" ]; then
        failed+=("$check") && report+=("FAIL  $check (skipped in CI)")
      else
        skipped+=("$check") && report+=("skip  $check")
      fi
      ;;
    *) failed+=("$check") && report+=("FAIL  $check (${took}s)") ;;
  esac
done
if [ ${#checks[@]} -gt 1 ]; then
  printf '%s\n' "${report[@]}"
  [ ${#skipped[@]} -eq 0 ] || echo "skipped: ${skipped[*]}"
fi
[ ${#failed[@]} -eq 0 ] || {
  echo "failed: ${failed[*]}" >&2
  exit 1
}
