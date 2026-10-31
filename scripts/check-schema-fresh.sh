#!/usr/bin/env bash
# Check committed schemas, limits and generated types against the Rust model and compose spec.
# Regenerate with `npm run generate:schema`; hand-edited output also fails this check.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fresh="$(mktemp -d)"
trap 'rm -rf "$fresh"' EXIT

cargo run --quiet --manifest-path "$root/Cargo.toml" -p alpha2-model \
  --features schema --example emit-schema -- "$fresh"
cargo run --quiet --manifest-path "$root/Cargo.toml" -p alpha2-compose \
  --features schema --example emit-compose-schema -- "$fresh"
status=0
for name in project.v1.schema.json project-limits.v1.json compose.v1.schema.json; do
  if ! diff -u "$root/schema/$name" "$fresh/$name"; then
    echo "schema/$name is stale; run \`npm run generate:schema\` and commit the result" >&2
    status=1
  fi
done
node "$root/scripts/generate-project-types.mjs" --check || status=1
exit "$status"
