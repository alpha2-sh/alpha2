#!/bin/sh
# Fetches a pinned Khronos validation layer (Linux x86_64) from the LunarG SDK and prints the
# directory to use as VK_LAYER_PATH.
#
# Layers older than 1.4 can crash FFmpeg video sessions.
set -eu

version=1.4.357.1
sha256=4b41e3b30e8aedaa5dac7c136561ab463eb316a25a54e2c6245f2c299ea1fb85
url="https://sdk.lunarg.com/sdk/download/$version/linux/vulkan-sdk.tar.xz"

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cache="${ALPHA2_VULKAN_SDK_CACHE:-$workspace/target/vulkan-sdk-cache}"
root="$cache/$version/x86_64"
layers="$root/share/vulkan/explicit_layer.d"
# Both files: a cache restored by a tool that prunes what it does not know can leave either alone.
complete() {
  [ -f "$layers/VkLayer_khronos_validation.json" ] && [ -f "$root/lib/libVkLayer_khronos_validation.so" ]
}

if ! complete; then
  # A private stage published with one rename, so concurrent fetches never touch each other's files.
  mkdir -p "$cache"
  stage="$cache/.fetch-$$"
  trap 'rm -rf "$stage"' EXIT
  trap 'exit 1' HUP INT TERM
  mkdir -p "$stage"
  archive="$stage/vulkan-sdk-$version.tar.xz"
  # A stall with the connection open counts as a timeout, which --retry takes.
  curl --fail --location --silent --show-error --retry 3 --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 --output "$archive" "$url"
  actual=$(sha256sum "$archive" | awk '{print $1}')
  if [ "$actual" != "$sha256" ]; then
    echo "Vulkan SDK $version checksum mismatch: $actual" >&2
    exit 1
  fi
  tar -xJf "$archive" -C "$stage" \
    "$version/x86_64/lib/libVkLayer_khronos_validation.so" \
    "$version/x86_64/share/vulkan/explicit_layer.d/VkLayer_khronos_validation.json"
  # A rename onto an existing directory fails: another fetch published an identical copy first, or
  # an incomplete copy is in the way, which this one replaces.
  if ! mv "$stage/$version" "$cache/" 2>/dev/null && ! complete; then
    rm -rf "$cache/$version"
    if ! mv "$stage/$version" "$cache/" 2>/dev/null && ! complete; then
      echo "$cache/$version could not be replaced with the validation layer" >&2
      exit 1
    fi
  fi
fi

echo "$layers"
