#!/usr/bin/env bash
# Fails when committed schemas and generated types differ from `npm run generate:schema`.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fresh="$(mktemp -d)"
trap 'rm -rf "$fresh"' EXIT

cargo run --quiet --manifest-path "$root/Cargo.toml" -p alpha2-model \
  --features schema --example emit-schema -- "$fresh"
cargo run --quiet --manifest-path "$root/Cargo.toml" -p alpha2-compose \
  --features schema --example emit-compose-schema -- "$fresh"
cargo run --quiet --manifest-path "$root/Cargo.toml" -p alpha2-render \
  --example emit-effect-catalog -- "$fresh" "$fresh/builtin-effects.json"
cargo run --quiet --manifest-path "$root/Cargo.toml" -p alpha2-engine \
  --features schema --example emit-plugin-manifest-schema -- "$fresh"
status=0
for name in project.v1.schema.json project-limits.v1.json compose.v1.schema.json compose-core.v1.schema.json effects.v1.json plugin-manifest.v1.json; do
  if ! diff -u "$root/schema/$name" "$fresh/$name"; then
    echo "schema/$name is stale; run \`npm run generate:schema\` and commit the result" >&2
    status=1
  fi
done
if ! diff -u "$root/crates/alpha2-model/src/builtin-effects.json" "$fresh/builtin-effects.json"; then
  echo "crates/alpha2-model/src/builtin-effects.json is stale; run \`npm run generate:schema\`" >&2
  status=1
fi
node "$root/scripts/generate-project-types.mjs" --check || status=1
exit "$status"
