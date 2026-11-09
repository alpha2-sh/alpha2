# Plugins

Plugins an alpha2 host loads at run time, and the SDK to write them. This is a cargo workspace of its own, with its own lock; nothing in it is published or released from the repository, and no release archive or package contains a plugin.

- `sdk`: `alpha2-plugin-sdk`. Implement `Extractor` and call `export_extractor!` once; the crate builds as a core wasm module for `wasm32-unknown-unknown` and as a C-ABI library (`crate-type = ["cdylib"]`) for every other target. `include/alpha2_plugin.h` is the same ABI for plugins written in C, C++, Zig or Go. [`docs/plugin-abi.md`](../docs/plugin-abi.md) specifies it.
- `pack`: builds a plugin and writes a directory ready to upload to any server: the artifacts, the resources and `manifest.json` with relative URLs, SHA-256 hashes and sizes. It builds wasm without relaxed SIMD and with getrandom's `unsupported` backend (a plugin has no randomness), and refuses a module that uses relaxed SIMD, threads or shared memory, or imports anything but the `alpha2` host functions.
- `first-party/`: the plugins meant to be published, one crate each.
  - `silence`: `alpha2.silence`, the intervals without speech, from Silero VAD (`model/` says where the model comes from) run through tract, with a `score` each. Its tests use a LibriVox recording with silences inserted at known places.
- `fixtures/`: plugins for tests only, never published.
  - `digest`, `scripted`: the engine's `alpha2.test.digest` and `alpha2.test.scripted` extractors as plugins, for loader and surface tests.

```sh
cargo run --manifest-path plugins/Cargo.toml -p alpha2-plugin-pack -- \
  plugins/fixtures/digest --out out/digest --target x86_64-unknown-linux-gnu
```

`scripts/pack-plugins.sh <directory>` packs every crate under `first-party/` (`--fixtures` adds `fixtures/`) into `<directory>/<group>/<name>`. A plugin crate keeps its descriptor in `descriptor.json` at its root (the SDK's `Extractor::DESCRIPTOR` includes it), files it reads through `resource()` under `resources/`, and resources hosted elsewhere in `resources.json` (`name`, `url`, `sha256`, `bytes`). `--target` adds a native artifact for that triple (the host's when none is given; `--no-native` builds wasm only); building for another platform needs its linker.
