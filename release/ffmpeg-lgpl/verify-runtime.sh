#!/bin/sh
# Checks an FFmpeg SDK or runtime directory for <target> against manifest.toml: the configure
# report's options and hardware inventory, the shared libraries and notices, and no static
# libraries. With a fourth argument `executables`, also requires the alpha2 executables in bin/.
#   verify-runtime.sh <dir> <target> <configure-report> [executables]
set -eu

root=${1:?usage: verify-runtime.sh sdk-or-runtime-directory target configure-report}
target=${2:?usage: verify-runtime.sh sdk-or-runtime-directory target configure-report}
report=${3:?usage: verify-runtime.sh sdk-or-runtime-directory target configure-report}
verify_executables=${4:-}

test -f "$report"

# The policy is read from `manifest.toml` rather than restated here, so a tightened lock file is
# enforced at once.
manifest_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
manifest="$manifest_dir/manifest.toml"
test -f "$manifest" || {
  echo "missing FFmpeg runtime lock file: $manifest" >&2
  exit 1
}

# Reads one single-line-per-entry TOML string array out of the lock file.
read_array() {
  awk -v key="$1" '
    index($0, key " = [") == 1 { inside = 1; next }
    inside && index($0, "]") == 1 { exit }
    inside {
      line = $0
      sub(/^[ \t]*"/, "", line)
      sub(/",?[ \t]*$/, "", line)
      if (line != "") print line
    }
  ' "$manifest"
}

# An array that is missing or renamed reads as empty, which would pass every loop below without
# checking anything. Each one that must have entries says so.
require_array() {
  value=$(read_array "$1")
  test -n "$value" || {
    echo "FFmpeg runtime lock file declares no $1" >&2
    exit 1
  }
  printf '%s\n' "$value"
}

required_options=$(require_array required_configure)
forbidden_options=$(require_array forbidden_configure)

printf '%s\n' "$required_options" | while IFS= read -r option; do
  grep -qF -- "$option" "$report" || {
    echo "FFmpeg configure report is missing required option: $option" >&2
    exit 1
  }
done

printf '%s\n' "$forbidden_options" | while IFS= read -r option; do
  if grep -qF -- "$option" "$report"; then
    echo "FFmpeg configure report contains forbidden option: $option" >&2
    exit 1
  fi
done

# The upstream fixes the lock file says the SDK applies, as `<file> sha256 <hash> upstream <commit>`
# lines: the report's "Applied patches" section must list exactly these, in this order.
expected_patches=$(sed -n 's/.*file = "\([^"]*\)", upstream = "\([0-9a-f]*\)", sha256 = "\([0-9a-f]*\)".*/\1 sha256 \3 upstream \2/p' "$manifest")
applied_patches=$(awk '
  $0 == "Applied patches" { inside = 1; getline; next }
  inside && ($0 == "" || $0 == "none") { exit }
  inside { print }
' "$report")
test "$applied_patches" = "$expected_patches" || {
  echo "FFmpeg configure report's applied patches are not the lock file's:" >&2
  printf 'report:\n%s\nlock file:\n%s\n' "${applied_patches:-none}" "${expected_patches:-none}" >&2
  exit 1
}

# The hardware options are the one policy not in the lock file: what each target accelerates with is
# a build property, not a licence one. Each option is looked for on its own.
case "$target" in
  linux-*)
    platform=linux
    library_dir="$root/lib"
    hardware_options='--enable-vulkan
--disable-vaapi
--disable-vdpau
--disable-libdrm
--disable-v4l2-m2m
--disable-libmfx
--disable-libvpl'
    hardware_inventory='hwdevice: vulkan'
    ;;
  macos-*)
    platform=macos
    library_dir="$root/lib"
    hardware_options='--enable-videotoolbox'
    hardware_inventory='hwdevice: videotoolbox'
    ;;
  win-*)
    platform=windows
    library_dir="$root/bin"
    hardware_options='--enable-d3d11va
--enable-d3d12va
--enable-dxva2
--enable-mediafoundation'
    hardware_inventory='hwdevice: d3d11va
hwdevice: d3d12va
hwdevice: dxva2'
    ;;
  *)
    echo "unsupported FFmpeg runtime target: $target" >&2
    exit 1
    ;;
esac

libraries=$(require_array "libraries_$platform")

printf '%s\n' "$hardware_options" | while IFS= read -r option; do
  test -z "$option" || grep -qF -- "$option" "$report" || {
    echo "FFmpeg configure report is missing target hardware option: $option" >&2
    exit 1
  }
done
printf '%s\n' "$hardware_inventory" | while IFS= read -r entry; do
  grep -qF -- "$entry" "$report" || {
    echo "FFmpeg configure report is missing hardware inventory entry: $entry" >&2
    exit 1
  }
done

printf '%s\n' "$libraries" | while IFS= read -r library; do
  test -e "$library_dir/$library" || {
    echo "missing FFmpeg runtime library: $library_dir/$library" >&2
    exit 1
  }
done

if [ "$verify_executables" = executables ]; then
  case "$target" in
    win-*) alpha2_executable=alpha2.exe; host_executable=alpha2-host.exe ;;
    *) alpha2_executable=alpha2; host_executable=alpha2-host ;;
  esac
  for executable in "$alpha2_executable" "$host_executable"; do
    test -f "$root/bin/$executable" || {
      echo "missing native release executable: $root/bin/$executable" >&2
      exit 1
    }
  done
fi

if find "$root" -type f \( -name 'libav*.a' -o -name 'libsw*.a' \) \
  ! -name '*.dll.a' -print | grep -q .; then
  echo "static FFmpeg library found in runtime" >&2
  exit 1
fi

# `notices` ships everywhere; `notices_<platform>` is the per-target addition, and only Linux
# declares one.
notices=$(require_array notices)
target_notices=$(read_array "notices_$platform")
if [ -n "$target_notices" ]; then
  notices="$notices
$target_notices"
fi
printf '%s\n' "$notices" | while IFS= read -r notice; do
  test -f "$root/$notice" || {
    echo "missing FFmpeg runtime notice: $notice" >&2
    exit 1
  }
done

echo "verified FFmpeg shared runtime for $target" >&2
