#!/usr/bin/env bash
# Builds the alpha2 Swift package on a Mac with Xcode: the Rust library as static libraries for
# iOS devices and the simulator, its uniffi Swift bindings, and the XCFramework the package in
# packages/alpha2-swift links, with the privacy manifest. Refuses a library that calls a
# required-reason API the manifest does not declare. PROFILE=debug builds unoptimized libraries.
# ALPHA2_NOTICES names the Rust crate license notices (scripts/crate-licenses.mjs) each framework
# ships beside its privacy manifest.
#
#   scripts/build-ios.sh
set -euo pipefail
cd "$(dirname "$0")/.."

profile=${PROFILE:-release}
package=packages/alpha2-swift
build=$package/.build-native
module=Alpha2FFI
profile_flag=()
[ "$profile" = release ] && profile_flag=(--release)
slices=(aarch64-apple-ios aarch64-apple-ios-sim)
for target in "${slices[@]}"; do
  cargo rustc --locked "${profile_flag[@]}" -p alpha2-mobile --lib --crate-type staticlib --target "$target"
done

rm -rf "$build" "$package/$module.xcframework" "$package/Sources/Alpha2"
mkdir -p "$build" "$package/Sources/Alpha2"
cargo run --locked -q -p alpha2-mobile --features bindgen --bin uniffi-bindgen -- generate \
  --library "target/aarch64-apple-ios/$profile/libalpha2_mobile.a" --language swift --no-format \
  --out-dir "$build/swift"
cp "$build/swift/Alpha2.swift" "$package/Sources/Alpha2/"

# Every required-reason API the library imports must have a reason in the manifest.
declared=$(sed -n 's/.*<string>\(NSPrivacyAccessedAPICategory[A-Za-z]*\)<\/string>.*/\1/p' \
  "$package/PrivacyInfo.xcprivacy")
category() {
  case $1 in
    _stat | _fstat | _lstat | _fstatat | _getattrlist | _getattrlistbulk | _fgetattrlist | \
      _futimens | _utimensat | _utimes) echo NSPrivacyAccessedAPICategoryFileTimestamp ;;
    _mach_absolute_time) echo NSPrivacyAccessedAPICategorySystemBootTime ;;
    _statfs | _statvfs | _fstatfs | _fstatvfs | _getfsstat) echo NSPrivacyAccessedAPICategoryDiskSpace ;;
  esac
}
missing=0
for symbol in $(nm -u "target/aarch64-apple-ios/$profile/libalpha2_mobile.a" 2>/dev/null | sort -u); do
  needed=$(category "$symbol")
  if [ -n "$needed" ] && ! grep -qx "$needed" <<<"$declared"; then
    echo "the library calls $symbol, a $needed API the privacy manifest does not declare" >&2
    missing=1
  fi
done
[ "$missing" = 0 ]

frameworks=()
for target in "${slices[@]}"; do
  framework=$build/$target/$module.framework
  mkdir -p "$framework/Headers" "$framework/Modules"
  cp "target/$target/$profile/libalpha2_mobile.a" "$framework/$module"
  cp "$build/swift/$module.h" "$framework/Headers/"
  printf 'framework module %s {\n    umbrella header "%s.h"\n    export *\n}\n' "$module" "$module" \
    > "$framework/Modules/module.modulemap"
  cat > "$framework/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>$module</string>
<key>CFBundleIdentifier</key><string>sh.alpha2.ffi</string>
<key>CFBundleName</key><string>$module</string>
<key>CFBundlePackageType</key><string>FMWK</string>
<key>CFBundleShortVersionString</key><string>$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)</string>
<key>CFBundleVersion</key><string>1</string>
<key>MinimumOSVersion</key><string>17.0</string>
</dict></plist>
PLIST
  cp "$package/PrivacyInfo.xcprivacy" "$framework/"
  if [ -n "${ALPHA2_NOTICES:-}" ]; then
    cp "$ALPHA2_NOTICES" "$framework/rust-crate-licenses.md"
  fi
  frameworks+=(-framework "$framework")
done
xcodebuild -create-xcframework "${frameworks[@]}" -output "$package/$module.xcframework" > /dev/null

# The package tests' fixtures.
fixtures=$package/Tests/Alpha2Tests/Fixtures
mkdir -p "$fixtures"
cp tests/cases/host-parity-project.json tests/corpus/cross_topology_parity.png "$fixtures/"
echo "$package/$module.xcframework"
