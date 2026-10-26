# Schemas

Generated files; do not edit them by hand.

- `project.v1.schema.json`: the JSON Schema (draft 2020-12) of the project document, derived from `alpha2-model` with its `schema` feature.
- `project-limits.v1.json`: the document limits (`MAX_TRACKS` and the rest), which the TypeScript constants are generated from.
- `compose.v1.schema.json`: the JSON Schema of the compose spec, derived from `alpha2-compose` with its `schema` feature. The model types it embeds are stated as in the project schema; the Python package's `compose_types.py` is generated from it.

Regenerate them, `packages/alpha2/src/generated/`, and the Python package's generated types with

```sh
npm run generate:schema
```

`scripts/check-schema-fresh.sh` (run in CI) fails when any of them differs from what the Rust crates generate.

The schema describes shape only: JSON types, required fields, enumerated and tagged values, and the rejection of unknown fields. A document it rejects is never a valid project. One it accepts may still be invalid: ranges, limits, references between items, keyframe order, and transition coverage are checked by `Project::validate` and by `validateProject` in `alpha2/project`.
