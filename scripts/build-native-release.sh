#!/bin/sh
# Builds the alpha2 and alpha2-host release executables against the pinned FFmpeg SDK and packs
# them, with compliance files and licenses, into thin and/or bundled archives under
# target/alpha2-native-release/archives. Must run on the target's own platform.
#   build-native-release.sh [thin|bundled|all] [target]
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$workspace/release/ffmpeg-lgpl/manifest.toml"
mode=${1:-all}

detect_target() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) echo linux-x64 ;;
    Linux-aarch64|Linux-arm64) echo linux-arm64 ;;
    Darwin-arm64) echo macos-arm64 ;;
    Darwin-x86_64) echo macos-x64 ;;
    MINGW*-x86_64|MSYS*-x86_64|CYGWIN*-x86_64) echo win-x64 ;;
    *) echo "unsupported native release host: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
}

target=${2:-$(detect_target)}
case "$mode" in thin|bundled|all) ;; *) echo "usage: build-native-release.sh [thin|bundled|all] [target]" >&2; exit 1 ;; esac

case "$target" in
  linux-*) platform=linux; executable=alpha2; host_executable=alpha2-host; archive_extension=tar.gz ;;
  macos-*) platform=macos; executable=alpha2; host_executable=alpha2-host; archive_extension=tar.gz ;;
  win-*) platform=windows; executable=alpha2.exe; host_executable=alpha2-host.exe; archive_extension=zip ;;
  *) echo "unsupported native release target: $target" >&2; exit 1 ;;
esac

# Windows ships the bundled archive only: a thin one would load FFmpeg DLLs through the default
# search order, where a planted avutil-60.dll in the current folder comes before PATH.
case "$mode-$platform" in
  thin-windows)
    echo "win-x64 has no thin archive: Windows would load FFmpeg from the current folder before PATH" >&2
    exit 1
    ;;
  all-windows) modes=bundled ;;
  all-*) modes="thin bundled" ;;
  *) modes=$mode ;;
esac
version=$(awk -F ' *= *' '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' \
  "$workspace/Cargo.toml")

# ALPHA2_RELEASE_PLAN=1 prints the archives this would build, and builds nothing.
if [ "${ALPHA2_RELEASE_PLAN:-}" = 1 ]; then
  for package_mode in $modes; do
    echo "alpha2-$version-$target-$package_mode.$archive_extension"
  done
  exit 0
fi

host_target=$(detect_target)
if [ "$host_target" != "$target" ]; then
  echo "native release target $target must be built on matching host $host_target" >&2
  exit 1
fi

# Every packaging tool is found before the long build, so a machine without one fails fast. The
# LLVM and 7-Zip installers' default folders count: neither adds itself to PATH.
. "$workspace/scripts/lib/windows-tools.sh"
missing=
need() {
  command -v "$1" >/dev/null 2>&1 || missing="$missing $1"
}
need node
cargo about --version >/dev/null 2>&1 || missing="$missing cargo-about"
case "$platform" in
  linux) need readelf; need tar ;;
  macos) need otool; need codesign; need install_name_tool; need tar ;;
  windows)
    need cygpath
    llvm_readobj=$(windows_tool llvm-readobj LLVM/bin) || missing="$missing llvm-readobj(LLVM)"
    # Git Bash has neither zip nor unzip; 7-Zip does both.
    seven_zip=
    if ! command -v zip >/dev/null 2>&1 || ! command -v unzip >/dev/null 2>&1; then
      seven_zip=$(windows_tool 7z 7-Zip) || missing="$missing 7z(7-Zip)"
    fi
    ;;
esac
if [ -n "$missing" ]; then
  echo "packaging $target needs these tools on PATH (on Windows, or in the Program Files folder their installer uses):$missing" >&2
  exit 1
fi
# ALPHA2_RELEASE_CHECK_TOOLS=1 stops here, after printing the Windows tools found.
if [ "${ALPHA2_RELEASE_CHECK_TOOLS:-}" = 1 ]; then
  if [ "$platform" = windows ]; then
    echo "llvm-readobj: $llvm_readobj"
    echo "7z: ${seven_zip:-not needed}"
  fi
  echo "packaging tools for $target found"
  exit 0
fi

# Built on the pinned Rust release, which each archive records in compliance/TOOLCHAIN.txt.
rust_toolchain=$("$workspace/scripts/lib/check-rust-toolchain.sh")

