#!/bin/sh
# Fetches the pinned `ffmpeg` and `ffprobe` executables for a target (default: this machine) into
# a shared cache, verifies them against release/ffmpeg-tools/manifest.toml and prints the
# directory that holds both. Tests read them through ALPHA2_FFPROBE and ALPHA2_FFMPEG.
#   fetch-ffmpeg-tools.sh [linux-x64|linux-arm64|macos-arm64|win-x64]
# ALPHA2_FFMPEG_CACHE overrides the cache directory.
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$workspace/release/ffmpeg-tools/manifest.toml"

. "$workspace/scripts/lib/windows-tools.sh"
. "$workspace/scripts/lib/pinned-download.sh"

scalar() {
  awk -F ' *= *' -v key="$1" '$1 == key { gsub(/"/, "", $2); print $2; exit }' "$manifest"
}

asset_value() {
  awk -F ' *= *' -v section="[assets.$1]" -v key="$2" '
    $0 == section { active = 1; next }
    active && /^\[/ { exit }
    active && $1 == key { gsub(/"/, "", $2); print $2; exit }
  ' "$manifest"
}

target=${1:-$(detect_target)}
repository=$(scalar repository)
release_tag=$(scalar release_tag)
archive=$(asset_value "$target" archive)
archive_sha=$(asset_value "$target" sha256)
if [ -z "$archive" ] || [ -z "$archive_sha" ]; then
  echo "no pinned FFmpeg tools for $target" >&2
  exit 1
fi

cache_root=${ALPHA2_FFMPEG_CACHE:-$workspace/target/ffmpeg-sdk-cache}
if command -v cygpath >/dev/null 2>&1; then
  cache_root=$(cygpath -u "$cache_root")
fi
release_root="$cache_root/$release_tag"
downloads="$release_root/downloads"
tools="$release_root/tools/$target"
mkdir -p "$downloads" "$release_root/tools"
download "https://github.com/$repository/releases/download/$release_tag/$archive" \
  "$downloads/$archive" "$archive_sha"

# The ready marker holds the archive's sha256, so a re-pinned archive is extracted again. The
# archive holds the two executables at its root.
ready="$tools/.alpha2-tools-ready"
if [ "$(cat "$ready" 2>/dev/null || true)" != "$archive_sha" ]; then
  lock="$release_root/tools/.lock-$target"
  stage="$release_root/tools/.extract-$target-$$"
  trap 'rm -rf "$stage"; [ "$(cat "$lock/pid" 2>/dev/null || true)" != "$$" ] || rm -rf "$lock"' EXIT
  trap 'exit 1' HUP INT TERM
  acquire_lock "$lock"
  if [ "$(cat "$ready" 2>/dev/null || true)" != "$archive_sha" ]; then
    mkdir -p "$stage"
    extract_archive "$downloads/$archive" "$stage"
    for tool in ffmpeg ffprobe; do
      if [ ! -f "$stage/$tool" ] && [ ! -f "$stage/$tool.exe" ]; then
        echo "$archive does not hold $tool at its root" >&2
        exit 1
      fi
      chmod +x "$stage/$tool"* 2>/dev/null || true
    done
    printf '%s\n' "$archive_sha" > "$stage/.alpha2-tools-ready"
    if [ -e "$tools" ]; then
      mv "$tools" "$stage.replaced"
      rm -rf "$stage.replaced"
    fi
    mv "$stage" "$tools"
  fi
  rm -rf "$lock"
  trap - EXIT HUP INT TERM
fi
printf '%s\n' "$tools"
