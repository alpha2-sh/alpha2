#!/bin/sh
# Packs every plugin crate (a directory with descriptor.json) under plugins/first-party, and with
# --fixtures under plugins/fixtures, into <directory>/<group>/<name>; with --no-native, the crates
# that build no wasm are left out. Artifacts are built in plugins/target, so tests that pack a
# plugin again find it built.
#   pack-plugins.sh [--fixtures] <directory> [<pack option>...]
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
for group in $groups; do
  for descriptor in "$workspace/plugins/$group"/*/descriptor.json; do
    [ -f "$descriptor" ] || continue
    plugin=$(dirname -- "$descriptor")
    if [ "$wasm_only" = true ] && awk '/^\[/ { table = $0 }
      table == "[package.metadata.alpha2-plugin]" && /^wasm *= *false/ { found = 1 }
      END { exit !found }' "$plugin/Cargo.toml"; then
      echo "$plugin builds no wasm; left out"
      continue
    fi
    cargo run --release --locked --manifest-path "$workspace/plugins/Cargo.toml" \
      -p alpha2-plugin-pack -- "$plugin" --out "$out/$group/$(basename -- "$plugin")" "$@"
  done
done
