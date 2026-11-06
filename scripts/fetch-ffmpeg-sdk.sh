#!/bin/sh
# Fetches the pinned LGPL FFmpeg SDK for a target (default: this machine) into a shared cache,
# verifies it against release/ffmpeg-lgpl/manifest.toml and prints its root.
#   fetch-ffmpeg-sdk.sh [linux-x64|linux-arm64|macos-arm64|win-x64]
# ALPHA2_FFMPEG_CACHE overrides the cache directory. ALPHA2_FFMPEG_FRESH=1 re-extracts the SDK.
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

. "$workspace/scripts/lib/windows-tools.sh"
. "$workspace/scripts/lib/pinned-download.sh"

llvm_tool() {
  windows_tool "$1" LLVM/bin || {
    echo "the Windows FFmpeg SDK needs $1 (LLVM) to make MSVC import libraries: on PATH or in Program Files/LLVM/bin" >&2
    exit 1
  }
}

# The Windows SDK is a MinGW build, so MSVC import libraries are made from each DLL's exports.
# Exports outside executable sections (`av_md5_size`, `*_ffversion`) are marked DATA; otherwise
# llvm-dlltool makes a code thunk that a Rust `extern static` would read.
#
# Prints one DLL's .def EXPORTS lines. POSIX awk cannot parse hexadecimal addresses itself.
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
# Git Bash's sha256sum escapes backslash names and its GNU tar reads "C:" as a remote host.
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

# The ready marker holds the archive's sha256, so a re-pinned archive under the same tag is
# extracted again.
ready="$sdk_root/.alpha2-sdk-ready"
fresh=${ALPHA2_FFMPEG_FRESH:-}
is_ready() {
  [ "$fresh" != 1 ] && [ "$(cat "$ready" 2>/dev/null || true)" = "$archive_sha" ]
}

# Extract into a private stage and publish with one rename, so a tree another fetch returned is
# never removed.
if ! is_ready; then
  lock="$release_root/sdk/.lock-$target"
  stage="$release_root/sdk/.extract-$target-$$"
  trap 'rm -rf "$stage"; [ "$(cat "$lock/pid" 2>/dev/null || true)" != "$$" ] || rm -rf "$lock"' EXIT
  trap 'exit 1' HUP INT TERM
  acquire_lock "$lock"
  if ! is_ready; then
    mkdir -p "$stage"
    extract_archive "$downloads/$archive" "$stage"
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
