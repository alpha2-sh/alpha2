#!/bin/sh
# Packs every plugin crate (a directory with descriptor.json) under plugins/first-party, and with
# --fixtures under plugins/fixtures, into <directory>/<group>/<name>. Artifacts are built in
# plugins/target, so tests that pack a plugin again find it built.
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
for group in $groups; do
  for descriptor in "$workspace/plugins/$group"/*/descriptor.json; do
    [ -f "$descriptor" ] || continue
    plugin=$(dirname -- "$descriptor")
    cargo run --release --locked --manifest-path "$workspace/plugins/Cargo.toml" \
      -p alpha2-plugin-pack -- "$plugin" --out "$out/$group/$(basename -- "$plugin")" "$@"
  done
done
