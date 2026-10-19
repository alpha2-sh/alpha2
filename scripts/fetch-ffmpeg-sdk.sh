#!/bin/sh
# Fetches the pinned LGPL FFmpeg SDK for a target (default: this machine) into a shared cache,
# verifies it against release/ffmpeg-lgpl/manifest.toml and prints its root.
#   fetch-ffmpeg-sdk.sh [linux-x64|linux-arm64|macos-arm64|win-x64]
# ALPHA2_FFMPEG_CACHE overrides the cache directory.
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$workspace/release/ffmpeg-lgpl/manifest.toml"

scalar() {
  key=$1
  awk -F ' *= *' -v key="$key" '
    $1 == key {
      value = $2
      sub(/^[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' "$manifest"
}

asset_value() {
  target=$1
  key=$2
  awk -F ' *= *' -v section="[assets.$target]" -v key="$key" '
    $0 == section { active = 1; next }
    active && /^\[/ { exit }
    active && $1 == key {
      value = $2
      sub(/^[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' "$manifest"
}

detect_target() {
  os=$(uname -s)
  machine=$(uname -m)
  case "$os-$machine" in
    Linux-x86_64) echo linux-x64 ;;
    Linux-aarch64|Linux-arm64) echo linux-arm64 ;;
    Darwin-arm64) echo macos-arm64 ;;
    Darwin-x86_64) echo macos-x64 ;;
    MINGW*-x86_64|MSYS*-x86_64|CYGWIN*-x86_64) echo win-x64 ;;
    *) echo "unsupported FFmpeg SDK host: $os-$machine" >&2; exit 1 ;;
  esac
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

. "$workspace/scripts/lib/windows-tools.sh"

llvm_tool() {
  windows_tool "$1" LLVM/bin || {
    echo "the Windows FFmpeg SDK needs $1 (LLVM) to make MSVC import libraries: on PATH or in Program Files/LLVM/bin" >&2
    exit 1
  }
}

# The Windows SDK is a MinGW build with only GNU import libraries; the MSVC linker needs
# `avcodec.lib`, so one is made from each pinned DLL's export table, in the private stage before
# publishing. Exports outside the executable sections (FFmpeg 8's `av_md5_size`, `*_ffversion`)
# are marked DATA: otherwise llvm-dlltool makes a code thunk a Rust `extern static` would read.
#
# Prints one DLL's .def EXPORTS lines; fails when there are none. POSIX awk cannot convert the
# hexadecimal addresses itself.
export_definitions='
function hex(text,   value, i) {
  value = 0
  text = tolower(text)
  sub(/^0x/, "", text)
  for (i = 1; i <= length(text); i++)
    value = value * 16 + index("0123456789abcdef", substr(text, i, 1)) - 1
  return value
}
FNR == 1 { file++ }
file == 1 && $1 == "VirtualSize:" { size = hex($2) }
file == 1 && $1 == "VirtualAddress:" { start = hex($2) }
file == 1 && $1 == "IMAGE_SCN_MEM_EXECUTE" { code++; code_start[code] = start; code_end[code] = start + size }
file == 2 && $1 == "Name:" { name = $2 }
file == 2 && $1 == "RVA:" && name != "" {
  address = hex($2)
  data = " DATA"
  for (i = 1; i <= code; i++)
    if (address >= code_start[i] && address < code_end[i]) data = ""
  print "  " name data
  name = ""
  found = 1
}
END { exit !found }
'

msvc_import_libraries() {
  sdk=$1
  readobj=$(llvm_tool llvm-readobj)
  dlltool=$(llvm_tool llvm-dlltool)
  for dll in "$sdk"/bin/*.dll; do
    file=$(basename "$dll")
    name=${file%-*.dll}
    definition="$sdk/lib/$name.def"
    # Separate commands, not a pipeline, so a failing llvm-readobj is not masked.
    "$readobj" --sections "$(native_path "$dll")" > "$definition.sections"
    "$readobj" --coff-exports "$(native_path "$dll")" > "$definition.exports"
    if ! awk "$export_definitions" "$definition.sections" "$definition.exports" \
      > "$definition.names"; then
      echo "llvm-readobj listed no exports for $file" >&2
      exit 1
    fi
    {
      echo "LIBRARY $file"
      echo "EXPORTS"
      cat "$definition.names"
    } > "$definition"
    rm -f "$definition.sections" "$definition.exports" "$definition.names"
    "$dlltool" -m i386:x86-64 -d "$(native_path "$definition")" \
      -l "$(native_path "$sdk/lib/$name.lib")"
  done
}

# Fetches sharing a cache take turns through a lock directory (`mkdir` is atomic everywhere; macOS
# has no flock). It holds its owner's PID so a dead owner's lock is taken over. Per machine only.
acquire_lock() {
  lock=$1
  waiting=
  until mkdir "$lock" 2>/dev/null; do
    owner=$(cat "$lock/pid" 2>/dev/null || true)
    if lock_is_abandoned "$lock" "$owner"; then
      # Moved aside first so only one of two fetches takes it over; put back if someone took it between.
      if mv "$lock" "$lock.abandoned-$$" 2>/dev/null; then
        if [ "$(cat "$lock.abandoned-$$/pid" 2>/dev/null || true)" = "$owner" ] || [ -e "$lock" ]; then
          rm -rf "$lock.abandoned-$$"
        else
          mv "$lock.abandoned-$$" "$lock"
        fi
      fi
      continue
    fi
    if [ -z "$waiting" ]; then
      echo "waiting for another fetch of the $target FFmpeg SDK (lock $lock)" >&2
      waiting=1
    fi
    sleep 1
  done
  echo "$$" > "$lock/pid"
}

# Abandoned when its owner no longer runs, or ownerless for a minute (the owner writes its PID
# right after making it).
lock_is_abandoned() {
  if [ -n "$2" ]; then
    ! ps -p "$2" >/dev/null 2>&1
  else
    [ -n "$(find "$1" -prune -mmin +1 2>/dev/null)" ]
  fi
}

download() {
  url=$1
  output=$2
  expected=$3
  if [ -f "$output" ] && [ "$(hash_file "$output")" = "$expected" ]; then
    return
  fi
  partial="$output.partial.$$"
  trap 'rm -f "$partial"' EXIT HUP INT TERM
  echo "downloading $(basename "$output")" >&2
  # curl has no stall timeout: under 1 KiB/s for a minute counts as a timeout, which --retry takes.
  curl --fail --location --retry 3 --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 --output "$partial" "$url"
  actual=$(hash_file "$partial")
  if [ "$actual" != "$expected" ]; then
    echo "checksum mismatch for $(basename "$output"): expected $expected, got $actual" >&2
    exit 1
  fi
  mv "$partial" "$output"
  trap - EXIT HUP INT TERM
}

target=${1:-$(detect_target)}
repository=$(scalar repository)
release_tag=$(scalar release_tag)
archive=$(asset_value "$target" archive)
archive_sha=$(asset_value "$target" sha256)
report=$(asset_value "$target" report)
report_sha=$(asset_value "$target" report_sha256)

if [ -z "$archive" ] || [ -z "$archive_sha" ] || [ -z "$report" ] || [ -z "$report_sha" ]; then
  echo "unknown or incomplete FFmpeg SDK target: $target" >&2
  exit 1
fi

cache_root=${ALPHA2_FFMPEG_CACHE:-$workspace/target/ffmpeg-sdk-cache}
# A Windows cache path becomes a Git Bash one: its sha256sum prefixes backslash names with "\",
# and its GNU tar reads "C:" as a remote host.
if command -v cygpath >/dev/null 2>&1; then
  cache_root=$(cygpath -u "$cache_root")
fi
release_root="$cache_root/$release_tag"
downloads="$release_root/downloads"
sdk_root="$release_root/sdk/$target"
base_url="https://github.com/$repository/releases/download/$release_tag"
mkdir -p "$downloads" "$release_root/sdk"

download "$base_url/$archive" "$downloads/$archive" "$archive_sha"
download "$base_url/$report" "$downloads/$report" "$report_sha"

# The ready marker holds the archive's sha256, so an archive re-pinned under the same tag is
# extracted again (verify-runtime.sh checks names, not contents).
ready="$sdk_root/.alpha2-sdk-ready"
is_ready() {
  [ "$(cat "$ready" 2>/dev/null || true)" = "$archive_sha" ]
}

# A published SDK is built in a private stage under the lock and appears in one rename; a fetch
# that finds it ready only verifies it, so it never removes a tree another fetch returned.
if ! is_ready; then
  lock="$release_root/sdk/.lock-$target"
  stage="$release_root/sdk/.extract-$target-$$"
  trap 'rm -rf "$stage"; [ "$(cat "$lock/pid" 2>/dev/null || true)" != "$$" ] || rm -rf "$lock"' EXIT
  trap 'exit 1' HUP INT TERM
  acquire_lock "$lock"
  if ! is_ready; then
    mkdir -p "$stage"
    case "$archive" in
      *.zip)
        # Git Bash on a Windows runner has no unzip; 7-Zip is on its image.
        if command -v unzip >/dev/null 2>&1; then
          unzip -q "$downloads/$archive" -d "$stage"
        else
          seven_zip=$(windows_tool 7z 7-Zip) || {
            echo "extracting $archive needs unzip or 7z (7-Zip): on PATH or in Program Files/7-Zip" >&2
            exit 1
          }
          "$seven_zip" x -y "-o$(native_path "$stage")" "$(native_path "$downloads/$archive")" >/dev/null
        fi
        ;;
      *.tar.gz) tar -xzf "$downloads/$archive" -C "$stage" ;;
      *) echo "unsupported SDK archive: $archive" >&2; exit 1 ;;
    esac
    extracted="$stage/$(basename "$archive" .zip)"
    extracted=$(printf '%s' "$extracted" | sed 's/\.tar\.gz$//')
    if [ ! -d "$extracted" ]; then
      echo "archive did not contain the expected SDK root: $extracted" >&2
      exit 1
    fi
    case "$target" in
      win-*) msvc_import_libraries "$extracted" ;;
    esac
    printf '%s\n' "$archive_sha" > "$extracted/.alpha2-sdk-ready"
    # A tree from another archive, or left unmarked by a killed fetch, is removed with the stage.
    if [ -e "$sdk_root" ]; then
      mv "$sdk_root" "$stage/replaced"
    fi
    mv "$extracted" "$sdk_root"
  fi
  rm -rf "$stage" "$lock"
  trap - EXIT HUP INT TERM
fi

"$workspace/release/ffmpeg-lgpl/verify-runtime.sh" \
  "$sdk_root" "$target" "$downloads/$report"
printf '%s\n' "$sdk_root"
