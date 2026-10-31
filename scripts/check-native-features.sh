#!/usr/bin/env bash
# Lints each native hardware/host feature alone, since the workspace lints only all or none.
# `hwdevice` is not enabled alone; core crates are linted with and without `wgpu-interop`.
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
