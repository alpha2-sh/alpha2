#!/usr/bin/env bash
# The native crates' hardware and host features build alone as well as together: the workspace
# lints only every feature or none, so a helper gated on the wrong feature would slip through.
# `hwdevice` underlies `hwdecode` and `hwencode` and is not enabled alone. The core crates'
# `wgpu-interop` is linted off and on, since the workspace never builds them without it.
set -euo pipefail
cd "$(dirname "$0")/.."

lint() {
  echo "$1 with only $2"
  cargo clippy -p "$1" --no-default-features --features "$2" --all-targets -- -D warnings
}

for feature in hwdecode hwencode extensions; do
  lint alpha2-host "$feature"
done
for feature in hwdecode hwencode timeline cpal-host; do
  lint alpha2-native "$feature"
done
lint alpha2-native-gpu hwdevice
for crate in alpha2-gpu alpha2-render alpha2-engine alpha2; do
  echo "$crate without wgpu-interop"
  cargo clippy -p "$crate" --all-targets -- -D warnings
  lint "$crate" wgpu-interop
done
echo "alpha2-facade-without-wgpu alone"
cargo clippy -p alpha2-facade-without-wgpu --all-targets -- -D warnings
