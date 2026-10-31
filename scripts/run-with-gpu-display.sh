#!/bin/sh
# Runs a command against an X display that has the machine's GPU, for headed Chrome with
# ALPHA2_CHROME_GPU=machine on Linux. Xvfb cannot serve that: it has no DRI3, and Chrome's GPU process
# then hangs on Mesa's Vulkan drivers. This starts a headless weston with Xwayland on the GL
# renderer, runs the command with DISPLAY on its Xwayland, ALPHA2_BROWSER_HEADED=1 and
# ALPHA2_CHROME_GPU=machine, and stops weston when the command ends.
#   run-with-gpu-display.sh [--gpu amd|nvidia|any] <command...>
# --gpu amd runs weston in a bubblewrap sandbox that shows it only the AMD GPU's device nodes, with
# Mesa's EGL, and points the command's Vulkan loader at the radeon driver: Mesa's surfaceless EGL
# stops at the first DRM device, which on a machine with an NVIDIA card is the NVIDIA one.
# --gpu nvidia uses NVIDIA's EGL and Vulkan drivers; --gpu any (the default) leaves the choice to the
# system. Needs weston (with its headless backend and Xwayland), and bwrap for --gpu amd.
set -eu

gpu=any
if [ "${1:-}" = "--gpu" ]; then
  gpu=${2:-}
  shift 2 || { echo "usage: run-with-gpu-display.sh [--gpu amd|nvidia|any] <command...>" >&2; exit 2; }
fi
case "$gpu" in amd|nvidia|any) ;; *) echo "unknown --gpu $gpu: expected amd, nvidia or any" >&2; exit 2 ;; esac
if [ "$#" -eq 0 ]; then
  echo "usage: run-with-gpu-display.sh [--gpu amd|nvidia|any] <command...>" >&2
  exit 2
fi
command -v weston >/dev/null 2>&1 || { echo "weston is not installed; it serves the GPU display" >&2; exit 1; }

# The card and render nodes of the first DRM device whose PCI vendor is $1.
device_nodes() {
  for render in /sys/class/drm/renderD*; do
    [ "$(cat "$render/device/vendor" 2>/dev/null)" = "$1" ] || continue
    for card in /sys/class/drm/card*; do
      case "$card" in *-*) continue ;; esac
      if [ "$(readlink -f "$card/device")" = "$(readlink -f "$render/device")" ]; then
        echo "/dev/dri/$(basename "$card") /dev/dri/$(basename "$render")"
        return 0
      fi
    done
  done
  return 1
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/alpha2-gpu-display-XXXXXX")
log="$scratch/weston.log"
socket="alpha2-$$"
weston_pid=
cleanup() {
  if [ -n "$weston_pid" ]; then
    kill "$weston_pid" 2>/dev/null || true
    wait "$weston_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

weston_args="--backend=headless-backend.so --use-gl --xwayland --socket=$socket --idle-time=0 --log=$log"
case "$gpu" in
  amd)
    command -v bwrap >/dev/null 2>&1 || { echo "bwrap is not installed; --gpu amd needs it to hide the other GPUs from weston" >&2; exit 1; }
    nodes=$(device_nodes 0x1002) || { echo "no AMD GPU found under /sys/class/drm" >&2; exit 1; }
    binds=
    for node in $nodes; do binds="$binds --dev-bind $node $node"; done
    # shellcheck disable=SC2086
    __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json \
      bwrap --die-with-parent --dev-bind / / --dev /dev $binds weston $weston_args &
    weston_pid=$!
    export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/radeon_icd.x86_64.json
    ;;
  nvidia)
    # shellcheck disable=SC2086
    __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/10_nvidia.json weston $weston_args &
    weston_pid=$!
    export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json
    ;;
  any)
    # shellcheck disable=SC2086
    weston $weston_args &
    weston_pid=$!
    ;;
esac

display=
waited=0
while [ "$waited" -lt 100 ]; do
  display=$(sed -n 's/.*xserver listening on display \(:[0-9][0-9]*\).*/\1/p' "$log" 2>/dev/null | head -n 1)
  [ -n "$display" ] && break
  if ! kill -0 "$weston_pid" 2>/dev/null; then
    echo "weston exited before its Xwayland started:" >&2
    cat "$log" >&2 2>/dev/null || true
    exit 1
  fi
  sleep 0.1
  waited=$((waited + 1))
done
if [ -z "$display" ]; then
  echo "weston's Xwayland did not start within 10 s:" >&2
  cat "$log" >&2 2>/dev/null || true
  exit 1
fi

status=0
env -u WAYLAND_DISPLAY DISPLAY="$display" ALPHA2_BROWSER_HEADED=1 ALPHA2_CHROME_GPU=machine "$@" || status=$?
exit "$status"
