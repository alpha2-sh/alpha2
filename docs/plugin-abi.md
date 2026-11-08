# Plugin ABI

A plugin is a package an alpha2 host loads at run time: a manifest, one or more code artifacts and the resources they read. This page is `alpha2-plugin/1`, the ABI every artifact implements, as a core wasm module or as a C-ABI library. Plugins written in Rust use `plugins/sdk`, which builds both from one `Extractor` implementation. In C, C++, Zig or Go, implement [`plugins/sdk/include/alpha2_plugin.h`](../plugins/sdk/include/alpha2_plugin.h) directly.

A plugin of kind `extractor` provides one extractor: the host runs the extraction session, decodes the input and calls the plugin step by step, exactly as it drives an extractor written in the host's own language. `alpha2-host` runs one named in an `extract` request's `plugin` (see [`host-protocol.md`](host-protocol.md#plugins)), and Rust programs load one with `alpha2_host_core::plugins` (feature `plugins`). Every value the plugin emits is validated by the session, so a plugin cannot write invalid project data.

## Manifest

```json
{
  "id": "com.example.silence",
  "version": "1.0.0",
  "abi": "alpha2-plugin/1",
  "kind": "extractor",
  "artifacts": [
    { "target": "wasm32", "type": "wasm", "url": "silence.wasm", "sha256": "...", "bytes": 412000 },
    { "target": "x86_64-unknown-linux-gnu", "type": "cdylib", "url": "libsilence.so", "sha256": "...", "bytes": 980000 }
  ],
  "resources": [{ "name": "vad.onnx", "url": "https://example.com/vad.onnx", "sha256": "...", "bytes": 1800000 }],
  "descriptor": {
    "id": "com.example.silence",
    "version": 1,
    "input": { "media": "audio", "from": ["source"], "format": "f32", "sample_rate": 16000, "channels": "mono", "block_frames": 512 },
    "params_schema": { "type": "object" },
    "outputs": [{ "name": "silence", "kind": "intervals", "suggest": "remove_ranges" }]
  }
}
```

[`schema/plugin-manifest.v1.json`](../schema/plugin-manifest.v1.json) is its JSON Schema.

- `id` names both the plugin and its extractor, and equals `descriptor.id`. `version` is the package's semantic version; `descriptor.version` is the extractor's output-contract version. Ids starting `alpha2.` are kept for first-party plugins.
- `artifacts` has one entry per build: `target` is `wasm32` for a wasm module (`type` `wasm`), or the Rust target triple of a native one (`type` `cdylib`, or `staticlib` for a library an app links at build time). A loader takes the native artifact for its own target when the caller allows native code for this plugin, else the wasm artifact, else fails with `PLUGIN_NO_ARTIFACT`.
- `url` is relative to the manifest's own URL, or absolute. Every artifact and resource is pinned by `sha256` (64 lowercase hexadecimal digits) and `bytes`; a mismatch refuses the load.
- `descriptor` is exactly what the artifact's `describe` returns; a loader refuses an artifact whose descriptor differs.

## Trust

A wasm artifact is sandboxed: it imports only the functions below, with no files, network or clock, its memory is capped and its run time bounded. It is allowed once its hash matches. A loader refuses a module that uses relaxed SIMD, threads or shared memory, so the same module gives the same values on every runtime; it must not depend on the bit pattern of a NaN (emits refuse NaN anyway).

A native artifact runs with the full rights of the process and is refused unless the caller allows native code for that plugin id. It must not bring libraries that collide with the host's own (FFmpeg under its standard names). Native builds may differ across targets in floating-point results.

## Calls

Every call but `describe` returns a status: `0` succeeded, `1` failed (the plugin called `fail` with the reason first), `2` a panic was caught at the boundary. Integers and floats are little-endian; times are ticks (1/120000 s, signed 64-bit) on the input's clock.

| Call | Arguments | |
| --- | --- | --- |
| `describe` | | The descriptor as UTF-8 JSON, as in the manifest. |
| `begin` | context (UTF-8 JSON) | Once, first. Load models and resources here. |
| `pass` | `pass`, `direction` (0 forward, 1 backward) | At the start of each pass. |
| `video` | `time`, `width`, `height`, `format` (0 `rgba8`, 1 `luma8`), pixels | A frame: rows top first, tightly packed; `rgba8` is straight alpha in display-encoded sRGB, `luma8` the BT.709 luma of it. |
| `audio` | `start`, `start_sample`, `frames`, `rate`, `channels`, samples | A block of `frames` x `channels` 32-bit floats, planar, channel after channel. `start` is the tick at or before the first sample, `start_sample` its index at `rate` from the clock's zero. |
| `end` | | Once, last. Whole-range work emits here. |

The context `begin` receives:

```json
{
  "range": { "start": 0, "end": 480000 },
  "passes": [{ "start": 0, "direction": "forward" }],
  "from": "source",
  "video": { "size": 640, "format": "luma8" },
  "audio": null,
  "frame_rate": null,
  "params": { "threshold": 0.5 }
}
```

`video` (`size`, the delivered frame's longer side; `format`) is set for a video extractor, `audio` (`rate`, `channels` as `mono` or `source`, `block_frames`) for an audio one, and `frame_rate` (`num`, `den`) for timeline video. `params` is the request's, checked against the descriptor's `params_schema` by the caller.

The host gives the plugin these functions:

| Function | | |
| --- | --- | --- |
| `emit` | records | A batch of [emit records](#emit-records). Returns `0`, or `1` once the session has refused an emit; the plugin then returns `1` from the call it is in. |
| `resource` | name | The bytes of a resource the manifest declares. |
| `fail` | message | Why the call about to return `1` failed (UTF-8). |
| `log` | level (0 debug, 1 info, 2 warn), message | A diagnostic (UTF-8). |

Within a pass, a value is emitted no further along the pass than what the pass has delivered; `end` may emit anywhere in the range. The session's other rules (finite values in range, one sample per time, words and intervals inside the range) are those of the extraction contract.

## Native artifacts

A native artifact exports one symbol, `alpha2_plugin_v1`, a C function without arguments that returns a pointer to a static `Alpha2PluginV1` table (the header has the exact types). The table and the descriptor's bytes stay valid while the library is loaded.

- `create(host)` returns a new instance, or null. The host's `Alpha2HostV1` table stays valid until `destroy`, and every host function takes the table's `context` first. One run uses one instance; the host calls it from one thread at a time.
- Every other call takes the instance first. Pixels, samples, the context and the strings passed to the plugin are borrowed for the call. Records passed to `emit` are borrowed for that call alone.
- `resource(context, name, name_len, &len)` returns the resource's bytes, or null for a name the manifest does not declare. They stay valid until `destroy`.
- A panic must not cross the boundary: catch it and return `2`. The SDK does.
- A later ABI adds another symbol (`alpha2_plugin_v2`), so a library can export several.

## wasm artifacts

A wasm artifact is a core module (not a component) that exports `memory` and these functions. Pointers and lengths are `i32` offsets and byte counts in that memory.

| Export | Signature | |
| --- | --- | --- |
| `alpha2_abi` | `() -> i32` | `1`. |
| `alpha2_alloc` | `(size: i32, align: i32) -> i32` | Memory for the host to write a call's arguments into. |
| `alpha2_free` | `(ptr: i32, size: i32, align: i32)` | Frees what `alpha2_alloc` returned. |
| `alpha2_describe` | `() -> i64` | The descriptor JSON: its offset in the low 32 bits, its length in the high 32. |
| `alpha2_begin` | `(ptr: i32, len: i32) -> i32` | |
| `alpha2_pass` | `(pass: i32, direction: i32) -> i32` | |
| `alpha2_video` | `(time: i64, width: i32, height: i32, format: i32, ptr: i32, len: i32) -> i32` | |
| `alpha2_audio` | `(start: i64, start_sample: i64, frames: i32, rate: i32, channels: i32, ptr: i32, count: i32) -> i32` | `count` samples, 4 bytes each, 4-byte aligned. |
| `alpha2_end` | `() -> i32` | |

The host writes each call's input into memory it allocated with `alpha2_alloc`, makes the call and frees it afterwards; the plugin must not keep the pointer. One module instance serves one run.

The host functions are imports from module `alpha2`:

| Import | Signature | |
| --- | --- | --- |
| `emit` | `(ptr: i32, len: i32) -> i32` | |
| `resource_size` | `(name: i32, name_len: i32) -> i64` | The resource's length, or `-1` for a name the manifest does not declare. |
| `resource_read` | `(name: i32, name_len: i32, dst: i32, dst_len: i32) -> i32` | Copies the resource into `dst`; `0` when it was copied whole. |
| `fail` | `(ptr: i32, len: i32)` | |
| `log` | `(level: i32, ptr: i32, len: i32)` | |

A trap ends the run with `PLUGIN_TRAPPED`; running past the memory cap or the time limit with `PLUGIN_LIMIT`.

## Emit records

`emit` takes records back to back. Each is an 8-byte header and a body; nothing is padded or aligned.

| Header | |
| --- | --- |
| `u8` | tag: `1` sample, `2` word, `3` interval |
| `u8` | `0` |
| `u16` | the output's index in the descriptor's `outputs` |
| `u32` | the body's length in bytes |

| Body | Fields |
| --- | --- |
| sample | `i64` time, `u32` count, `ceil(count / 32)` `u32` lost masks, `count` `f32` values |
| word | `i64` start, `i64` end, `u32` text length, `u32` speaker length, text, speaker |
| interval | `i64` start, `i64` end, `u32` flags, `f32` score, `u32` label length, label |

- A sample carries one value per channel component, as the output's kind has them: two per channel for `points`, one per channel for `scalars`, four for `similarity`. Bit `i % 32` of mask `i / 32` set marks value `i` lost at that time (its float is ignored); bits past `count` are `0`.
- Texts are UTF-8. A speaker or label length of `0xFFFFFFFF` means it is absent; a word's text is never absent. An interval has a score when bit 0 of its flags is set; the other flag bits are `0`.
- A batch with an unknown tag, a nonzero reserved byte, a body shorter or longer than its fields, or text that is not UTF-8 is refused whole, as an invalid emit.

## Versions

`alpha2-plugin/1` changes only by additions a `/1` plugin cannot notice: new host imports a plugin may use when the host offers them, and new record tags only plugins that know them emit. A change an existing plugin would notice is `alpha2-plugin/2`, with its own manifest `abi`, wasm `alpha2_abi` value and native symbol. A loader refuses an ABI it does not implement with `PLUGIN_ABI_UNSUPPORTED`.

## Errors

The same codes are used on every surface: `PLUGIN_FETCH_FAILED`, `PLUGIN_HASH_MISMATCH`, `PLUGIN_ABI_UNSUPPORTED`, `PLUGIN_NO_ARTIFACT` (with the targets the manifest offers), `PLUGIN_NATIVE_REFUSED`, `PLUGIN_DESCRIPTOR_MISMATCH`, `PLUGIN_UNSUPPORTED_FEATURE`, `PLUGIN_LOAD_FAILED`, `PLUGIN_TRAPPED` and `PLUGIN_LIMIT`. An emit the session refuses is `INVALID_EMIT`, and a call that returns `1` is `EXTRACTOR_FAILED` with the plugin's message, as for any extractor.
