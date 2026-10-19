#!/bin/sh
# Prints `rustc -vV` of the compiler cargo uses in this checkout, and fails unless it is the release
# rust-toolchain.toml pins: a release started elsewhere or under RUSTUP_TOOLCHAIN would otherwise
# build on whatever compiler the machine has. The release scripts run this before they build.
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$workspace"

pin=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
if [ -z "$pin" ]; then
  echo "rust-toolchain.toml pins no channel" >&2
  exit 1
fi
info=$(rustc -vV) || {
  echo "rustc -vV failed; install rustup, which installs the pinned Rust $pin" >&2
  exit 1
}
release=$(printf '%s\n' "$info" | sed -n 's/^release: //p')
if [ "$release" != "$pin" ]; then
  echo "rustc ${release:-of unknown release} is not the pinned Rust $pin (rust-toolchain.toml)${RUSTUP_TOOLCHAIN:+; RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN overrides it}" >&2
  exit 1
fi
printf '%s\n' "$info"