sdk_root=$("$workspace/scripts/fetch-ffmpeg-sdk.sh" "$target")
cache_root=${ALPHA2_FFMPEG_CACHE:-$workspace/target/ffmpeg-sdk-cache}
manifest_scalar() {
  awk -F ' *= *' -v key="$1" '$1 == key { gsub(/"/, "", $2); print $2; exit }' "$manifest"
}
release_tag=$(manifest_scalar release_tag)
sdk_release_url="https://github.com/$(manifest_scalar repository)/releases"
ffmpeg_version=$(manifest_scalar ffmpeg_version)
report_name=$(awk -F ' *= *' -v section="[assets.$target]" '
  $0 == section { active = 1; next }
  active && /^\[/ { exit }
  active && $1 == "report" { gsub(/"/, "", $2); print $2; exit }
' "$manifest")
report="$cache_root/$release_tag/downloads/$report_name"

export FFMPEG_LINK_MODE=dynamic
case "$platform" in
  windows)
    # x86_64-pc-windows-msvc from Git Bash: no pkg-config, so the SDK's directories are passed directly,
    # in Windows form. Line tables go into the shipped PDBs so crash reports resolve to source lines.
    windows_sdk_root=$(cygpath -m "$sdk_root")
    unset FFMPEG_PKG_CONFIG_PATH
    export FFMPEG_INCLUDE_DIR="$windows_sdk_root/include"
    export FFMPEG_LIBS_DIR="$windows_sdk_root/lib"
    export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
    ;;
  *)
    pkg_config_path="$sdk_root/lib/pkgconfig"
    export FFMPEG_PKG_CONFIG_PATH="$pkg_config_path"
    export PKG_CONFIG_PATH="$pkg_config_path${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    ;;
esac

case "$platform" in
  linux) release_rpath='-C link-arg=-Wl,-rpath,$ORIGIN/../lib' ;;
  # Room in the header for the thin archive's system rpaths added by install_name_tool below.
  macos) release_rpath='-C link-arg=-Wl,-rpath,@executable_path/../lib -C link-arg=-Wl,-headerpad_max_install_names' ;;
  # Static C runtime (the C shims are built /MT too): VCRUNTIME140.dll is not part of Windows.
  windows) release_rpath='-C target-feature=+crt-static' ;;
esac
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$release_rpath"

if [ -n "${CARGO_BUILD_TARGET:-}" ]; then
  (cd "$workspace" && cargo build --manifest-path "$workspace/Cargo.toml" -p alpha2-native -p alpha2-host \
    --release --target "$CARGO_BUILD_TARGET")
  built_binary="$workspace/target/$CARGO_BUILD_TARGET/release/$executable"
  built_host_binary="$workspace/target/$CARGO_BUILD_TARGET/release/$host_executable"
else
  (cd "$workspace" && cargo build --manifest-path "$workspace/Cargo.toml" -p alpha2-native -p alpha2-host --release)
  built_binary="$workspace/target/release/$executable"
  built_host_binary="$workspace/target/release/$host_executable"
fi
test -f "$built_binary"
test -f "$built_host_binary"

# Where a thin macOS archive finds the system FFmpeg 8, after a lib/ beside bin/: Homebrew on Apple
# silicon, /usr/local, then MacPorts. dyld has no fallback path for current-SDK binaries, and SIP
# strips DYLD_* from anything started through /bin/sh or npm.
macos_system_ffmpeg='/opt/homebrew/lib /usr/local/lib /opt/local/lib'

release_root="$workspace/target/alpha2-native-release"
archives="$release_root/archives"
mkdir -p "$archives"

