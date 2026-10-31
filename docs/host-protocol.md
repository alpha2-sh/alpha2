# Host protocol

`alpha2-host serve` takes requests on standard input and answers on standard output. A program in any language can start it and export projects through it; `@alpha2-sh/node` is a client of this protocol, and Node.js programs use that instead. This page is version 1 of the protocol. [`native.md`](native.md) says where `alpha2-host` comes from.

## Messages

Every message, both ways, is one JSON object on one line of UTF-8 text, and has `"protocolVersion": 1`. What the host writes is ASCII: every character past ASCII in a string is a `\uXXXX` escape (two for one outside the Basic Multilingual Plane), so a reader that decodes it in a legacy code page reads the same text.

The first line the host writes, before it reads any input, is `{"protocolVersion":1,"type":"ready"}`. A host that writes nothing for long is held before it runs (a Gatekeeper check on macOS, an antivirus scan or an App Control policy on Windows, a stopped process); `@alpha2-sh/node` and the Python client end it after 30 seconds and fail with `HOST_START_TIMEOUT`. A host that dies of `SIGKILL` while its cgroup's `oom_kill` count (cgroup v2 `memory.events`, Linux) rises fails with `HOST_OOM_KILLED` in both clients.

A request has a `requestId`, a string that is not empty, and an `operation`: `capabilities`, `conform`, `export`, `still` or `cancel`.

```json
{"protocolVersion":1,"requestId":"1","operation":"capabilities"}
```

The host answers each request with one response that carries its `requestId`:

```json
{"protocolVersion":1,"requestId":"1","ok":true,"result":{...}}
{"protocolVersion":1,"requestId":"1","ok":false,"error":{"code":"INVALID_INPUT","message":"..."}}
```

- Requests can be written before earlier ones are answered. Match each response to its request by `requestId`.
- A request the host cannot read is answered with `"requestId": "unknown"` when it has no usable `requestId`.
- A request line is at most 128 MiB, and the `project` in it at most 64 MiB.
- While an export runs, the host also writes progress lines: `{"protocolVersion":1,"type":"progress","requestId":"...","phase":"frames","completed":12,"total":30}`. `phase` is `frames` while frames are encoded and `finalizing` once every frame is and the file is being completed. A line with `"restarted": true` and `completed` 0 says the export started over on the readback path, because the GPU-resident encoder rejected a frame on a device that was still usable; the result's `gpuOutputPathFallback` names the encoder, the frame and the reason. The export is finished only when its response arrives.
- When the host cannot start its GPU device (or, in `hardware-required` decode mode, its decode device) for the first request that needs it, it writes `{"protocolVersion":1,"type":"fatal","error":{...}}` and exits; that request and the ones after it get no response. `conform` needs no GPU.
- When the host's device is lost or a decoder or encoder thread is stuck, the host exits after its response; the requests after it get no response. Start a new process for them.
- The end of standard input means no more requests: the host answers every request it has read, then exits. A client that may cancel keeps standard input open until it has its answer.

Standard error carries human-readable diagnostics, not protocol messages.

## `capabilities`

