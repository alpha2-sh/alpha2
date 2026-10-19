#!/bin/sh
# Proves a bundled native archive runs on its own: it is unpacked into a fresh directory and its
# executables start there with no loader environment, so the only FFmpeg they can load is the one
# the archive's RUNPATH, @rpath install names or application folder point at.
#   smoke-archive.sh <archive.tar.gz|archive.zip> <target>
set -eu

archive=${1:?usage: smoke-archive.sh <archive> <target>}
target=${2:?usage: smoke-archive.sh <archive> <target>}

case "$target" in
  linux-*) platform=linux; executables="alpha2 alpha2-host" ;;
  macos-*) platform=macos; executables="alpha2 alpha2-host" ;;
  win-*) platform=windows; executables="alpha2.exe alpha2-host.exe" ;;
  *) echo "unsupported native release target: $target" >&2; exit 1 ;;
esac

case "$archive" in
  /*) ;;
  *) archive="$(pwd)/$archive" ;;
esac
test -f "$archive" || { echo "no archive at $archive" >&2; exit 1; }

scratch=$(mktemp -d "${TMPDIR:-/tmp}/alpha2-archive-smoke.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
trap 'exit 1' INT TERM
case "$archive" in
  *.tar.gz)
    package_name=$(basename "$archive" .tar.gz)
    tar -xzf "$archive" -C "$scratch"
    ;;
  *.zip)
    package_name=$(basename "$archive" .zip)
    if command -v unzip >/dev/null 2>&1; then
      unzip -q "$archive" -d "$scratch"
    else
      # Git Bash on a Windows runner has no unzip; 7-Zip is on its image.
      . "$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)/scripts/lib/windows-tools.sh"
      seven_zip=$(windows_tool 7z 7-Zip) || {
        echo "unpacking $archive needs unzip or 7z (7-Zip): on PATH or in Program Files/7-Zip" >&2
        exit 1
      }
      "$seven_zip" x -y "-o$(cygpath -w "$scratch")" "$(cygpath -w "$archive")" >/dev/null
    fi
    ;;
  *) echo "unsupported archive: $archive" >&2; exit 1 ;;
esac
package="$scratch/$package_name"
test -d "$package/bin" || { echo "$archive does not unpack to $package_name/bin" >&2; exit 1; }
# Run from outside the package: on Windows the current folder is a DLL search location, and nothing
# in the archive may rely on it.
cd "$scratch"

# The seven libraries of the locked runtime (manifest.toml).
ffmpeg_libraries='avcodec|avdevice|avfilter|avformat|avutil|swresample|swscale'

failed=0
fail() {
  echo "$archive: $*" >&2
  failed=1
}

# Each executable starts and prints its usage. Its output is shown when it does not.
expect_usage() {
  name=$1
  shift
  if output=$("$@" 2>&1); then :; fi
  case "$output" in
    *"usage: ${name%.exe}"*) ;;
    *) fail "bin/$name did not start from the unpacked archive without a loader environment:
$output" ;;
  esac
}

case "$platform" in
  linux)
    for name in $executables; do
      executable="$package/bin/$name"
      interpreter=$(readelf -l "$executable" | sed -n 's/.*Requesting program interpreter: \(.*\)]$/\1/p')
      test -n "$interpreter" || { fail "bin/$name has no program interpreter"; continue; }
      # --inhibit-cache: a library ld.so.cache knows (say an FFmpeg 8 in /usr/local/lib) must not
      # stand in for one the archive's RUNPATH fails to reach.
      if ! listing=$(env -u LD_LIBRARY_PATH -u LD_PRELOAD "$interpreter" --inhibit-cache --list "$executable" 2>&1); then
        fail "bin/$name does not load from the unpacked archive:
$listing"
        continue
      fi
      outside=$(printf '%s\n' "$listing" | awk -v package="$package/" -v ffmpeg="$ffmpeg_libraries" '
        $1 ~ ("^lib(" ffmpeg ")[.]") && (NF < 3 || index($3, package) != 1) { print }
      ')
      if [ -n "$outside" ]; then
        fail "bin/$name loads FFmpeg from outside the archive:
$outside"
      fi
      expect_usage "$name" env -u LD_LIBRARY_PATH -u LD_PRELOAD "$interpreter" --inhibit-cache "$executable"
    done
    ;;
  macos)
    # Every FFmpeg reference is @rpath-relative; an absolute install name (a Homebrew FFmpeg leaking
    # into the link) would load whatever is at that path on the user's Mac.
    for file in "$package/bin/alpha2" "$package/bin/alpha2-host" "$package"/lib/*.dylib; do
      test -f "$file" || continue
      absolute=$(otool -L "$file" | awk -v ffmpeg="$ffmpeg_libraries" 'NR > 1 && $1 ~ ("(^|/)lib(" ffmpeg ")[.]") && $1 !~ /^@rpath\// { print $1 }')
      if [ -n "$absolute" ]; then
        fail "${file#"$package"/} links FFmpeg by a path the archive does not control:
$absolute"
      fi
    done
    for name in $executables; do
      expect_usage "$name" env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH -u DYLD_INSERT_LIBRARIES \
        "$package/bin/$name"
    done
    ;;
  windows)
    # Only Windows' own directories on PATH, so a DLL the archive lacks is not picked up from the
    # runner's toolchains; the application folder is searched first.
    system_root=$(cygpath -u "${SYSTEMROOT:-C:\\Windows}")
    for name in $executables; do
      expect_usage "$name" env PATH="$system_root/System32:$system_root" "$package/bin/$name"
    done
    ;;
esac

if [ "$failed" -ne 0 ]; then
  exit 1
fi
echo "smoke-tested $archive unpacked, without a loader environment"
