# Plugin ABI

A plugin is a package an alpha2 host loads at run time: a manifest, one or more code artifacts and the resources they read. This page is `alpha2-plugin/1`, the ABI every artifact implements, as a core wasm module or as a C-ABI library. Plugins written in Rust use `plugins/sdk`, which builds both from one `Extractor` implementation. The SDK is not published to crates.io, so a Rust plugin crate lives in a checkout of this repository, as a member of the `plugins` workspace or with a path dependency on `plugins/sdk`. In C, C++, Zig or Go, implement [`plugins/sdk/include/alpha2_plugin.h`](../plugins/sdk/include/alpha2_plugin.h) directly.

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
  "resources": [
    { "name": "vad.onnx", "url": "https://example.com/vad.onnx", "sha256": "...", "bytes": 1800000 },
    { "name": "model", "supplied": { "shape": "directory", "required": false, "accept": [".onnx"] } }
  ],
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
- `artifacts` has one entry per build: `target` is `wasm32` for a wasm module (`type` `wasm`), or the Rust target triple of a native one (`type` `cdylib`, or `staticlib` for a library an app links at build time). A loader takes the native artifact for its own target when native code is allowed (for `alpha2-host`: the operator started it with `--plugins native` and the request sets `allow_native`), else the wasm artifact, else fails with `PLUGIN_NO_ARTIFACT`.
- `url` is a path under the manifest's own directory (`/`-separated, never climbing above it with `..`) or an absolute `https:` URL; a manifest with any other `url` is refused with `PLUGIN_LOAD_FAILED`. Every artifact and resource is pinned by `sha256` (64 lowercase hexadecimal digits) and `bytes`; a mismatch refuses the load.
- A resource is either pinned (`url`, `sha256`, `bytes`) or a slot (`supplied`) the caller fills at load time with a file or a directory it names, such as a model the user already has. A slot's `shape` is `file` or `directory`; `required` (default `true`) fails a load without it, and an optional slot left empty reads as an undeclared name, so a plugin can fall back to a pinned default; `max_bytes` is the largest single file the plugin takes from it (a `file` slot is checked at load, a file of a `directory` slot when it is first read); `accept` lists file name suffixes for pickers (`.onnx`, `.gguf`), for interfaces only. A slot name is 1 to 64 of `A-Z a-z 0-9 . _ -`, not starting with `.`. A manifest has at most 16 slots within its 64 resources; the 4 GiB total counts pinned resources only, and no pinned name lies under a directory slot's names (`<slot>/...`).
- `descriptor` is exactly what the artifact's `describe` returns; a loader refuses an artifact whose descriptor differs.

## Trust

A wasm artifact is sandboxed: it imports only the functions below, with no files, network or clock, and its run time is bounded. It has one memory, capped by the host and never larger than 2 GiB, and its tables together hold at most 1,048,576 elements. It is allowed once its hash matches. A loader refuses a module that uses relaxed SIMD, threads, shared memory, more than one memory or 64-bit memory, so the same module gives the same values on every runtime; it must not depend on the bit pattern of a NaN (emits refuse NaN anyway).

A native artifact runs with the full rights of the user and is refused unless native code is allowed: in `alpha2-host` the operator allowed it at start (`--plugins native`) and the request asks for it (`allow_native`); a program that loads plugins itself decides alone. It must not bring libraries that collide with the host's own (FFmpeg under its standard names). Native builds may differ across targets in floating-point results.

`alpha2-host` runs a native artifact in a worker process of its own, `alpha2-host plugin-worker`, one per run (unless the operator starts it with `--plugin-isolation in-process`). A crash, an abort or a hang of the plugin, or of a runtime it links, fails that run with `PLUGIN_TRAPPED` or `PLUGIN_LIMIT` and leaves the host serving; each call ends at the call deadline and the cancel kills the worker at once, with every process the plugin started. In the worker, standard input reads nothing and what the plugin prints to standard output goes to standard error, which the host writes as plugin diagnostics under the log budget. The host checks what the worker returns as if the plugin ran inside it, so a native plugin gets the same codes either way. A Rust program that loads plugins isolates them the same way with `alpha2_host_core::plugins::Isolation::Child`, naming a subcommand of its own that calls `alpha2_host_core::plugins::worker::main()`. The worker's memory is not limited, and on Linux it is the out-of-memory killer's first choice: a worker killed for memory fails its run as a crash.

Files supplied for slots are trusted less than pinned ones: the loader records what a run read by name, SHA-256 and length in `made_by.plugin.supplied`, and the manifest's own SHA-256 in `made_by.plugin.manifest_sha256`, but it cannot tell which files a runtime opens by itself, and a file a native plugin opens by its path after it was hashed is read as it is then. `alpha2-host` takes supplied files only within `--plugin-resource-root`.

## Mobile apps