Takes no other fields. The result reports what this host can do: `status: "ready"`, `capabilities` (the GPU and its tier, with `gpu.memory.budgetBytes`, the device memory budget the adapter's graphics API reports (DXGI's local segment budget, Vulkan's `VK_EXT_memory_budget` device-local heaps, Metal's recommended working set) or `null` where it reports none, to a quarter of which the host holds its idle texture pools and a default in-flight budget, `decode`: the decode mode and the hardware decoders, `encode`: the containers an export can write with the codecs each offers, the video and audio encoders, and the GPU-resident encode path, `features`, and `adapters`: which GPU adapter the host renders on, under which `ALPHA2_GPU_ADAPTER` policy (unset: `auto` on Linux and macOS, `performance` on Windows) and why, with every adapter it considered, `null` on the CPU renderer; see [Which GPU adapter](native.md#which-gpu-adapter)), `effectCatalog` (the built-in effects and transitions), and the adapter and FFmpeg versions with the operating system's (`osVersion`: `27.0.1 (26A434)` on macOS, `Ubuntu 22.04.5 LTS, kernel 6.8.0-136-generic` on Linux, `10.0.26300.9711` on Windows), which every result and a failure report's `environment` also carry. The result is typed as `HostCapabilities` in `@alpha2-sh/node`.

## `conform`

Reports which media each source of a project would read, and what blocks it, without rendering and without a GPU.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | `{}` | The original file of each source. |
| `proxies` | `{ "<source-id>": [{ "path": "<path>", "madeFrom"?: <fingerprint>, "colorPipeline"?: "<string>" }] }` | `{}` | Proxies made from each original, in order of preference. |
| `frames` | `{ "<source-id>": ["<path>" or null, ...] }` | `{}` | The frame files of each image sequence, in order; `null` where the sequence has a gap. |
| `purpose` | `preview`, `preview-proxy`, `thumbnail`, `remote-edit`, `export-original`, `export-proxy` | `export-original` | What the media is read for. |

The result is `{ "catalog": ..., "report": ... }`: what the host found for each source, and per source what the purpose reads and its issues. `report.ready` is `false` when an issue blocks. The types are `AssetCatalog` and `ConformReport` in `alpha2/assets`. An `offline` original and its `offline` issue carry `path`, the path the host looked up and found nothing at, when the request gave one; the `CONFORM_FAILED` message names it (`<source>: offline (<path> not found)`), and adds a file beside it whose name is the same text in another Unicode normalization when there is one. Every path reaches FFmpeg as a `file:` URL of its absolute path, so a name such as `take:1.mov` is a file, never a protocol.

## `export`

Renders a range of a project and encodes it into an MP4, MOV, Matroska or WebM file.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | required | The file of each source the export reads. |
| `startTicks`, `endTicks` | integers | required | The timeline range, in ticks (120,000 a second), widened to the frame grid. |
| `frameRate` | `{ "num", "den" }` | required | The output frame rate. |
| `output` | path | required | The file to write. A file already there is refused unless `overwrite` is true. Its extension (`.mp4` or `.m4v`, `.mov`, `.mkv`, `.webm`, in any case) names the container unless `container` does, and must match it. |
| `overwrite` | boolean | `false` | Replace a regular file at `output` once the export is whole, by rename: a failed or cancelled export leaves it as it was. Anything else there (a directory, a symbolic link) is refused with `IO_ERROR`. On Windows, a file at `output` that another program holds open without sharing deletion is waited for, up to 10 seconds, when the export finishes. |
| `container` | `mp4`, `mov`, `mkv`, `webm` | the output's extension's | |
| `proxies`, `frames` | as for `conform` | `{}` | |
| `media` | `original`, `proxies` | `original` | `proxies` reads fresh proxies where there are some, and originals elsewhere. |
| `resources` | `{ "<source-id>": { "<locator>": "<path>" } }` | `{}` | Files an SVG source's `<image href>` or a Lottie source's image assets name, by the locator the source writes. |
| `effectAssets` | `{ "<resource-id>": "<path>" }` | `{}` | The files of the project's effect resources, such as `.cube` LUTs. |
| `fonts` | `["<path>", ...]` | `[]` | Font files for captions, text and SVG text, in fallback order. A project that shows text needs them. |
| `effects` | effect definitions | `[]` | Runtime effects the project uses besides the built-in ones (`EffectDefinition` in `alpha2/effects`). An effect's `inputs` names the images it samples besides its source, which an effect node connects under `inputs`. |
| `includeAudio` | boolean | `true` | Whether the output has an audio track. |
| `videoCodec` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2`, `prores` | the container's | One the container offers (below). `prores` is 422 HQ, or 4444 with `alpha`. |
| `alpha` | boolean | `false` | Keep transparency: frames are drawn without the project background and written with an alpha plane. `vp9` in `webm` or `mkv` (8-bit), or `prores` 4444; any other codec, or VP9 in MP4, is refused with `INVALID_INPUT`. |
| `bitDepth` | `8`, `10` | `10` for `prores`, else `8` | 10-bit pictures, for `hevc`, `av1`, `vp9` (profile 2) and `prores`. Refused with `UNSUPPORTED` on a host whose renderer stores 8 bits (`features["output.tenBit"]`). |
| `videoEncoder` | FFmpeg encoder name | chosen by the host | An encoder `capabilities` reports for the codec, in `encode.video` or as the GPU-resident path's. |
| `videoQuality` | a level, or `{ "bitrate", "mode"?, "quantizer"? }` | `high` | Levels are `very-low`, `low`, `medium`, `high`, `very-high`; the host turns a level into a rate for the codec, size and frame rate. `bitrate` is in bits per second, `mode` is `constant` or `variable`. |
| `audioCodec` | `aac`, `opus` | the container's | One the container offers (below). |
| `audioQuality` | a level, or `{ "bitrate" }` | `high` | |
| `loudness` | `{ "targetLufs", "truePeakCeilingDbtp" }` | none | Normalizes the audio to an integrated loudness (-60 to 0 LUFS) under a true-peak ceiling (-20 to 0 dBTP). Needs `includeAudio`. |
| `inputPolicy` | `auto`, `gpu-required`, `cpu-readback` | `auto` | How frames reach the encoder: `auto` stays on the GPU where the host can and reads back otherwise; `gpu-required` fails with `GPU_OUTPUT_UNAVAILABLE` instead; `cpu-readback` always reads back. |
| `strategy` | `render`, `prefer-copy`, `require-copy` | `render` | `prefer-copy` copies the sources' compressed packets instead of rendering when the range shows them untouched, and renders otherwise; `require-copy` fails with `COPY_INELIGIBLE` instead. |
| `renderCache` | `{ "directory", "quotaBytes"?, "backlog"? }` | none | A directory to keep rendered frames in, so a later export of the same project, files and effects by the same host build on the same GPU takes them instead of rendering them again. `quotaBytes` is 4 GiB by default; `backlog` is `skip` (default) or `wait`. |
| `memory` | `{ "inFlightBytes"?, "decodeQueueBytes"?, "reverseCacheBytes"?, "audioWindowBytes"?, "providerSourceBytes"?, "decoderSessionBytes"?, "scale"? }` | host defaults | Byte budgets for the export. An export whose smallest working set does not fit fails with `RESOURCE_LIMIT` before it renders. `decoderSessionBytes` bounds how many video decoders stay open by an estimate of each (its pictures in their decoded format, so a 10-bit stream counts twice an 8-bit one), and never refuses an export: a session too large for it runs with fewer decoder threads, and past it, videos take turns on fewer decoders (see `native.md`). On a machine or container with less memory than the defaults assume, the budgets left at their defaults shrink together to half of what the host may use (the lower of its cgroup or job limit and the physical memory); a default `inFlightBytes` above a quarter of the GPU's memory budget (`capabilities.gpu.memory.budgetBytes`) is lowered to that quarter; the result's `memoryPolicy` reports each budget's source (`request`, `env`, `default`, `machine` or `gpu`) and the machine memory (`machine`: `{ "effective", "source", "available" }`). |

Each container offers only the codecs its mainstream players decode there; the first of each is the default:

| Container | Video | Audio |
| --- | --- | --- |
| `mp4` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `mov` | `h264`, `hevc`, `mpeg4-part2`, `prores` | `aac` |
| `mkv` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `webm` | `vp9`, `av1` | `opus` |

An unknown container, an extension that names none or disagrees with `container`, and a codec the container does not offer are refused with `INVALID_INPUT` before anything opens, as is a codec this host's FFmpeg cannot encode. `capabilities` reports the same table as `encode.containers` (`[{ "id", "extensions", "video", "audio", "defaultVideoCodec", "defaultAudioCodec" }]`), beside the encoders this host has (`encode.video`, `encode.audio`) and the codecs it writes with alpha and at 10 bits (`encode.output`: `{ "alpha", "tenBit" }`). Matroska and WebM store each frame's time to the millisecond, so their frames sit on the frame grid only to within half a millisecond.

The fields and the result are typed and described one by one in `@alpha2-sh/node` (`ExportProjectRequest`, `NativeExportResult`). The result reports, among others, the frames, the encoder and rate control used, how frames reached the encoder (`videoPipeline`: `readback`, `gpu-resident` or `packet-copy`), the encoders tried before the one that wrote the video (`videoEncoderFallbacks`: `{ "encoder", "hardwareAccelerated", "kind", "reason", "detail" }`, `kind` being `absent`, `busy`, `unsuitable` or `unknown`), the audio encoder that ran (`audioEncoder`), the font families the project's texts name that no supplied font has (`unmatchedFontFamilies`, sorted; a family matches any name a font's name table gives it, in any language, after NFC and lowercasing, and a text naming one that matches none draws in the fallback order; a still result has it too), the absolute path of the file written (`output`), how it was moved there (`publish`, below), what each source read (`media`) and timing (`renderMs`, `fps`). Among the `performance` counters: on the GPU-resident path `encoderSurfaceWaitMs` (waits for an encoder surface after a frame's sources were installed) and `encoderPlaneWaitMs` (waits for the plane pair the previous hand-off read); `sourceDownloadBytes` and `sourceDownloadMs`, decoded surfaces the renderer could not import and downloaded to the CPU instead; `encoderInputCopyBytes` and `encoderInputCopyOperations`, the copies into the encoder's input, counted on the GPU-resident path and, for a hardware encoder fed CPU frames, estimated as the frames' bytes (`encoderInputCopyEstimated: true`); and the GPU time of every eighth frame from timestamp queries, `gpuRenderMsPerFrame` (the graph) and `gpuPackMsPerFrame` (the pack into the encoder's planes or the readback), means over `gpuTimeSampledFrames`, or `gpuTimeUnavailable` saying why there are none. GPU waits also show in backend-specific fields: `readbackWaitMs` on the readback path, `encoderPlaneWaitMs` on D3D12 and Vulkan, `encoderInputReadyWaitMs` on VideoToolbox. `decodeDiagnostics` lists, besides each source's damaged-input counts, a `{ "type": "layerDropped", "code": "alpha-dropped" | "extra-layers-dropped", "source", "reason" }` entry for a source whose alpha or extra HEVC layers were not decoded (HEVC's auxiliary alpha layer; VP9 alpha on a runtime without the `libvpx-vp9` decoder), whose frames decode opaque.

The host writes the file as `<stem>.a2partial.<ext>` beside `output` and moves it into place when it is complete. A partial file another running export is writing makes the request fail with `OUTPUT_IN_USE` (after waiting a quarter of a second for it), with or without `overwrite`; one left by a process that was killed is removed and created again, never reused. An `output` that is the same file as any of the request's inputs (an asset, a proxy, a frame, a resource, an effect asset or a font; compared by file identity, so a hard link counts) is refused with `INVALID_REQUEST` before any work, with or without `overwrite`. A name too long for its folder once the tag is added gives every such file the name `<stem prefix>~<8 hex digits>.<tag>.<ext>`, the digits a hash of the output's file name.

Before it renders, the host probes the output folder with two small files, `<stem>.a2probe1.<ext>` and `<stem>.a2probe2.<ext>`, and removes them: whether a file there can be rewritten at an earlier offset (an MP4's index is), and whether a file can be renamed there without replacing one. A folder where no such rename works fails the request with `IO_ERROR` before anything renders. A folder that only writes files in order (a FUSE mount such as rclone without a cache) gets the export written to a staging file in the temporary directory, which is copied into the partial file once finished; that needs as much free space in the temporary directory as the output takes.

A finished partial file is synced to the disk, moved into place, and its folder synced. Without `overwrite` the move never replaces a file: it is one rename that refuses an existing file (`renameat2` with `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS, `MoveFileExW` without replacing on Windows). A file system that refuses that rename (exFAT on macOS, FUSE servers) gets an empty file created at `output` in a way that fails when a file is there, and the partial renamed over it. On those file systems only, a program that removes that empty file and saves its own at `output` in the moment before the rename loses its file, and a crash in that moment leaves an empty file at `output`. On Windows, a move another program blocks by holding a file open without sharing deletion is tried again for up to 10 seconds. `publish` in the result says how the file was moved: `{ "method": "rename-no-replace" | "placeholder-rename" | "rename-replace", "stagedLocally": <bool>, "retries": <count> }`.

