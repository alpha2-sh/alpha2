#!/bin/sh
# Downloads the pinned DirectX Shader Compiler release, checks the archive and every kept file
# against release/dxc/manifest.toml, and prints the absolute path of `dxcompiler.dll` (a Windows
# path where `cygpath` exists). The Windows host loads that file (ALPHA2_DXC_PATH, or beside
# alpha2-host.exe in a release); the fetch itself runs on any OS.
#   fetch-dxc.sh [<directory>]
# Default directory: ${ALPHA2_DXC_CACHE:-<workspace>/target/dxc-cache}/<version>.
# ALPHA2_DXC_DOWNLOAD_URL replaces the manifest's download URL (for the script's test).
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$workspace/release/dxc/manifest.toml"
. "$workspace/scripts/lib/windows-tools.sh"

scalar() {
  awk -F ' *= *' -v key="$1" '
    /^\[/ { exit }
    $1 == key { value = $2; gsub(/^"|"$/, "", value); print value; exit }
  ' "$manifest"
}

# Lines of `<path> <sha256>` from a table of the manifest.
table() {
  awk -F ' *= *' -v section="[$1]" '
    $0 == section { active = 1; next }
    active && /^\[/ { exit }
    active && NF == 2 && $1 !~ /^#/ { key = $1; value = $2; gsub(/"/, "", key); gsub(/"/, "", value); print key, value }
  ' "$manifest"
}

# A path a Windows program takes: Git Bash's /c/... form translated where cygpath exists.
native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

# From standard input: given a name with a backslash (a Windows path), sha256sum escapes its line
# and prefixes the hash with one.
hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum < "$1" | awk '{print $1}'
  else
    shasum -a 256 < "$1" | awk '{print $1}'
  fi
}

version=$(scalar version)
archive=$(scalar archive)
archive_sha=$(scalar archive_sha256)
base_url=${ALPHA2_DXC_DOWNLOAD_URL:-$(scalar download_url)}
directory=${1:-${ALPHA2_DXC_CACHE:-$workspace/target/dxc-cache}/$version}
mkdir -p "$directory"

# Every kept file and licence, verified in place.
verified() {
  { table files; table licenses; } | while read -r path sha; do
    file="$directory/$(basename "$path")"
    [ -f "$file" ] && [ "$(hash_file "$file")" = "$sha" ] || { echo missing; break; }
  done | grep -q missing && return 1
  return 0
}

print_path() {
  native_path "$directory/dxcompiler.dll"
}

if verified; then
  print_path
  exit 0
fi

stage="$directory/.extract-$$"
partial="$directory/$archive.partial.$$"
trap 'rm -rf "$stage" "$partial"' EXIT
trap 'exit 1' HUP INT TERM
if ! { [ -f "$directory/$archive" ] && [ "$(hash_file "$directory/$archive")" = "$archive_sha" ]; }; then
  echo "downloading $archive" >&2
  curl --fail --location --retry 3 --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 --output "$partial" "$base_url/$archive"
  actual=$(hash_file "$partial")
  if [ "$actual" != "$archive_sha" ]; then
    echo "checksum mismatch for $archive: expected $archive_sha, got $actual" >&2
    exit 1
  fi
  mv "$partial" "$directory/$archive"
fi
mkdir -p "$stage"
# Git Bash on a Windows runner has no unzip; 7-Zip is on its image. The archive's paths use
# backslashes, which unzip warns about and turns into directories.
if command -v unzip >/dev/null 2>&1; then
  unzip -q -o "$directory/$archive" -d "$stage" 2>/dev/null || [ $? -eq 1 ]
else
  seven_zip=$(windows_tool 7z 7-Zip) || {
    echo "extracting $archive needs unzip or 7z (7-Zip): on PATH or in Program Files/7-Zip" >&2
    exit 1
  }
  "$seven_zip" x -y "-o$(native_path "$stage")" "$(native_path "$directory/$archive")" >/dev/null
fi
{ table files; table licenses; } | while read -r path sha; do
  extracted="$stage/$path"
  if [ ! -f "$extracted" ]; then
    echo "$archive has no $path" >&2
    exit 1
  fi
  actual=$(hash_file "$extracted")
  if [ "$actual" != "$sha" ]; then
    echo "checksum mismatch for $path: expected $sha, got $actual" >&2
    exit 1
  fi
  cp "$extracted" "$directory/$(basename "$path")"
done
verified || exit 1
print_path