The App Store and Google Play forbid downloading executable code, so an app bundles each plugin it runs: the manifest, the artifacts and the resources, at the same relative paths the manifest names (`pack` writes that layout). With the library's `plugins` feature, `Plugin.load(manifestPath, sha256, allowNative, supplied)` reads them from the bundle, checks every hash, and `Alpha2.extractPlugin(request, plugin)` runs the plugin like an extractor the app wrote, in the app's process. `supplied` fills the manifest's slots with paths inside the app's sandbox, such as a model the user picked (on iOS, start access to a security-scoped URL before the extraction and stop it after; on Android, copy a `content:` document into the app's storage first). A bundled manifest's `https:` URLs are refused with `PLUGIN_FETCH_FAILED`.

- The wasm artifact runs everywhere, sandboxed: wasmtime compiles it on Android; on iOS, where apps may not generate code, it runs in wasmtime's interpreter.
- An Android app may bundle the `.so` built for its ABI (`aarch64-linux-android`) and pass `allowNative`; it runs with the app's rights.

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
  "params": { "threshold": 0.5 },
  "supplied": {
    "model": {
      "shape": "directory",
      "files": [
        { "name": "model/encoder_model.onnx", "bytes": 33000000 },
        { "name": "model/tokenizer.json", "bytes": 2400000 }
      ]
    }
  }
}
```

`video` (`size`, the delivered frame's longer side; `format`) is set for a video extractor, `audio` (`rate`, `channels` as `mono` or `source`, `block_frames`) for an audio one, and `frame_rate` (`num`, `den`) for timeline video. `params` is the request's. `supplied` lists the filled slots, sorted, with the files each offers and their sizes (`{}` when none is filled, and always in the browser, which fills no slot): a `file` slot is the resource named as the slot, a `directory` slot one resource per regular file under it, named `<slot>/<relative path>` with `/` separators, found through links (a Hugging Face snapshot links into `blobs/`) and skipping entries whose name starts with `.`. A plugin checks in `begin` that the files fit it (names, a model's inputs and outputs, a vocabulary) and fails with a message naming what does not match. The browser's `runExtractor` and the Node and Python clients check a request's `params` against the descriptor's `params_schema` before an extractor in the caller's own process runs, but no surface checks the params a plugin receives (in the browser, those given to `plugin.extractor(params)`), so a plugin must check what it reads.

The host gives the plugin these functions:

| Function | | |
| --- | --- | --- |
| `emit` | records | A batch of [emit records](#emit-records). Returns `0`, or `1` once the session has refused an emit; the plugin then returns `1` from the call it is in. |
| `resource` | name | The bytes of a resource the manifest declares, pinned or supplied. A supplied file is read and hashed the first time it is asked for. |
| `resource_path` | name | Native only: the absolute path of a supplied file, a filled directory slot or a directory under one; see [Native artifacts](#native-artifacts). |
| `fail` | message | Why the call about to return `1` failed (UTF-8); loaders keep the first 4096 bytes. |
| `log` | level (0 debug, 1 info, 2 warn), message | A diagnostic (UTF-8); loaders write the first 2048 bytes of a line and 64 KiB per run, then one notice that further lines are dropped. |

A read that fails (a pinned file that changed after the load checked it, `PLUGIN_HASH_MISMATCH`; a supplied file that changed while the plugin ran, `PLUGIN_FETCH_FAILED`; a file of a directory slot past `max_bytes`, `PLUGIN_RESOURCE_INVALID`; the call's deadline or a cancel) gives the plugin nothing and ends the call it happened in with that failure, whatever the plugin returns. Reading and hashing count against the call's deadline: a plugin that reads gigabytes does so in `begin` or as its input arrives, so no call outruns the deadline.

Within a pass, a value is emitted no further along the pass than what the pass has delivered; `end` may emit anywhere in the range. The session's other rules (finite values in range, one sample per time, words and intervals inside the range) are those of the extraction contract.

## Native artifacts

A native artifact exports one symbol, `alpha2_plugin_v1`, a C function without arguments that returns a pointer to a static `Alpha2PluginV1` table (the header has the exact types). The table and the descriptor's bytes stay valid while the library is loaded. A loader loads it from its own copy of the checked bytes. In a worker process (above) the library lives for one run; in the loader's own process it is loaded once per process and never unloaded: `destroy` ends an instance, not the library, so a plugin keeps no per-run state in statics, and threads it starts may outlive the run. Both tables start with `abi` and `size`, the bytes of the table as the side that filled it knows it. A host refuses a plugin table that is shorter than its own or has a null entry with `PLUGIN_LOAD_FAILED`; every entry is required.

- `create(host)` returns a new instance, or null. The host's `Alpha2HostV1` table stays valid until `destroy`, and every host function takes the table's `context` first. One run uses one instance; the host calls it from one thread at a time. A plugin calls the host functions only during a call the host made, on that call's thread.
- Every other call takes the instance first. Pixels, samples, the context and the strings passed to the plugin are borrowed for the call and read-only: in a worker process a native plugin's video frames lie in memory the worker maps read-only, so a write into them fails the run with `PLUGIN_TRAPPED`. Records passed to `emit` are borrowed for that call alone.
- `resource(context, name, name_len, &len)` returns the resource's bytes, or null for a name the manifest does not declare (and for an empty optional slot, or after a failed read). They stay valid until `destroy`.
- `resource_path(context, name, name_len, &len)`, appended to `Alpha2HostV1` within `/1`, returns the absolute path (UTF-8) of a supplied file, a filled directory slot or a directory under one, valid until `destroy`, for runtimes that open or map a model themselves (onnxruntime, whisper.cpp, llama.cpp style loaders). The path is the canonical path of what the caller named for the slot, then the name below it as enumerated, with links below the slot not resolved, so a runtime finds the files beside its model (external tensor data, a split model, an encoder directory) by name; on Windows it has no `\\?\` prefix where a plain form exists. It is null for a pinned resource (reading it after the pin was checked would undo the pin), an empty optional slot, an undeclared name, or a path that is not UTF-8. A file is hashed the first time its path is handed out, and every file under a directory the first time the directory's is; a later change is not seen, and entries whose name starts with `.` are neither offered nor recorded, though a runtime given the directory can open them. Ask for every file a runtime opens, or for the directory that holds them, since only those are recorded. On Windows convert the path to UTF-16 for a C runtime whose narrow API reads the ANSI code page. Check `ALPHA2_HOST_HAS(host, resource_path)` before calling it: an older host's table is shorter, and the SDK's `Host::resource_path` is `None` there and in wasm.
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
| `resource_size` | `(name: i32, name_len: i32) -> i64` | The resource's length, or `-1` for a name the manifest does not declare, an empty optional slot, or a supplied file past 2 GiB. |
| `resource_read` | `(name: i32, name_len: i32, dst: i32, dst_len: i32) -> i32` | Streams the resource into `dst`, checking it as it is read; `0` when it was copied whole. |
| `fail` | `(ptr: i32, len: i32)` | |
| `log` | `(level: i32, ptr: i32, len: i32)` | |

A trap ends the run with `PLUGIN_TRAPPED`; running past the memory cap, the table bound or the time limit with `PLUGIN_LIMIT`.

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
- A batch with an unknown tag, a nonzero reserved byte, a body shorter or longer than its fields, text that is not UTF-8, a sample of more than 256 values or a text of more than 256 bytes is refused whole, as an invalid emit. So is a batch that takes an output past the items one data set holds (`MAX_DATA_VALUES` values, `MAX_DATA_WORDS` words, `MAX_DATA_INTERVALS` intervals), checked as it is emitted. A wasm plugin that calls `emit` again in the call after a refusal traps, and the run ends with `INVALID_EMIT`.

## Versions

`alpha2-plugin/1` changes only by additions a `/1` plugin cannot notice. The native tables grow by fields appended at their end, and each side reads the other's `size` before it uses a field added after the first `/1`: a plugin refuses a host table shorter than the first `/1`'s (`ALPHA2_HOST_V1_BASE_SIZE`, `alpha2_plugin_abi::HOST_V1_BASE_BYTES`) and uses an appended entry only when `size` covers it. `resource_path` is the first such field. A wasm module has no such query: one built against a later `/1` imports the new host function and loads only on a host that has it; an older host refuses it with `PLUGIN_UNSUPPORTED_FEATURE`, naming the import. New record tags are emitted only by plugins that know them. A change an existing plugin would notice is `alpha2-plugin/2`, with its own manifest `abi`, wasm `alpha2_abi` value and native symbol. A loader refuses an ABI it does not implement with `PLUGIN_ABI_UNSUPPORTED`.

## Errors

The same codes are used on every surface: `PLUGIN_FETCH_FAILED`, `PLUGIN_HASH_MISMATCH`, `PLUGIN_ABI_UNSUPPORTED`, `PLUGIN_NO_ARTIFACT` (with the targets the manifest offers), `PLUGIN_NATIVE_REFUSED`, `PLUGIN_REFUSED` (the host's plugin policy), `PLUGIN_DESCRIPTOR_MISMATCH`, `PLUGIN_UNSUPPORTED_FEATURE`, `PLUGIN_LOAD_FAILED`, `PLUGIN_TRAPPED` (also a native worker that ended without its reply, with `details.exit` or `details.signal`, or broke the protocol, `details.wire`), `PLUGIN_LIMIT` (also a native call in a worker past its deadline) and `PLUGIN_RESOURCE_INVALID`, a file supplied for a slot that does not fit it, with `details` `{ "slot", "reason" }`: `missing`, `unknown_slot`, `shape`, `too_large`, `too_many_files` (more than 1,024 in a directory slot), `too_deep` (deeper than 8, or a directory that is its own ancestor), `name_too_long` (a resource name over 512 bytes), `not_utf8` or `unsupported` (a loader that fills no slots, such as the browser's). `PLUGIN_REFUSED` with `details.policy` `resource_root` is a supplied file the caller's policy does not allow. An emit the session refuses is `INVALID_EMIT`, and a call that returns `1` is `EXTRACTOR_FAILED` with the plugin's message, as for any extractor.
