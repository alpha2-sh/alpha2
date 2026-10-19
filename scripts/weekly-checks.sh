#!/usr/bin/env bash
# The checks too slow for every change, run on a schedule (weekly) on a Linux x64 machine with the
# GPU the tests use, and summed up in one report:
#
#   fuzz         every fuzz target (scripts/fuzz.sh, the extension targets included), all at once,
#                ALPHA2_WEEKLY_FUZZ_SECONDS each (default 3600)
#   miri         the C-ABI worklet binding's tests under Miri (nightly); the other unsafe crates
#                call foreign code Miri cannot run. ALPHA2_WEEKLY_MIRI_FILTER narrows the run.
#   valgrind     the FFmpeg bridge and the native binding's tests under memcheck, and the GPU wait
#                timeout setter under ThreadSanitizer
#   determinism  the pinned audio hashes and evaluation constants natively, and the engine mix
#                natively against wasm (npm run test:audio-determinism)
#   bench        the workspace benchmarks, ALPHA2_WEEKLY_BENCH_RUNS times (default 3), compared
#                with a baseline by scripts/lib/bench-compare.mjs: a time more than
#                ALPHA2_WEEKLY_BENCH_THRESHOLD percent (default 5) slower is a regression if it
#                still is after as many runs again
#
#   scripts/weekly-checks.sh [step...]          (default: every step, bench first)
#
# Output goes to ALPHA2_WEEKLY_DIR (default .local/weekly): <dir>/<UTC time>/report.txt plus a log
# per step, copied to <dir>/latest-report.txt. The report's first line is `ok`, or
# `FAIL: <steps>` and/or `REGRESSION: ...`; the exit status is 1 when it is not `ok`.
#
# The baseline is <dir>/bench-baseline.txt (times belong to one machine). The first bench run
# records it and later runs never move it, so slow drift shows; ALPHA2_WEEKLY_UPDATE_BASELINE=1
# makes that run's times the baseline.
#
# Schedule it yourself, e.g. crontab:
#   0 3 * * 6  cd <checkout> && scripts/weekly-checks.sh > .local/weekly/cron.log 2>&1
# Run it while the machine is idle. Benchmarks run first: run right after the other steps, which
# load every core for up to an hour, they measured up to 88 % slower.
#
# Needs: nightly with cargo-fuzz and miri, valgrind, a C compiler with ThreadSanitizer, node, and
# the pinned FFmpeg SDK (fetched by scripts/fetch-ffmpeg-sdk.sh when missing).
set -uo pipefail
cd "$(dirname "$0")/.."

steps=("$@")
[ ${#steps[@]} -gt 0 ] || steps=(bench fuzz miri valgrind determinism)
for step in "${steps[@]}"; do
  case $step in
    fuzz | miri | valgrind | determinism | bench) ;;
    *) echo "unknown step $step (bench fuzz miri valgrind determinism)" >&2; exit 2 ;;
  esac
done

dir=${ALPHA2_WEEKLY_DIR:-.local/weekly}
out=$dir/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$out"
baseline=$dir/bench-baseline.txt
threshold=${ALPHA2_WEEKLY_BENCH_THRESHOLD:-5}

# The pinned FFmpeg SDK: the system's may differ from the one the tests expect.
sdk=$(scripts/fetch-ffmpeg-sdk.sh) || { echo "no FFmpeg SDK" >&2; exit 2; }
export FFMPEG_PKG_CONFIG_PATH=$sdk/lib/pkgconfig PKG_CONFIG_PATH=$sdk/lib/pkgconfig
export LD_LIBRARY_PATH=$sdk/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH} FFMPEG_LINK_MODE=dynamic

step_fuzz() { scripts/fuzz.sh "${ALPHA2_WEEKLY_FUZZ_SECONDS:-3600}"; }

step_miri() {
  # Skipped: sample rate conversion calls libm's sqrt, inline assembly on x86 that Miri cannot run,
  # and the low EQ band sweep takes over 25 minutes under Miri.
  # shellcheck disable=SC2086 # the filter is optional and one word
  cargo +nightly miri test -p alpha2-audio-worklet --lib -- ${ALPHA2_WEEKLY_MIRI_FILTER:-} \
    --skip tests::a_reset_starts_the_conversion_again \
    --skip tests::a_source_at_the_instance_rate_or_one_it_cannot_convert_takes_no_conversion \
    --skip tests::audio_at_another_rate_is_converted_as_the_engine_converts_it_whole \
    --skip tests::a_low_eq_band_filters_alike_at_every_context_rate
}