A finished render is never deleted. When it cannot be moved into place (a file appeared at `output` while the export ran, the folder refused, a program held the file past the wait), it is moved beside `output` as `<stem>.a2kept.<ext>` (or `a2kept2` to `a2kept99` when that name is taken) and the request fails with `OUTPUT_PUBLISH_FAILED`, whose message names `output`, the operating system's error and the kept file, and whose `details.keptPath` is the kept file's absolute path. The kept file is the caller's to move or remove. When even that move fails, the message says so and the file stays as the partial file, which the next export to `output` removes.

Before it renders, the host conforms the media for `export-original` (or `export-proxy` with `media: "proxies"`); an export whose media is not ready fails with `CONFORM_FAILED`, and the error's `details` is the conform report.

Before any output opens, a render whose least working set (one frame in flight, one decoder session at one thread per video read at once, and the renderer's pictures where they are in system memory) is more than the machine can give the host now (the lower of the memory the operating system reports available and what its cgroup or job limit has left) fails with `RESOURCE_LIMIT`, `failure.stage` `machine-memory` and a message naming both numbers. On Linux the host also looks at its cgroup v2 memory once a second while it renders, and stops the export with `RESOURCE_LIMIT`, `failure.stage` `memory-limit-reached`, once the cgroup's working set reaches 95 % of its limit. Neither catches every allocation: a host the kernel's out-of-memory killer ends gets no response, and the clients report it as `HOST_OOM_KILLED` (class `OUT_OF_MEMORY`) when the cgroup's `oom_kill` count rose while it ran.

## `still`

Renders frames of a project to PNG files, each the frame an export of it draws there. One request decodes once for all its frames.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | required | As for `export`. |
| `stills` | `[{ "time": <ticks>, "output": "<path>" }, ...]` | required | 1 to 256 frames, each to its own file. `time` is an integer tick count from 0; the frame drawn is the one of the project's frame grid that holds it, as an export snaps its range. They are rendered in time order whatever order they are given in. Two outputs whose absolute paths are equal once composed (Unicode NFC) and lower-cased are one file, as a Windows or macOS file system sees them, and the request is `INVALID_INPUT` (`two stills write one file: <a> and <b>`); so is a still whose output turns out, when it is about to be written, to be a file an earlier still of the request wrote (a second name through a link). |
| `size` | `{ "width", "height" }` | the canvas | Renders the project at this canvas size, 1 to 16384 pixels each way; odd sizes too, since nothing is encoded. |
| `overwrite` | boolean | `false` | As for `export`, for every output. |
| `alpha` | boolean | `false` | Draws each still without the project background, its layers over transparent pixels. |
| `proxies`, `frames`, `media`, `resources`, `effectAssets`, `fonts`, `effects` | as for `export` | | |

Each PNG is 8-bit RGBA: the renderer's picture before any video encoding, display-encoded (sRGB) with straight alpha. Every frame is opaque over the project's `background`. Every output is SDR BT.709. The PNG is written beside its output as a partial file and moved into place, like an export's file, and a PNG that cannot be moved is kept the same way (`OUTPUT_PUBLISH_FAILED`). An output that is one of the request's inputs is refused with `INVALID_REQUEST`, and an empty list, more than 256 frames, two frames to one file, a negative time or a size out of range with `INVALID_INPUT`, before anything renders.

The result is `{ "stills": [{ "output", "frameTime", "width", "height", "publish" }], "renderer", "renderMs", "timings": { "decodeMs", "renderMs", "writeMs" }, "media", "decodeDiagnostics" }`, the frames in the order they were asked for. `output` is the absolute path written and `frameTime` the time of the frame drawn, in ticks. A still writes no progress lines.

## Cancelling

To cancel a request, write a `cancel` line naming it in `target`:

```json
{"protocolVersion":1,"requestId":"1:cancel","operation":"cancel","target":"1"}
```

It is answered at once, whatever the host is doing, with `{"protocolVersion":1,"requestId":"1:cancel","ok":true,"result":{"target":"1","state":"running"}}`:

- `running`: `target` is the request being handled. An export or still stops, removes its partial file and is answered `CANCELLED`; `capabilities` and `conform` have nothing to stop and are answered as usual. The host goes on serving.
- `pending`: no request with that id is being handled. When a request with that id is read or starts later, it is answered `CANCELLED` without running. A cancel for a request that has already been answered is kept until 1024 newer ones push it out.

A `cancel` without a `target` is answered `INVALID_REQUEST`. The host reads cancel lines while it holds at most 256 MiB of requests it has not started; past that it reads, and answers, a cancel only once it starts the next request.

`SIGTERM`, `SIGINT` and `SIGHUP` (a console control event on Windows) cancel the running export the same way; the process then exits, with the status the signal would have given it. An idle process exits at once. When the process that started `serve` exits, the host cancels what it runs and exits too (status 143), so a client that is killed does not leave its export running. A second signal, or `SIGKILL`, ends the process immediately; an export ended that way leaves its partial file and gets no response.

## Errors

| Code | Meaning |
| --- | --- |
| `UNSUPPORTED_PROTOCOL` | `protocolVersion` is missing or not 1. |
| `INVALID_REQUEST` | The line is not a request: not UTF-8, not a JSON object, too long, an unknown or missing `operation`, a missing or mistyped field, an empty `requestId`, a `cancel` without a `target`; or an output is one of the request's inputs. |
| `INVALID_INPUT` | A value is not accepted: an invalid project, range, rate or setting, or settings that do not go together. |
| `CONFORM_FAILED` | The export's media is not ready; `details` is the conform report. |
| `MISSING_INPUT` | A file the render needs was not given: an asset, an effect resource, a font or a frame. |
| `INCOMPLETE_COMPOSITION` | A composition graph has an input or output that is not connected. |
| `EFFECT_ERROR` | A runtime effect's definition, parameters or shader failed. |
| `GPU_INIT_FAILED` | The host could not create its GPU device. |
| `HARDWARE_DECODE_UNAVAILABLE` | The decode mode is `hardware-required` and the host cannot decode in hardware. |
| `GPU_DEVICE_LOST` | The GPU device was lost, or no longer runs the work it is given. |
| `GPU_DEVICE_HUNG` | The GPU device was lost to the export's own work: a submission shortly before the loss kept the GPU busy for at least half the GPU watchdog (2000 ms, `ALPHA2_GPU_WATCHDOG_MS`), or the platform reported a reset for a command that ran too long. Running it on the GPU again stops the GPU again; `failure.diagnostics.gpuSubmission` names the longest submission. |
| `GPU_TIMEOUT` | GPU work did not complete in time. |
| `DECODE_STALLED` | A decoder stopped making progress. |
| `ENCODE_STALLED` | The encoder or muxer stopped making progress. |
| `COPY_INELIGIBLE` | `strategy: "require-copy"` and the range cannot be copied. |
| `GPU_OUTPUT_UNAVAILABLE` | `inputPolicy: "gpu-required"` and the GPU-resident encode path cannot take the export. |
| `ENCODE_ERROR` | Encoding or muxing failed. |
| `ENCODER_BUSY` | No video encoder opened, and a hardware one refused because other exports hold its sessions; the same export can succeed once they end. |
| `IO_ERROR` | A file could not be read or written, or `output` already exists and the request does not overwrite it. |
| `OUTPUT_IN_USE` | Another running export is writing the same `output`. |
| `OUTPUT_PUBLISH_FAILED` | The output was rendered in full but could not be moved into place; `details.keptPath` names where the finished file was kept. |
| `CANCELLED` | The request was cancelled: by a `cancel` line, before or while it ran, or by a termination signal. |
| `RESOURCE_LIMIT` | A budget or limit refused the work, such as a memory budget too small for the export; `failure.stage` is `machine-memory` or `memory-limit-reached` when the machine's memory refused it (see `export`). |
| `INTERNAL` | A defect in the host failed the request. The host answers the requests after it. |
| `ALPHA2_ERROR` | A failure no other code describes. |

The codes the engine itself reports (`INVALID_INPUT`, `MISSING_INPUT`, `INCOMPLETE_COMPOSITION`, `EFFECT_ERROR`, `ENCODE_ERROR`, `GPU_DEVICE_LOST`, `GPU_DEVICE_HUNG`, `GPU_TIMEOUT`, `ALPHA2_ERROR`) are the ones the browser engine reports too.

An `export` that fails, before or while it renders, also has `error.failure`: the class of the failure (`code`, such as `OUT_OF_MEMORY`, `DEVICE_LOST`, `INVALID_INPUT` or `CANCELLED`), the step it failed at (`phase`, `stage`) and the frame (`frameIndex`), the protocol code (`hostCode`), and the device, memory and path diagnostics recorded up to then (`diagnostics`). It reports the first failure, not the cancellations that failure caused in the rest of the export. A `RESOURCE_BUSY` failure (`ENCODER_BUSY`) also has `retryAfterMs`, how long to wait before trying again. The type is `ExportFailure` in `alpha2/export-failure`, and `alpha2/export-retry` decides from it whether to retry.

## Security

Runtime effects are WGSL that runs on the GPU. A host that takes requests from other users or over a network must decide who may send effect definitions before it passes them on: the engine checks a definition, not who sent it. `assets`, `output` and every other path are paths on the host's file system; moving files to and from the host is the caller's job.
