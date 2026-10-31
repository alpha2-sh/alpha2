#!/usr/bin/env bash
# Builds, lints and tests each extension format feature alone; the workspace only runs all or none.
set -euo pipefail
cd "$(dirname "$0")/.."

for feature in svg animated-image image-sequence lottie exr; do
  echo "alpha2-extensions with only ${feature}"
  cargo clippy -p alpha2-extensions --no-default-features --features "$feature" --all-targets -- -D warnings
  cargo test -p alpha2-extensions --no-default-features --features "$feature" --no-fail-fast
done
