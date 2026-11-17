#!/usr/bin/env bash
# usage: android-device-tests.sh <log> <test binary>...
set -uo pipefail

log=$1
shift
dir=/data/local/tmp/alpha2-test
adb shell mkdir -p "$dir/tests"
adb push tests/corpus tests/cases tests/fonts tests/golden "$dir/tests/" > /dev/null
status=0
for binary in "$@"; do
  name=$(basename "$binary")
  adb push "$binary" "$dir/" > /dev/null
  echo "     Running $name" | tee -a "$log"
  adb shell "cd $dir && chmod 755 $name && ALPHA2_TESTS_DIR=$dir/tests TMPDIR=$dir ALPHA2_SKIP_LOG=$dir/skip-log.jsonl ${ALPHA2_DEVICE_TEST_ENV:-} ./$name --test-threads 1" 2>&1 |
    tee -a "$log" || status=1
done
adb pull "$dir/skip-log.jsonl" "$ALPHA2_SKIP_LOG" > /dev/null 2>&1 || true
exit "$status"
