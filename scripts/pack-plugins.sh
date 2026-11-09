#!/bin/sh
# Packs the plugins under plugins/ into <directory>, building their artifacts in plugins/target so
# the tests that pack them again find them built.
#   pack-plugins.sh <directory> [<pack option>...]
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=$1
shift
for plugin in fixture-digest fixture-scripted silence; do
  cargo run --release --locked --manifest-path "$workspace/plugins/Cargo.toml" \
    -p alpha2-plugin-pack -- "$workspace/plugins/$plugin" --out "$out/$plugin" "$@"
done
