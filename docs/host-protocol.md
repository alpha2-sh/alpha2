# Host protocol

`alpha2-host serve` takes requests on standard input and answers on standard output. A program in any language can start it and export projects through it; `@alpha2-sh/node` is a client of this protocol, and Node.js programs use that instead. This page is version 1 of the protocol. [`native.md`](native.md) says where `alpha2-host` comes from.

## Messages

Every message, both ways, is one JSON object on one line of UTF-8 text, and has `"protocolVersion": 1`.

A request has a `requestId`, a string that is not empty, and an `operation`: `capabilities`, `conform`, `export` or `still`.

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

Standard error carries human-readable diagnostics, not protocol messages.

## `capabilities`

Takes no other fields. The result reports what this host can do: `status: "ready"`, `capabilities` (the GPU and its tier, `decode`: the decode mode and the hardware decoders, `encode`: the containers an export can write with the codecs each offers, the video and audio encoders, and the GPU-resident encode path, and `features`), `effectCatalog` (the built-in effects and transitions), and the adapter and FFmpeg versions. The result is typed as `HostCapabilities` in `@alpha2-sh/node`.

## `conform`

Reports which media each source of a project would read, and what blocks it, without rendering and without a GPU.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | `{}` | The original file of each source. |
| `proxies` | `{ "<source-id>": [{ "path": "<path>", "madeFrom"?: <fingerprint>, "colorPipeline"?: "<string>" }] }` | `{}` | Proxies made from each original, in order of preference. |
| `frames` | `{ "<source-id>": ["<path>" or null, ...] }` | `{}` | The frame files of each image sequence, in order; `null` where the sequence has a gap. |
| `purpose` | `preview`, `preview-proxy`, `thumbnail`, `remote-edit`, `export-original`, `export-proxy` | `export-original` | What the media is read for. |

The result is `{ "catalog": ..., "report": ... }`: what the host found for each source, and per source what the purpose reads and its issues. `report.ready` is `false` when an issue blocks. The types are `AssetCatalog` and `ConformReport` in `alpha2/assets`.

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
| `memory` | `{ "inFlightBytes"?, "decodeQueueBytes"?, "reverseCacheBytes"?, "audioWindowBytes"?, "providerSourceBytes"?, "decoderSessionBytes"?, "scale"? }` | host defaults | Byte budgets for the export. An export whose smallest working set does not fit fails with `RESOURCE_LIMIT` before it renders. `decoderSessionBytes` bounds how many video decoders stay open by an estimate of each, and never refuses an export: past it, videos take turns on fewer decoders (see `native.md`). |

Each container offers only the codecs its mainstream players decode there; the first of each is the default:

| Container | Video | Audio |
| --- | --- | --- |
| `mp4` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `mov` | `h264`, `hevc`, `mpeg4-part2`, `prores` | `aac` |
| `mkv` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `webm` | `vp9`, `av1` | `opus` |

An unknown container, an extension that names none or disagrees with `container`, and a codec the container does not offer are refused with `INVALID_INPUT` before anything opens, as is a codec this host's FFmpeg cannot encode. `capabilities` reports the same table as `encode.containers` (`[{ "id", "extensions", "video", "audio", "defaultVideoCodec", "defaultAudioCodec" }]`), beside the encoders this host has (`encode.video`, `encode.audio`) and the codecs it writes with alpha and at 10 bits (`encode.output`: `{ "alpha", "tenBit" }`). Matroska and WebM store each frame's time to the millisecond, so their frames sit on the frame grid only to within half a millisecond.

The fields and the result are typed and described one by one in `@alpha2-sh/node` (`ExportProjectRequest`, `NativeExportResult`). The result reports, among others, the frames, the encoder and rate control used, how frames reached the encoder (`videoPipeline`: `readback`, `gpu-resident` or `packet-copy`), the absolute path of the file written (`output`), how it was moved there (`publish`, below), what each source read (`media`) and timing (`renderMs`, `fps`).

