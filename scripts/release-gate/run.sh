#!/usr/bin/env bash
# The manual release gate on one machine (lin, win or mac; see machines.json). Runs the named
# phases in order, each as steps with one log per step, and writes
# target/release-gate/<machine>-<UTC stamp>/:
#   summary.txt   one line per step: <phase>/<step> rc=<rc> <seconds>s load=<load average>
#   <phase>-<step>.log, header.json, skip-log.jsonl (ALPHA2_SKIP_LOG), report.json
# report.json matches every failure against known-failures.json; any failure it does not excuse
# makes the verdict "blocked" and the exit status 1.
#
#   run.sh [--allow-dirty] [--previous <earlier run directory>] <phase>...
#
# Phases: env p1 adapters corruption validation radv browser e2e stress perf
# (adapters and corruption are Windows phases, radv a Linux one). The working tree must be clean
# (exit 2 otherwise) unless --allow-dirty, which marks the run as not a release run. --previous
# names an earlier run of this machine (from the previous release's gate tarball): its skip log
# and perf summary are compared with this run's.
set -uo pipefail

usage() {
  echo "usage: run.sh [--allow-dirty] [--previous <run directory>] <phase>..." >&2
  echo "phases: env p1 adapters corruption validation radv browser e2e stress perf" >&2
  exit 2
}

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root" || exit 2
gate=scripts/release-gate
allow_dirty=0
previous=
phases=()
while [ $# -gt 0 ]; do
  case $1 in
    --allow-dirty) allow_dirty=1 ;;
    --previous) previous=${2:-}; [ -d "$previous" ] || usage; previous=$(cd "$previous" && pwd); shift ;;
    env | p1 | adapters | corruption | validation | radv | browser | e2e | stress | perf) phases+=("$1") ;;
    *) usage ;;
  esac
  shift
done
[ ${#phases[@]} -gt 0 ] || usage

dirty=$(git status --porcelain)
if [ -n "$dirty" ] && [ "$allow_dirty" = 0 ]; then
  echo "the working tree is not clean (run.sh --allow-dirty for a run that is not a release run):"
  echo "$dirty"
  exit 2
fi

# shellcheck source=scripts/release-gate/env.sh
. "$gate/env.sh" || exit 2

run=$root/target/release-gate/$GATE_MACHINE-$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$run"
export ALPHA2_SKIP_LOG=$run/skip-log.jsonl
summary=$run/summary.txt
: > "$summary"
GIT_DIRTY="$dirty" node -e '
  const [machine, phases] = process.argv.slice(1);
  const dirty = process.env.GIT_DIRTY.split("\n").filter(Boolean);
  const head = require("node:child_process").execFileSync("git", ["rev-parse", "HEAD"]).toString().trim();
  require("node:fs").writeFileSync(process.argv[3], JSON.stringify({
    head, machine, phases: phases.split(" "), started: new Date().toISOString(),
    releaseRun: dirty.length === 0, dirty,
  }, null, 2) + "\n");
' "$GATE_MACHINE" "${phases[*]}" "$run/header.json"

machine_value() {
  node -e 'const value = process.argv[2].split(".").reduce((v, k) => v?.[k], require(process.argv[1])[process.argv[3]]);
    process.stdout.write(value === undefined ? "" : Array.isArray(value) ? value.join(",") : String(value));' \
    "$root/$gate/machines.json" "$1" "$GATE_MACHINE"
}
capabilities=$(machine_value capabilities)

load_now() {
  if [ -r /proc/loadavg ]; then cut -d' ' -f1 /proc/loadavg
  elif [ "$GATE_PLATFORM" = macos ]; then sysctl -n vm.loadavg | awk '{print $2}'
  else echo n/a
  fi
}

# step <phase> <name> <command...>: runs the command with its output in <phase>-<name>.log.
step() {
  local phase=$1 name=$2
  shift 2
  local log=$run/$phase-$name.log started=$SECONDS rc
  echo "== $phase/$name: $*"
  "$@" > "$log" 2>&1
  rc=$?
  local line="$phase/$name rc=$rc $((SECONDS - started))s load=$(load_now)"
  echo "$line" >> "$summary"
  echo "   $line"
  return 0
}

host=$root/target/release/alpha2-host
[ "$GATE_PLATFORM" = windows ] && host=$host.exe
# `cargo test` relinks target/release/alpha2-host with the test dependencies' features, so every
# phase that runs the host builds the product binary again first.
build_host() { cargo build --release --locked -p alpha2-host; }

versions() {
  git rev-parse HEAD
  uname -a
  rustc -vV
  cargo --version
  node --version
  npm --version
  case $GATE_PLATFORM in
    linux)
      nvidia-smi --query-gpu=name,driver_version --format=csv,noheader || true
      vulkaninfo --summary || true
      ;;
    macos)
      sw_vers
      system_profiler SPDisplaysDataType
      ;;
    windows)
      powershell -NoProfile -Command \
        "Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion | Format-List"
      ;;
  esac
  cargo run --quiet --release --locked -p alpha2-gpu --example adapters
}

