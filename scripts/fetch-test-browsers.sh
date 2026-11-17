#!/bin/sh
# usage: fetch-test-browsers.sh [<directory>]
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$workspace/release/test-browsers/manifest.toml"
. "$workspace/scripts/lib/pinned-download.sh"

value() {
  awk -F ' *= *' -v section="[$1]" -v key="$2" '
    $0 == section { active = 1; next }
    active && /^\[/ { exit }
    active && $1 == key { value = $2; gsub(/^"|"$/, "", value); print value; exit }
  ' "$manifest"
}

if [ "$(detect_target)" != linux-x64 ]; then
  echo "the pinned test browsers are for linux-x64" >&2
  exit 1
fi
cache=${1:-${ALPHA2_TEST_BROWSERS_CACHE:-$workspace/target/test-browsers-cache}}
mkdir -p "$cache"
cache=$(CDPATH= cd -- "$cache" && pwd)
for browser in chrome firefox geckodriver; do
  version=$(value "$browser" version)
  url=$(value "$browser" url)
  directory="$cache/$browser-$version"
  executable="$directory/$(value "$browser" executable)"
  if [ ! -x "$executable" ]; then
    archive="$cache/$(basename "$url")"
    download "$url" "$archive" "$(value "$browser" sha256)"
    rm -rf "$directory.partial"
    mkdir -p "$directory.partial"
    extract_archive "$archive" "$directory.partial"
    rm -rf "$directory"
    mv "$directory.partial" "$directory"
    rm -f "$archive"
    test -x "$executable"
  fi
  echo "$(value "$browser" variable)=$executable"
done
