#!/bin/sh
# Downloads Cisco's OpenH264 binary for a target into a directory, checks the download and the
# library against release/openh264/manifest.toml, and prints the library's path. For CI and local
# test runs on a developer's own machine: users install it with `python -m alpha2 install
# --openh264` or `npx @alpha2-sh/node install --openh264`, and nothing alpha2 publishes carries it.
#   fetch-openh264.sh <linux-x64|linux-arm64|macos-arm64|win-x64> <directory>
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$workspace/release/openh264/manifest.toml"
target=${1:?usage: fetch-openh264.sh target directory}
directory=${2:?usage: fetch-openh264.sh target directory}

scalar() {
  awk -F ' *= *' -v key="$1" '
    /^\[/ { exit }
    $1 == key { value = $2; gsub(/^"|"$/, "", value); print value; exit }
  ' "$manifest"
}

target_value() {
  awk -F ' *= *' -v section="[targets.$target]" -v key="$1" '
    $0 == section { active = 1; next }
    active && /^\[/ { exit }
    active && $1 == key { value = $2; gsub(/^"|"$/, "", value); print value; exit }
  ' "$manifest"
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

base_url=$(scalar download_url)
archive=$(target_value archive)
archive_sha=$(target_value archive_sha256)
library=$(target_value library)
library_sha=$(target_value library_sha256)
if [ -z "$archive" ] || [ -z "$archive_sha" ] || [ -z "$library" ] || [ -z "$library_sha" ]; then
  echo "no OpenH264 binary is pinned for $target" >&2
  exit 1
fi

mkdir -p "$directory"
if [ -f "$directory/$library" ] && [ "$(hash_file "$directory/$library")" = "$library_sha" ]; then
  printf '%s\n' "$directory/$library"
  exit 0
fi
if ! { [ -f "$directory/$archive" ] && [ "$(hash_file "$directory/$archive")" = "$archive_sha" ]; }; then
  partial="$directory/$archive.partial.$$"
  trap 'rm -f "$partial"' EXIT HUP INT TERM
  echo "downloading $archive" >&2
  curl --fail --location --retry 3 --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 --output "$partial" "$base_url/$archive"
  actual=$(hash_file "$partial")
  if [ "$actual" != "$archive_sha" ]; then
    echo "checksum mismatch for $archive: expected $archive_sha, got $actual" >&2
    exit 1
  fi
  mv "$partial" "$directory/$archive"
  trap - EXIT HUP INT TERM
fi
bunzip2 --keep --force --stdout "$directory/$archive" > "$directory/$library.partial.$$"
actual=$(hash_file "$directory/$library.partial.$$")
if [ "$actual" != "$library_sha" ]; then
  rm -f "$directory/$library.partial.$$"
  echo "checksum mismatch for $library: expected $library_sha, got $actual" >&2
  exit 1
fi
mv "$directory/$library.partial.$$" "$directory/$library"
printf '%s\n' "$directory/$library"