phase_env() {
  step env versions versions
  local browsers=(chrome firefox)
  [ "$GATE_PLATFORM" = windows ] && browsers+=(edge)
  [ "$GATE_PLATFORM" = macos ] && browsers+=(safari)
  step env browsers node "$gate/browsers.mjs" "${browsers[@]}"
  step env known-failures node "$gate/known-failures.mjs"
}

phase_p1() {
  local require=(env ALPHA2_REQUIRE="$capabilities")
  step p1 npm-ci npm ci
  step p1 build npm run build
  step p1 debug "${require[@]}" cargo test --workspace --locked --all-features --no-fail-fast \
    -- --show-output
  step p1 release-strict "${require[@]}" ALPHA2_GPU_STRICT=1 cargo test --workspace --locked \
    --release --all-features --no-fail-fast -- --show-output
  step p1 default-features "${require[@]}" cargo test --workspace --locked --no-fail-fast \
    -- --show-output
  step p1 test-js "${require[@]}" npm run test:js
}

phase_adapters() {
  if [ "$GATE_PLATFORM" != windows ]; then echo "adapters: a Windows phase, skipped"; return; fi
  local adapter
  for adapter in $(machine_value adapters.render | tr , ' '); do
    step adapters "$adapter-render" env ALPHA2_GPU_ADAPTER="$adapter" ALPHA2_GPU_STRICT=1 \
      cargo test --release --locked -p alpha2-render --no-fail-fast -- --show-output
    step adapters "$adapter-engine" env ALPHA2_GPU_ADAPTER="$adapter" ALPHA2_GPU_STRICT=1 \
      cargo test --release --locked -p alpha2-engine --no-fail-fast -- --show-output
  done
  step adapters "$(machine_value adapters.host)-host" \
    env ALPHA2_GPU_ADAPTER="$(machine_value adapters.host)" ALPHA2_GPU_STRICT=1 \
    cargo test --release --locked -p alpha2-host --all-features --no-fail-fast -- --show-output
}

layout_corruption() {
  local out=$run/corruption adapter reference i
  adapter=$(machine_value corruption.adapter)
  reference=$(machine_value corruption.reference)
  mkdir -p "$out"
  build_host || return 1
  layout() {
    env ALPHA2_GPU_ADAPTER="$1" ALPHA2_EXPORT_FRAMES_Y4M="$out/$2.y4m" "$host" export \
      --project tests/cases/host-parity-layout-project.json \
      --asset still=tests/corpus/parity-image.png --font tests/fonts/Alpha2Fixture.ttf \
      --no-audio --overwrite --out "$out/$2.mp4"
  }
  layout "$reference" reference || return 1
  for i in 1 2 3; do layout "$adapter" "run$i" || return 1; done
  node "$gate/y4m-blocks.mjs" "$out/reference.y4m" "$out/run1.y4m" "$out/run2.y4m" "$out/run3.y4m"
}

phase_corruption() {
  if [ "$GATE_PLATFORM" != windows ]; then echo "corruption: a Windows phase, skipped"; return; fi
  step corruption layout-corruption layout_corruption
}

phase_validation() {
  case $GATE_PLATFORM in
    macos) step validation gpu node scripts/test-gpu-validation.mjs --shader-validation ;;
    *) step validation gpu node scripts/test-gpu-validation.mjs ;;
  esac
  if [ "$GATE_PLATFORM" = linux ]; then step validation vulkan npm run test:vulkan-validation; fi
}

phase_radv() {
  if [ "$GATE_PLATFORM" != linux ]; then echo "radv: a Linux phase, skipped"; return; fi
  step radv container bash "$gate/radv-container.sh"
}

