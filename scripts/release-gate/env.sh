# The release gate's environment, sourced by run.sh from the repository root (bash, Git Bash on
# Windows). Detects the machine, fetches and exports the pinned FFmpeg SDK, OpenH264, the Vulkan
# validation layer (Linux) and DXC (Windows), as CI does. Node, cargo and git must be on PATH.
#
# Sets GATE_MACHINE (lin, win or mac; ALPHA2_GATE_MACHINE overrides) and GATE_PLATFORM.

gate_root=$(pwd)
case "$(uname -s)" in
  Linux) GATE_PLATFORM=linux; gate_machine=lin; gate_target=linux-x64 ;;
  Darwin) GATE_PLATFORM=macos; gate_machine=mac; gate_target=macos-arm64 ;;
  MINGW* | MSYS* | CYGWIN*) GATE_PLATFORM=windows; gate_machine=win; gate_target=win-x64 ;;
  *) echo "release gate: unsupported system $(uname -s)" >&2; return 2 ;;
esac
if [ "$GATE_PLATFORM" = linux ] && [ "$(uname -m)" = aarch64 ]; then gate_target=linux-arm64; fi
GATE_MACHINE=${ALPHA2_GATE_MACHINE:-$gate_machine}
export GATE_MACHINE GATE_PLATFORM

for tool in node cargo git; do
  command -v "$tool" > /dev/null || { echo "release gate: $tool is not on PATH" >&2; return 2; }
done

export CARGO_TERM_COLOR=never

gate_sdk=$(scripts/fetch-ffmpeg-sdk.sh "$gate_target") || { echo "release gate: FFmpeg SDK fetch failed" >&2; return 2; }
export FFMPEG_LINK_MODE=dynamic
if [ "$GATE_PLATFORM" = windows ]; then
  gate_sdk=$(cygpath -m "$gate_sdk")
  export FFMPEG_INCLUDE_DIR="$gate_sdk/include" FFMPEG_LIBS_DIR="$gate_sdk/lib"
  PATH="$(cygpath -u "$gate_sdk/bin"):$PATH"
  export PATH
else
  export FFMPEG_PKG_CONFIG_PATH="$gate_sdk/lib/pkgconfig" PKG_CONFIG_PATH="$gate_sdk/lib/pkgconfig"
fi
if [ "$GATE_PLATFORM" = linux ]; then
  LD_LIBRARY_PATH="$gate_sdk/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  if gate_layers=$(scripts/fetch-vulkan-validation-layer.sh); then
    export VK_LAYER_PATH="$gate_layers"
    LD_LIBRARY_PATH="$(cd "$gate_layers/../../../lib" && pwd):$LD_LIBRARY_PATH"
  else
    echo "release gate: the Vulkan validation layer fetch failed; validation runs will say so" >&2
  fi
  export LD_LIBRARY_PATH
fi

if gate_openh264=$(scripts/fetch-openh264.sh "$gate_target" "$gate_root/target/release-gate/cache/openh264"); then
  export ALPHA2_OPENH264_LIBRARY="$gate_openh264"
  # Cisco's archive, which the Node client's decompression test reads when OpenH264 is required.
  for gate_archive in "$gate_root"/target/release-gate/cache/openh264/*.bz2; do
    [ -f "$gate_archive" ] && export ALPHA2_TEST_OPENH264_ARCHIVE="$gate_archive"
  done
else
  echo "release gate: the OpenH264 fetch failed; its tests and perf configurations will fail" >&2
fi

if [ "$GATE_PLATFORM" = windows ]; then
  # The Windows host compiles its DX12 pipelines with the pinned DXC and refuses to start without.
  gate_dxc=$(scripts/fetch-dxc.sh) || { echo "release gate: DXC fetch failed" >&2; return 2; }
  export ALPHA2_DXC_PATH="$gate_dxc"
fi
