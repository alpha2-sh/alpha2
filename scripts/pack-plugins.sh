#!/bin/sh
# usage: pack-plugins.sh [--fixtures] <directory> [<pack option>...]
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
groups=first-party
if [ "${1:-}" = --fixtures ]; then
  groups="first-party fixtures"
  shift
fi
out=$1
shift
wasm_only=false
for option in "$@"; do
  case $option in --no-native) wasm_only=true ;; esac
done
pack() {
  cargo run --release --locked --manifest-path "$workspace/plugins/Cargo.toml" \
    -p alpha2-plugin-pack "$@"
}
for group in $groups; do
  for descriptor in "$workspace/plugins/$group"/*/descriptor.json; do
    [ -f "$descriptor" ] || continue
    plugin=$(dirname -- "$descriptor")
    if [ "$wasm_only" = true ] && [ "$(pack --quiet -- builds-wasm "$plugin")" = false ]; then
      echo "$plugin builds no wasm; left out"
      continue
    fi
    pack -- "$plugin" --out "$out/$group/$(basename -- "$plugin")" "$@"
  done
done
