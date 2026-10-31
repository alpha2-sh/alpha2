#!/usr/bin/env bash
# The C-ABI worklet binding's tests under Miri (nightly with the miri component); the other unsafe
# crates call foreign code Miri cannot run. Arguments go to the test binary (a name filter).
#   scripts/miri.sh [FILTER...]
set -euo pipefail
cd "$(dirname "$0")/.."

# Skipped: sample rate conversion calls libm's sqrt, inline assembly on x86 that Miri cannot run,
# and the low EQ band sweep takes over 25 minutes under Miri.
cargo +nightly miri test -p alpha2-audio-worklet --lib -- "$@" \
  --skip tests::a_reset_starts_the_conversion_again \
  --skip tests::a_source_at_the_instance_rate_or_one_it_cannot_convert_takes_no_conversion \
  --skip tests::audio_at_another_rate_is_converted_as_the_engine_converts_it_whole \
  --skip tests::a_low_eq_band_filters_alike_at_every_context_rate
