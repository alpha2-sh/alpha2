#!/bin/sh
# Checks that a native archive's executables start with no loader environment, so they can only
# load the FFmpeg the archive itself points at, and then that they work from there: the feature
# report of alpha2, a capability report and a CPU-renderer export of alpha2-host.
# --loader-only skips the second part, for stand-in executables.
#   smoke-archive.sh <archive.tar.gz|archive.zip> <target> [--loader-only]
set -eu

usage="usage: smoke-archive.sh <archive> <target> [--loader-only]"
archive=${1:?$usage}
target=${2:?$usage}
case "${3:-}" in
  "") run_checks=1 ;;
  --loader-only) run_checks=0 ;;
  *) echo "$usage" >&2; exit 2 ;;
esac
workspace=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)

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
      . "$workspace/scripts/lib/windows-tools.sh"
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
# On Windows the current folder is a DLL search location; the archive must not rely on it.
cd "$scratch"

ffmpeg_libraries='avcodec|avdevice|avfilter|avformat|avutil|swresample|swscale'

failed=0
fail() {
  echo "$archive: $*" >&2
  failed=1
}

if [ "$platform" = windows ]; then
  system_root=$(cygpath -u "${SYSTEMROOT:-C:\\Windows}")
fi

# clean <name> [arguments...] runs bin/<name> with no loader environment: on Linux through its
# program interpreter without the ld.so cache, on Windows with only the Windows directories on PATH.
clean() {
  name=$1
  shift
  case "$platform" in
    linux)
      interpreter=$(readelf -l "$package/bin/$name" | sed -n 's/.*Requesting program interpreter: \(.*\)]$/\1/p')
      env -u LD_LIBRARY_PATH -u LD_PRELOAD "$interpreter" --inhibit-cache "$package/bin/$name" "$@"
      ;;
    macos)
      env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH -u DYLD_INSERT_LIBRARIES \
        "$package/bin/$name" "$@"
      ;;
    windows) env PATH="$system_root/System32:$system_root" "$package/bin/$name" "$@" ;;
  esac
}

expect_usage() {
  name=$1
  if output=$(clean "$name" 2>&1); then :; fi
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
      # --inhibit-cache so a system FFmpeg cannot stand in for one RUNPATH fails to reach.
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
      expect_usage "$name"
    done
    ;;
  macos)
    # An absolute install name (e.g. a leaked Homebrew FFmpeg) would load whatever is at that path.
    for file in "$package/bin/alpha2" "$package/bin/alpha2-host" "$package"/lib/*.dylib; do
      test -f "$file" || continue
      absolute=$(otool -L "$file" | awk -v ffmpeg="$ffmpeg_libraries" 'NR > 1 && $1 ~ ("(^|/)lib(" ffmpeg ")[.]") && $1 !~ /^@rpath\// { print $1 }')
      if [ -n "$absolute" ]; then
        fail "${file#"$package"/} links FFmpeg by a path the archive does not control:
$absolute"
      fi
    done
    for name in $executables; do
      expect_usage "$name"
    done
    ;;
  windows)
    for name in $executables; do
      expect_usage "$name"
    done
    ;;
esac

if [ "$failed" -ne 0 ]; then
  exit 1
fi

if [ "$run_checks" -eq 1 ]; then
  # shellcheck disable=SC2086
  set -- $executables
  cli=$1
  host=$2
  native() {
    if [ "$platform" = windows ]; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
  }
  # le32 <n>: n as the four little-endian bytes the protocol's frame lengths use.
  le32() {
    # shellcheck disable=SC2059
    printf "$(printf '\\%03o\\%03o\\%03o\\%03o' $(($1 % 256)) $(($1 / 256 % 256)) \
      $(($1 / 65536 % 256)) $(($1 / 16777216)))"
  }

  if ! features=$(clean "$cli" features 2>&1); then
    fail "bin/$cli features failed:
$features"
  else
    for feature in hwdecode hwencode timeline; do
      printf '%s\n' "$features" | grep -qx "$feature" || fail "bin/$cli is built without $feature"
    done
  fi

  header='{"protocolVersion":2,"requestId":"smoke","operation":"capabilities"}'
  { le32 ${#header}; le32 0; printf '%s' "$header"; } > "$scratch/request"
  if ! ALPHA2_RENDERER=cpu clean "$host" serve < "$scratch/request" > "$scratch/answer" 2> "$scratch/serve.log" ||
    ! grep -aq '"requestId":"smoke"' "$scratch/answer" || ! grep -aq '"ok":true' "$scratch/answer" ||
    ! grep -aq '"status":"ready"' "$scratch/answer"; then
    fail "bin/$host serve gave no capability report:
$(cat "$scratch/serve.log")
$(tr -c '[:print:]\n' '.' < "$scratch/answer" | head -c 2000)"
  fi

  corpus="$workspace/tests/corpus"
  if ! result=$(clean "$host" export --renderer cpu \
    --project "$(native "$workspace/tests/cases/host-parity-complex-project.json")" \
    --asset "video=$(native "$corpus/parity-rgb.mov")" --asset "still=$(native "$corpus/parity-image.png")" \
    --out "$(native "$scratch/export.mp4")" --video-codec mpeg4-part2 2>&1) ||
    ! printf '%s' "$result" | grep -q '"frames"'; then
    fail "bin/$host did not export with the CPU renderer:
$result"
  fi
  if [ "$failed" -ne 0 ]; then
    exit 1
  fi
  echo "smoke-tested $archive unpacked, without a loader environment, and ran it"
else
  echo "smoke-tested $archive unpacked, without a loader environment"
fi