# Licenses of the third-party crates compiled into the two programs for this triple.
rust_target=${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
crate_licenses="$release_root/rust-crate-licenses.md"
node "$workspace/scripts/crate-licenses.mjs" --target "$rust_target" --out "$crate_licenses" \
  alpha2-native alpha2-host

package_mode() {
  package_mode=$1
  asset_base="alpha2-$version-$target-$package_mode"
  package="$release_root/$asset_base"
  release_executables="$executable $host_executable"
  rm -rf "$package"
  mkdir -p "$package/bin" "$package/compliance" "$package/licenses"
  cp "$built_binary" "$package/bin/$executable"
  cp "$built_host_binary" "$package/bin/$host_executable"
  if [ "$platform" = windows ]; then
    for pdb in alpha2.pdb alpha2_host.pdb; do
      cp "$(dirname "$built_binary")/$pdb" "$package/bin/$pdb"
    done
  fi
  for release_executable in $release_executables; do
    chmod 0755 "$package/bin/$release_executable"
  done
  # Only thin copies search the system: a bundled archive loads its own FFmpeg or none.
  if [ "$package_mode-$platform" = thin-macos ]; then
    for release_executable in $release_executables; do
      for system_dir in $macos_system_ffmpeg; do
        install_name_tool -add_rpath "$system_dir" "$package/bin/$release_executable"
      done
      # The edit invalidates the linker's signature, and Apple silicon runs no unsigned code.
      codesign --force --sign "${MACOS_CODESIGN_IDENTITY:--}" "$package/bin/$release_executable"
    done
  fi
  cp "$manifest" "$package/compliance/FFMPEG-RUNTIME.toml"
  cp "$report" "$package/compliance/$report_name"
  cp "$workspace/LICENSE.md" "$package/LICENSE.md"
  cp "$workspace/THIRD_PARTY_NOTICES.md" "$package/compliance/THIRD_PARTY_NOTICES.md"
  printf '%s\n' "$rust_toolchain" > "$package/compliance/TOOLCHAIN.txt"
  cp -R "$sdk_root/licenses/FFmpeg" "$package/licenses/FFmpeg"
  # Only the Linux SDK builds against the Vulkan headers, so only it ships that notice.
  if [ -d "$sdk_root/licenses/Vulkan-Headers" ]; then
    cp -R "$sdk_root/licenses/Vulkan-Headers" "$package/licenses/Vulkan-Headers"
  fi
  # Only the Linux and Windows SDKs build against nv-codec-headers (NVENC).
  if [ -d "$sdk_root/licenses/nv-codec-headers" ]; then
    cp -R "$sdk_root/licenses/nv-codec-headers" "$package/licenses/nv-codec-headers"
  fi
  cp -R "$sdk_root/licenses/zlib" "$package/licenses/zlib"
  for library in dav1d opus libvpx SVT-AV1; do
    cp -R "$sdk_root/licenses/$library" "$package/licenses/$library"
  done
  cp "$crate_licenses" "$package/licenses/rust-crate-licenses.md"

  patch_list=$(sed -n 's/.*file = "\([^"]*\)", upstream = "\([0-9a-f]*\)".*/  - \1 (FFmpeg \2)/p' "$manifest" |
    sed "s|^  - |  - $sdk_release_url/download/$release_tag/|")
  test -n "$patch_list" || { echo "the FFmpeg lock file lists no patches" >&2; exit 1; }
  cat > "$package/compliance/SOURCE.md" <<EOF
# FFmpeg source and provenance

This alpha2 archive was linked dynamically against FFmpeg 8.1.3 from the pinned
shared SDK release:

- SDK release: $sdk_release_url/tag/$release_tag
- Corresponding source: $sdk_release_url/download/$release_tag/ffmpeg-$ffmpeg_version.tar.xz
  with these upstream FFmpeg commits applied in order, each a release asset:
$patch_list
- Upstream source: https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz
- Source SHA-256: 7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
- License: LGPL-2.1-or-later

The complete build scripts are published in the SDK repository. The exact
configuration/runtime audit is included beside this file. The shared libraries
may be replaced by ABI-compatible FFmpeg 8 builds.
EOF

  if [ "$package_mode" = bundled ]; then
    case "$platform" in
      linux)
        mkdir -p "$package/lib"
        cp -a "$sdk_root/lib"/libav*.so* "$sdk_root/lib"/libsw*.so* "$package/lib/"
        ;;
      macos)
        mkdir -p "$package/lib"
        cp -a "$sdk_root/lib"/libav*.dylib "$sdk_root/lib"/libsw*.dylib "$package/lib/"
        identity=${MACOS_CODESIGN_IDENTITY:--}
        find "$package/lib" -type f -name '*.dylib' -exec codesign --force --sign "$identity" {} \;
        for release_executable in $release_executables; do
          codesign --force --sign "$identity" "$package/bin/$release_executable"
        done
        ;;
      windows)
        cp "$sdk_root/bin"/*.dll "$package/bin/"
        # The host compiles its DX12 pipelines with the DXC beside it and refuses to start
        # without it, so the archive carries the pinned release's DLL and licences.
        dxc=$("$workspace/scripts/fetch-dxc.sh")
        dxc_dir=$(dirname "$(cygpath -u "$dxc")")
        cp "$dxc_dir/dxcompiler.dll" "$package/bin/"
        mkdir -p "$package/licenses/DirectXShaderCompiler"
        cp "$dxc_dir"/LICEN*.txt "$package/licenses/DirectXShaderCompiler/"
        cp "$workspace/release/dxc/manifest.toml" "$package/compliance/DXC.toml"
        ;;
    esac
    "$workspace/release/ffmpeg-lgpl/verify-runtime.sh" "$package" "$target" "$report" executables
  elif find "$package" -type f \( -name 'libav*.so*' -o -name 'libsw*.so*' \
    -o -name 'libav*.dylib' -o -name 'libsw*.dylib' -o -name 'av*.dll' \
    -o -name 'sw*.dll' \) -print | grep -q .; then
    echo "thin native release unexpectedly embeds FFmpeg" >&2
    exit 1
  fi

  case "$platform" in
    linux)
      : > "$package/compliance/DYNAMIC-LIBRARIES.txt"
      for release_executable in $release_executables; do
        readelf -d "$package/bin/$release_executable" >> "$package/compliance/DYNAMIC-LIBRARIES.txt"
        readelf -d "$package/bin/$release_executable" | grep -qF '$ORIGIN/../lib'
        # The Vulkan loader is opened at run time so a machine without one can still start the host.
        if readelf -d "$package/bin/$release_executable" | grep -q 'NEEDED.*libvulkan'; then
          echo "$release_executable links the Vulkan loader" >&2
          exit 1
        fi
      done
      ;;
    macos)
      : > "$package/compliance/DYNAMIC-LIBRARIES.txt"
      for release_executable in $release_executables; do
        otool -L "$package/bin/$release_executable" >> "$package/compliance/DYNAMIC-LIBRARIES.txt"
        otool -l "$package/bin/$release_executable" >> "$package/compliance/DYNAMIC-LIBRARIES.txt"
        rpaths=$(otool -l "$package/bin/$release_executable" | awk '$1 == "path" { print $2 }')
        printf '%s\n' "$rpaths" | grep -qxF '@executable_path/../lib'
        for system_dir in $macos_system_ffmpeg; do
          if printf '%s\n' "$rpaths" | grep -qxF "$system_dir"; then found=yes; else found=no; fi
          case "$package_mode-$found" in
            thin-no) echo "thin $release_executable has no rpath $system_dir for a system FFmpeg" >&2; exit 1 ;;
            bundled-yes) echo "bundled $release_executable searches $system_dir for FFmpeg" >&2; exit 1 ;;
          esac
        done
      done
      ;;
    windows)
      : > "$package/compliance/DYNAMIC-LIBRARIES.txt"
      for release_executable in $release_executables; do
        "$llvm_readobj" --coff-imports "$(cygpath -w "$package/bin/$release_executable")" \
          >> "$package/compliance/DYNAMIC-LIBRARIES.txt"
      done
      # Checks the whole import closure is in the archive or in Windows; the smoke run below starts
      # only the executables, so it cannot show that.
      "$workspace/release/ffmpeg-lgpl/check-pe-imports.sh" "$package/bin"
      ;;
  esac

  if [ "$archive_extension" = zip ]; then
    archive="$archives/$asset_base.zip"
    rm -f "$archive"
    if command -v zip >/dev/null 2>&1; then
      (cd "$release_root" && zip -9 -r "$archive" "$asset_base")
    else
      # Git Bash on a Windows runner has no zip; 7-Zip is on its image.
      (cd "$release_root" && "$seven_zip" a -tzip -mx=9 "$(cygpath -w "$archive")" "$asset_base" >/dev/null)
    fi
  else
    archive="$archives/$asset_base.tar.gz"
    rm -f "$archive"
    tar -czf "$archive" -C "$release_root" "$asset_base"
  fi
  # Runs the archive itself, unpacked elsewhere with no loader environment. A thin archive needs a
  # system FFmpeg 8 this machine is not required to have.
  if [ "$package_mode" = bundled ]; then
    if ! "$workspace/release/ffmpeg-lgpl/smoke-archive.sh" "$archive" "$target"; then
      rm -f "$archive"
      exit 1
    fi
  fi
  echo "packaged $archive"
}

for package_mode in $modes; do
  package_mode "$package_mode"
done
