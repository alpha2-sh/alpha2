# Schemas

Generated files; do not edit them by hand.

- `project.v1.schema.json`: the JSON Schema (draft 2020-12) of the project document, derived from `alpha2-model` with its `schema` feature.
- `project-limits.v1.json`: the document limits (`MAX_TRACKS` and the rest), which the TypeScript constants are generated from.
- `plugin-manifest.v1.json`: the JSON Schema of a plugin manifest, derived from `alpha2-engine` with its `schema` feature. `PluginManifest::from_json` checks the rest: the id against the descriptor's, the semantic version, the descriptor's own rules, repeated targets or resource names, the slot count, pinned names under a directory slot and the pinned total.
- `compose.v1.schema.json`: the JSON Schema of the compose spec, derived from `alpha2-compose` with its `schema` feature. The model types it embeds are stated as in the project schema; the Python package's `compose_types.py` is generated from it.
- `compose-core.v1.schema.json`: the compose spec without its engine-level nodes (`raw`, `layout`), self-contained, for whoever writes specs (a person or a model) rather than a program that needs every node.
- `effects.v1.json`: the catalog of built-in effects and transitions (id, version, kind, inputs, resources, and each parameter's type, range, step and default), from `alpha2-render`'s `builtin_catalog`. `alpha2-model` validates effect invocations against a copy of it (`crates/alpha2-model/src/builtin-effects.json`), and `packages/alpha2/src/generated/builtin-effects.ts` is generated from it.

Regenerate them, `packages/alpha2/src/generated/`, and the Python package's generated types with

```sh
npm run generate:schema
```

`scripts/check-schema-fresh.sh` (run in CI) fails when any of them differs from what the Rust crates generate.

The project schema describes shape: JSON types, required fields, enumerated and tagged values, the rejection of unknown fields, and the count limit of every list (`maxItems`, and `maxProperties` for an effect node's `inputs`). A document it rejects is never a valid project. One it accepts may still be invalid: value ranges, references between items, keyframe order, and transition coverage are checked by `Project::validate` and by `validateProject` in `alpha2/project`.