step_valgrind() {
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="valgrind --error-exitcode=9 --quiet" \
    ALPHA2_TEST_DECODE_WORK_LIMIT_MS=600000 \
    cargo test -p alpha2-ffmpeg-bridge -p alpha2-native --features alpha2-ffmpeg-bridge/hwdevice \
    --lib --bins --tests --no-fail-fast &&
    bash scripts/test-gpu-timeout-race.sh
}

step_determinism() {
  cargo test -p alpha2-audio -p alpha2-audio-determinism -p alpha2-eval -p alpha2-text \
    --no-fail-fast && npm run test:audio-determinism
}

bench_regressed=
step_bench() {
  local runs=${ALPHA2_WEEKLY_BENCH_RUNS:-3} run rc
  : > "$out/bench.txt"
  bench_runs() {
    for ((run = 1; run <= runs; run++)); do
      echo "== bench run $run of $runs"
      cargo bench --workspace --all-features >> "$out/bench.txt" || return 1
    done
  }
  compare() {
    node scripts/lib/bench-compare.mjs "$baseline" "$out/bench.txt" --threshold "$threshold" \
      > "$out/bench-compare.txt"
  }
  bench_runs || return 1
  if [ ! -f "$baseline" ]; then
    cp "$out/bench.txt" "$baseline"
    echo "recorded the baseline $baseline"
    return 0
  fi
  compare
  rc=$?
  if [ $rc = 1 ]; then
    # A regression has to hold over twice the runs: a real one stays, a noisy run's goes.
    cat "$out/bench-compare.txt"
    echo "== confirming over $runs more runs"
    bench_runs || return 1
    compare
    rc=$?
  fi
  cat "$out/bench-compare.txt"
  case $rc in
    0) ;;
    1) bench_regressed=$(head -1 "$out/bench-compare.txt" | sed -n 's/^REGRESSION: \([0-9]*\) of.*/\1/p') ;;
    *) return 1 ;;
  esac
  if [ "${ALPHA2_WEEKLY_UPDATE_BASELINE:-0}" = 1 ]; then
    cp "$out/bench.txt" "$baseline"
    echo "made this run the baseline $baseline"
  fi
}

failed=()
summary=()
for step in "${steps[@]}"; do
  started=$(date +%s)
  echo "== $step (log $out/$step.log)"
  if "step_$step" > "$out/$step.log" 2>&1; then result=ok; else result=FAIL; failed+=("$step"); fi
  line="$step: $result in $(($(date +%s) - started)) s"
  summary+=("$line")
  echo "$line"
done

{
  first=()
  [ ${#failed[@]} -eq 0 ] || first+=("FAIL: ${failed[*]}")
  [ -z "$bench_regressed" ] || first+=("REGRESSION: $bench_regressed benchmark times > $threshold %")
  if [ ${#first[@]} -eq 0 ]; then echo "ok"; else (IFS=';'; echo "${first[*]}" | sed 's/;/; /g'); fi
  echo "run $out, commit $(git rev-parse --short HEAD)$(git diff --quiet HEAD || echo ' (with local changes)')"
  printf '%s\n' "${summary[@]}"
  if [ -f "$out/fuzz.log" ]; then echo; echo "-- fuzz"; grep -E '^[a-z_]+: |^slow ' "$out/fuzz.log"; fi
  if [ -f "$out/bench-compare.txt" ]; then echo; echo "-- bench against $baseline"; cat "$out/bench-compare.txt"; fi
  # ${a[@]+...}: bash 3.2 (macOS /bin/bash) calls an empty array unbound under set -u.
  for step in ${failed[@]+"${failed[@]}"}; do echo; echo "-- $step (last lines of $out/$step.log)"; tail -20 "$out/$step.log"; done
} > "$out/report.txt"
cp "$out/report.txt" "$dir/latest-report.txt"
cat "$out/report.txt"
[ ${#failed[@]} -eq 0 ] && [ -z "$bench_regressed" ]
