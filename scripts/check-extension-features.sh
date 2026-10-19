#!/usr/bin/env bash
# Each extension format is a cargo feature, and a build with any one of them must build, lint and
# pass that format's tests. The workspace runs only every feature or none.
set -euo pipefail
cd "$(dirname "$0")/.."

for feature in svg animated-image image-sequence lottie exr; do
  echo "alpha2-extensions with only ${feature}"
  cargo clippy -p alpha2-extensions --no-default-features --features "$feature" --all-targets -- -D warnings
  cargo test -p alpha2-extensions --no-default-features --features "$feature" --no-fail-fast
done
