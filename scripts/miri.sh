#!/usr/bin/env bash
# Runs the C-ABI worklet binding's tests under Miri; other unsafe crates call foreign code Miri
# cannot run.
#   scripts/miri.sh [FILTER...]
set -euo pipefail
cd "$(dirname "$0")/.."

# Skipped: rate conversion calls libm's sqrt (x86 inline assembly), and the EQ sweep takes 25+ min.
cargo +nightly miri test -p alpha2-audio-worklet --lib -- "$@" \
  --skip tests::a_reset_starts_the_conversion_again \
  --skip tests::a_source_at_the_instance_rate_or_one_it_cannot_convert_takes_no_conversion \
  --skip tests::audio_at_another_rate_is_converted_as_the_engine_converts_it_whole \
  --skip tests::a_low_eq_band_filters_alike_at_every_context_rate
