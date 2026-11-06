#!/usr/bin/env bash
# Builds the alpha2 Android library (AAR): the Rust library for arm64-v8a and x86_64 with 16 KB
# pages, its uniffi Kotlin bindings, then the Gradle library in packages/alpha2-android. Needs the
# NDK (ANDROID_NDK_HOME), cargo-ndk, Gradle 8.11 or later on PATH, and the Android SDK
# (ANDROID_HOME). PROFILE=debug builds unoptimized libraries. ALPHA2_NOTICES names the Rust crate
# license notices (scripts/crate-licenses.mjs) the library ships in META-INF/alpha2/.
#
#   scripts/build-android.sh [gradle tasks...]   (default: :lib:assembleRelease)
set -euo pipefail
cd "$(dirname "$0")/.."

profile=${PROFILE:-release}
library=packages/alpha2-android/lib
native=$library/build/native
jni=$library/build/jniLibs
rm -rf "$native" "$jni"
mkdir -p "$native" "$jni"
profile_flag=()
[ "$profile" = release ] && profile_flag=(--release)
RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384" \
  cargo ndk -t arm64-v8a -t x86_64 -P 29 -o "$native" build --locked "${profile_flag[@]}" -p alpha2-mobile

# The bindings come from the built library's metadata, so they match it exactly.
rm -rf "$library/build/generated/uniffi"
cargo run --locked -q -p alpha2-mobile --features bindgen --bin uniffi-bindgen -- generate \
  --library "$native/arm64-v8a/libalpha2_mobile.so" --language kotlin --no-format \
  --out-dir "$library/build/generated/uniffi"

# The AAR ships the libraries without symbols; apps symbolicate crashes from the ones in
# build/native.
strip=$(ls "${ANDROID_NDK_HOME:?}"/toolchains/llvm/prebuilt/*/bin/llvm-strip | head -n 1)
for abi in arm64-v8a x86_64; do
  mkdir -p "$jni/$abi"
  if [ "$profile" = release ]; then
    "$strip" --strip-all -o "$jni/$abi/libalpha2_mobile.so" "$native/$abi/libalpha2_mobile.so"
  else
    cp "$native/$abi/libalpha2_mobile.so" "$jni/$abi/"
  fi
done

notices=$library/build/notices
rm -rf "$notices"
mkdir -p "$notices/META-INF/alpha2"
if [ -n "${ALPHA2_NOTICES:-}" ]; then
  cp "$ALPHA2_NOTICES" "$notices/META-INF/alpha2/rust-crate-licenses.md"
fi

# The instrumentation tests' media fixtures.
corpus=$library/build/test-corpus
mkdir -p "$corpus"
cp tests/corpus/cross_topology_parity.png tests/corpus/audio-only-aac-priming.m4a "$corpus/"

gradle -q -p packages/alpha2-android "${@:-:lib:assembleRelease}"
