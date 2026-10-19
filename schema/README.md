# Project schema

Generated files; do not edit them by hand.

- `project.v1.schema.json`: the JSON Schema (draft 2020-12) of the project document, derived from `alpha2-model` with its `schema` feature.
- `project-limits.v1.json`: the document limits (`MAX_TRACKS` and the rest), which the TypeScript constants are generated from.

Regenerate both, and `packages/alpha2/src/generated/`, with

```sh
npm run generate:schema
```

`scripts/check-schema-fresh.sh` (run in CI) fails when any of them differs from what the model generates.

The schema describes shape only: JSON types, required fields, enumerated and tagged values, and the rejection of unknown fields. A document it rejects is never a valid project. One it accepts may still be invalid: ranges, limits, references between items, keyframe order, and transition coverage are checked by `Project::validate` and by `validateProject` in `alpha2/project`.
