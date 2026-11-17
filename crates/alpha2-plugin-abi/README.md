# alpha2-plugin-abi

The `alpha2-plugin/1` ABI shared by plugins and the hosts that load them: the native function
tables, the wasm import and export names, and the binary emit records. `docs/plugin-abi.md` in the
repository specifies it; plugins written in Rust use `plugins/sdk` instead of this crate directly.

Part of [alpha2](https://github.com/alpha2-sh/alpha2), a media engine that renders timeline projects on the GPU.