phase_browser() {
  local require=(env ALPHA2_REQUIRE="$(machine_value browser.capabilities)")
  # A stock browser promises only that it runs the page: what it lacks without the harness's flags
  # (WebGPU on Linux Chrome, autoplay) is recorded as skips, which is what the pass is for.
  local stock=(env ALPHA2_REQUIRE=browser,wasm ALPHA2_BROWSER_SETTINGS=stock)
  local count index label part
  local -a display
  case " ${phases[*]} " in *" p1 "*) ;; *) step browser build npm run build ;; esac
  count=$(node -p "require('./$gate/machines.json')['$GATE_MACHINE'].browser.displays.length")
  for ((index = 0; index < count; index++)); do
    # A display is a command prefix (Linux: run-with-gpu-display.sh --gpu <vendor>); empty is the
    # machine's own session. (No mapfile: macOS ships bash 3.2.)
    display=()
    while IFS= read -r part; do display+=("$part"); done < <(node -e '
      for (const part of require(process.argv[1])[process.argv[2]].browser.displays[process.argv[3]])
        console.log(part);' "$root/$gate/machines.json" "$GATE_MACHINE" "$index")
    label=default
    [ ${#display[@]} -gt 2 ] && label=${display[2]}
    step browser "wgsl-$label" "${require[@]}" ${display[@]+"${display[@]}"} \
      node scripts/test-wgsl-browser.mjs
    step browser "wasm-$label" "${require[@]}" ${display[@]+"${display[@]}"} \
      node scripts/test-wasm-browser.mjs
    step browser "editor-$label" "${require[@]}" ${display[@]+"${display[@]}"} \
      npm run test:editor:package
    step browser "host-parity-$label" "${require[@]}" ${display[@]+"${display[@]}"} \
      npm run test:host-parity
    if [ "$(machine_value browser.stockNeedsConsole)" = true ]; then
      step browser "stock-$label" "${stock[@]}" bash "$gate/windows-console-run.sh" \
        node scripts/test-wasm-browser.mjs
    else
      step browser "stock-$label" "${stock[@]}" ${display[@]+"${display[@]}"} \
        node scripts/test-wasm-browser.mjs
    fi
  done
}

missing_venv() {
  echo "FAIL no $1: create it with: cd python/alpha2 && uv venv -p 3.11 .venv &&" \
    "uv pip install --group dev && uv pip install --no-deps -e ."
  return 1
}

phase_e2e() {
  # The host adapter where the machine names one, stated even where it is the host's default:
  # the GPU-resident path needs the GPU that holds the hardware encoder.
  local adapter=() host_adapter
  host_adapter=$(machine_value adapters.host)
  [ -n "$host_adapter" ] && adapter=(ALPHA2_GPU_ADAPTER="$host_adapter")
  local require=(env ${adapter[@]+"${adapter[@]}"} ALPHA2_REQUIRE="host,python,$capabilities")
  local python=python/alpha2/.venv/bin/pytest
  [ "$GATE_PLATFORM" = windows ] && python=python/alpha2/.venv/Scripts/pytest.exe
  step e2e node-e2e "${require[@]}" npm run test:node-e2e:release
  step e2e build-host build_host
  if [ -x "$python" ]; then
    step e2e pytest "${require[@]}" ALPHA2_HOST_BINARY="$host" "$python" \
      python/alpha2/tests/test_e2e.py python/alpha2/tests/test_scenes.py
  else
    step e2e pytest missing_venv "$python"
  fi
  step e2e gpu-export env ${adapter[@]+"${adapter[@]}"} ALPHA2_REQUIRE=gpu-resident npm run test:gpu-export
}

phase_stress() {
  local out=$run/stress
  step stress build-host build_host
  step stress projects node "$gate/stress-projects.mjs" "$out"
  step stress heavy-1920x1080 node "$gate/stress-export.mjs" "$out" heavy-1920x1080.json \
    heavy-effects.json
  step stress builtin-worst-1920x1080 node "$gate/stress-export.mjs" "$out" \
    builtin-worst-1920x1080.json
}

phase_perf() {
  local compare=()
  [ -n "$previous" ] && [ -f "$previous/perf/summary.json" ] \
    && compare=(--previous "$previous/perf/summary.json")
  step perf build-host build_host
  step perf exports node "$gate/perf.mjs" --machine "$GATE_MACHINE" --out "$run/perf" \
    ${compare[@]+"${compare[@]}"}
}

for phase in "${phases[@]}"; do "phase_$phase"; done

node scripts/skip-log-summary.mjs "$ALPHA2_SKIP_LOG" > "$run/skips.txt"
if [ -n "$previous" ] && [ -f "$previous/skip-log.jsonl" ]; then
  ALPHA2_REQUIRE="$capabilities" node "$gate/compare-skips.mjs" "$previous/skip-log.jsonl" \
    "$ALPHA2_SKIP_LOG" > "$run/skips-compared.txt"
fi
echo "run directory: $run"
node "$gate/report.mjs" "$run" "$GATE_MACHINE"