The host writes the file as `<stem>.a2partial.<ext>` beside `output` and moves it into place when it is complete. A partial file another running export is writing makes the request fail with `OUTPUT_IN_USE` (after waiting a quarter of a second for it), with or without `overwrite`; one left by a process that was killed is removed and created again, never reused. An `output` that is the same file as any of the request's inputs (an asset, a proxy, a frame, a resource, an effect asset or a font; compared by file identity, so a hard link counts) is refused with `INVALID_REQUEST` before any work, with or without `overwrite`. A name too long for its folder once the tag is added gives every such file the name `<stem prefix>~<8 hex digits>.<tag>.<ext>`, the digits a hash of the output's file name.

Before it renders, the host probes the output folder with two small files, `<stem>.a2probe1.<ext>` and `<stem>.a2probe2.<ext>`, and removes them: whether a file there can be rewritten at an earlier offset (an MP4's index is), and whether a file can be renamed there without replacing one. A folder where no such rename works fails the request with `IO_ERROR` before anything renders. A folder that only writes files in order (a FUSE mount such as rclone without a cache) gets the export written to a staging file in the temporary directory, which is copied into the partial file once finished; that needs as much free space in the temporary directory as the output takes.

A finished partial file is synced to the disk, moved into place, and its folder synced. Without `overwrite` the move never replaces a file: it is one rename that refuses an existing file (`renameat2` with `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS, `MoveFileExW` without replacing on Windows). A file system that refuses that rename (exFAT on macOS, FUSE servers) gets an empty file created at `output` in a way that fails when a file is there, and the partial renamed over it. On those file systems only, a program that removes that empty file and saves its own at `output` in the moment before the rename loses its file, and a crash in that moment leaves an empty file at `output`. On Windows, a move another program blocks by holding a file open without sharing deletion is tried again for up to 10 seconds. `publish` in the result says how the file was moved: `{ "method": "rename-no-replace" | "placeholder-rename" | "rename-replace", "stagedLocally": <bool>, "retries": <count> }`.

A finished render is never deleted. When it cannot be moved into place (a file appeared at `output` while the export ran, the folder refused, a program held the file past the wait), it is moved beside `output` as `<stem>.a2kept.<ext>` (or `a2kept2` to `a2kept99` when that name is taken) and the request fails with `OUTPUT_PUBLISH_FAILED`, whose message names `output`, the operating system's error and the kept file, and whose `details.keptPath` is the kept file's absolute path. The kept file is the caller's to move or remove. When even that move fails, the message says so and the file stays as the partial file, which the next export to `output` removes.

Before it renders, the host conforms the media for `export-original` (or `export-proxy` with `media: "proxies"`); an export whose media is not ready fails with `CONFORM_FAILED`, and the error's `details` is the conform report.

## `still`

Renders frames of a project to PNG files, each the frame an export of it draws there. One request decodes once for all its frames.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | required | As for `export`. |
| `stills` | `[{ "time": <ticks>, "output": "<path>" }, ...]` | required | 1 to 256 frames, each to its own file. `time` is an integer tick count from 0; the frame drawn is the one of the project's frame grid that holds it, as an export snaps its range. They are rendered in time order whatever order they are given in. |
| `size` | `{ "width", "height" }` | the canvas | Renders the project at this canvas size, 1 to 16384 pixels each way; odd sizes too, since nothing is encoded. |
| `overwrite` | boolean | `false` | As for `export`, for every output. |
| `alpha` | boolean | `false` | Draws each still without the project background, its layers over transparent pixels. |
| `proxies`, `frames`, `media`, `resources`, `effectAssets`, `fonts`, `effects` | as for `export` | | |

Each PNG is 8-bit RGBA: the renderer's picture before any video encoding, display-encoded (sRGB) with straight alpha. Every frame is opaque over the project's `background`. Every output is SDR BT.709. The PNG is written beside its output as a partial file and moved into place, like an export's file, and a PNG that cannot be moved is kept the same way (`OUTPUT_PUBLISH_FAILED`). An output that is one of the request's inputs is refused with `INVALID_REQUEST`, and an empty list, more than 256 frames, two frames to one file, a negative time or a size out of range with `INVALID_INPUT`, before anything renders.

The result is `{ "stills": [{ "output", "frameTime", "width", "height", "publish" }], "renderer", "renderMs", "timings": { "decodeMs", "renderMs", "writeMs" }, "media", "decodeDiagnostics" }`, the frames in the order they were asked for. `output` is the absolute path written and `frameTime` the time of the frame drawn, in ticks. A still writes no progress lines.

## Cancelling

To cancel the running export, send the process `SIGTERM` or `SIGINT` (a console control event on Windows). The export stops, removes its partial file and is answered `CANCELLED`, and the process exits. An idle process exits at once. A second signal, or `SIGKILL`, ends the process immediately; an export ended that way leaves its partial file and gets no response.

## Errors

| Code | Meaning |
| --- | --- |
| `UNSUPPORTED_PROTOCOL` | `protocolVersion` is missing or not 1. |
| `INVALID_REQUEST` | The line is not a request: not UTF-8, not a JSON object, too long, an unknown or missing `operation`, a missing or mistyped field, an empty `requestId`; or an output is one of the request's inputs. |
| `INVALID_INPUT` | A value is not accepted: an invalid project, range, rate or setting, or settings that do not go together. |
| `CONFORM_FAILED` | The export's media is not ready; `details` is the conform report. |
| `MISSING_INPUT` | A file the render needs was not given: an asset, an effect resource, a font or a frame. |
| `INCOMPLETE_COMPOSITION` | A composition graph has an input or output that is not connected. |
| `EFFECT_ERROR` | A runtime effect's definition, parameters or shader failed. |
| `GPU_INIT_FAILED` | The host could not create its GPU device. |
| `HARDWARE_DECODE_UNAVAILABLE` | The decode mode is `hardware-required` and the host cannot decode in hardware. |
| `GPU_DEVICE_LOST` | The GPU device was lost, or no longer runs the work it is given. |
| `GPU_TIMEOUT` | GPU work did not complete in time. |
| `DECODE_STALLED` | A decoder stopped making progress. |
| `ENCODE_STALLED` | The encoder or muxer stopped making progress. |
| `COPY_INELIGIBLE` | `strategy: "require-copy"` and the range cannot be copied. |
| `GPU_OUTPUT_UNAVAILABLE` | `inputPolicy: "gpu-required"` and the GPU-resident encode path cannot take the export. |
| `ENCODE_ERROR` | Encoding or muxing failed. |
| `IO_ERROR` | A file could not be read or written, or `output` already exists and the request does not overwrite it. |
| `OUTPUT_IN_USE` | Another running export is writing the same `output`. |
| `OUTPUT_PUBLISH_FAILED` | The output was rendered in full but could not be moved into place; `details.keptPath` names where the finished file was kept. |
| `CANCELLED` | The export was cancelled. |
| `RESOURCE_LIMIT` | A budget or limit refused the work, such as a memory budget too small for the export. |
| `INTERNAL` | A defect in the host failed the request. The host answers the requests after it. |
| `ALPHA2_ERROR` | A failure no other code describes. |

The codes the engine itself reports (`INVALID_INPUT`, `MISSING_INPUT`, `INCOMPLETE_COMPOSITION`, `EFFECT_ERROR`, `ENCODE_ERROR`, `GPU_DEVICE_LOST`, `GPU_TIMEOUT`, `ALPHA2_ERROR`) are the ones the browser engine reports too.

An `export` that fails, before or while it renders, also has `error.failure`: the class of the failure (`code`, such as `OUT_OF_MEMORY`, `DEVICE_LOST`, `INVALID_INPUT` or `CANCELLED`), the step it failed at (`phase`, `stage`) and the frame (`frameIndex`), the protocol code (`hostCode`), and the device, memory and path diagnostics recorded up to then (`diagnostics`). It reports the first failure, not the cancellations that failure caused in the rest of the export. The type is `ExportFailure` in `alpha2/export-failure`, and `alpha2/export-retry` decides from it whether to retry.

## Security

Runtime effects are WGSL that runs on the GPU. A host that takes requests from other users or over a network must decide who may send effect definitions before it passes them on: the engine checks a definition, not who sent it. `assets`, `output` and every other path are paths on the host's file system; moving files to and from the host is the caller's job.
