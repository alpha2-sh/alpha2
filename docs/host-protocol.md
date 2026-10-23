# Host protocol

`alpha2-host serve` takes requests on standard input and answers on standard output. A program in any language can start it and export projects through it; `@alpha2-sh/node` is a client of this protocol, and Node.js programs use that instead. This page is version 1 of the protocol. [`native.md`](native.md) says where `alpha2-host` comes from.

## Messages

Every message, both ways, is one JSON object on one line of UTF-8 text, and has `"protocolVersion": 1`.

A request has a `requestId`, a string that is not empty, and an `operation`: `capabilities`, `conform` or `export`.

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
- While an export runs, the host also writes progress lines: `{"protocolVersion":1,"type":"progress","requestId":"...","phase":"frames","completed":12,"total":30}`. `phase` is `frames` while frames are encoded and `finalizing` once every frame is and the file is being completed. The export is finished only when its response arrives.
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
| `output` | path | required | The file to write. It must not exist. Its extension (`.mp4` or `.m4v`, `.mov`, `.mkv`, `.webm`, in any case) names the container unless `container` does, and must match it. |
| `container` | `mp4`, `mov`, `mkv`, `webm` | the output's extension's | |
| `proxies`, `frames` | as for `conform` | `{}` | |
| `media` | `original`, `proxies` | `original` | `proxies` reads fresh proxies where there are some, and originals elsewhere. |
| `resources` | `{ "<source-id>": { "<locator>": "<path>" } }` | `{}` | Files an SVG source's `<image href>` or a Lottie source's image assets name, by the locator the source writes. |
| `effectAssets` | `{ "<resource-id>": "<path>" }` | `{}` | The files of the project's effect resources, such as `.cube` LUTs. |
| `fonts` | `["<path>", ...]` | `[]` | Font files for captions, text and SVG text, in fallback order. A project that shows text needs them. |
| `effects` | effect definitions | `[]` | Runtime effects the project uses besides the built-in ones (`EffectDefinition` in `alpha2/effects`). An effect's `inputs` names the images it samples besides its source, which an effect node connects under `inputs`. |
| `includeAudio` | boolean | `true` | Whether the output has an audio track. |
| `videoCodec` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | the container's | One the container offers (below). |
| `videoEncoder` | FFmpeg encoder name | chosen by the host | An encoder `capabilities` reports for the codec, in `encode.video` or as the GPU-resident path's. |
| `videoQuality` | a level, or `{ "bitrate", "mode"?, "quantizer"? }` | `high` | Levels are `very-low`, `low`, `medium`, `high`, `very-high`; the host turns a level into a rate for the codec, size and frame rate. `bitrate` is in bits per second, `mode` is `constant` or `variable`. |
| `audioCodec` | `aac`, `opus` | the container's | One the container offers (below). |
| `audioQuality` | a level, or `{ "bitrate" }` | `high` | |
| `loudness` | `{ "targetLufs", "truePeakCeilingDbtp" }` | none | Normalizes the audio to an integrated loudness (-60 to 0 LUFS) under a true-peak ceiling (-20 to 0 dBTP). Needs `includeAudio`. |
| `inputPolicy` | `auto`, `gpu-required`, `cpu-readback` | `auto` | How frames reach the encoder: `auto` stays on the GPU where the host can and reads back otherwise; `gpu-required` fails with `GPU_OUTPUT_UNAVAILABLE` instead; `cpu-readback` always reads back. |
| `strategy` | `render`, `prefer-copy`, `require-copy` | `render` | `prefer-copy` copies the sources' compressed packets instead of rendering when the range shows them untouched, and renders otherwise; `require-copy` fails with `COPY_INELIGIBLE` instead. |
| `renderCache` | `{ "directory", "quotaBytes"?, "backlog"? }` | none | A directory to keep rendered frames in, so a later export of the same project, files and effects by the same host build on the same GPU takes them instead of rendering them again. `quotaBytes` is 4 GiB by default; `backlog` is `skip` (default) or `wait`. |
| `memory` | `{ "inFlightBytes"?, "decodeQueueBytes"?, "reverseCacheBytes"?, "audioWindowBytes"?, "providerSourceBytes"?, "scale"? }` | host defaults | Byte budgets for the export. An export whose smallest working set does not fit fails with `RESOURCE_LIMIT` before it renders. |

Each container offers only the codecs its mainstream players decode there; the first of each is the default:

| Container | Video | Audio |
| --- | --- | --- |
| `mp4` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `mov` | `h264`, `hevc`, `mpeg4-part2` | `aac` |
| `mkv` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `webm` | `vp9`, `av1` | `opus` |

An unknown container, an extension that names none or disagrees with `container`, and a codec the container does not offer are refused with `INVALID_INPUT` before anything opens, as is a codec this host's FFmpeg cannot encode. `capabilities` reports the same table as `encode.containers` (`[{ "id", "extensions", "video", "audio", "defaultVideoCodec", "defaultAudioCodec" }]`), beside the encoders this host has (`encode.video`, `encode.audio`). Matroska and WebM store each frame's time to the millisecond, so their frames sit on the frame grid only to within half a millisecond.

The fields and the result are typed and described one by one in `@alpha2-sh/node` (`ExportProjectRequest`, `NativeExportResult`). The result reports, among others, the frames, the encoder and rate control used, the output path (`videoOutputPath`), what each source read (`media`) and timing (`renderMs`, `fps`).

The host writes the file as `<stem>.alpha2-partial.<ext>` beside `output` and renames it into place when it is complete. A partial file another running export is writing makes the request fail with `OUTPUT_IN_USE`; one left by a process that was killed is taken over.

Before it renders, the host conforms the media for `export-original` (or `export-proxy` with `media: "proxies"`); an export whose media is not ready fails with `CONFORM_FAILED`, and the error's `details` is the conform report.

## Cancelling

To cancel the running export, send the process `SIGTERM` or `SIGINT` (a console control event on Windows). The export stops, removes its partial file and is answered `CANCELLED`, and the process exits. An idle process exits at once. A second signal, or `SIGKILL`, ends the process immediately; an export ended that way leaves its partial file and gets no response.

## Errors

| Code | Meaning |
| --- | --- |
| `UNSUPPORTED_PROTOCOL` | `protocolVersion` is missing or not 1. |
| `INVALID_REQUEST` | The line is not a request: not UTF-8, not a JSON object, too long, an unknown or missing `operation`, a missing or mistyped field, an empty `requestId`. |
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
| `IO_ERROR` | A file could not be read or written, or `output` already exists. |
| `OUTPUT_IN_USE` | Another running export is writing the same `output`. |
| `CANCELLED` | The export was cancelled. |
| `RESOURCE_LIMIT` | A budget or limit refused the work, such as a memory budget too small for the export. |
| `INTERNAL` | A defect in the host failed the request. The host answers the requests after it. |
| `ALPHA2_ERROR` | A failure no other code describes. |

The codes the engine itself reports (`INVALID_INPUT`, `MISSING_INPUT`, `INCOMPLETE_COMPOSITION`, `EFFECT_ERROR`, `ENCODE_ERROR`, `GPU_DEVICE_LOST`, `GPU_TIMEOUT`, `ALPHA2_ERROR`) are the ones the browser engine reports too.

An `export` that fails, before or while it renders, also has `error.failure`: the class of the failure (`code`, such as `OUT_OF_MEMORY`, `DEVICE_LOST`, `INVALID_INPUT` or `CANCELLED`), the step it failed at (`phase`, `stage`) and the frame (`frameIndex`), the protocol code (`hostCode`), and the device, memory and path diagnostics recorded up to then (`diagnostics`). It reports the first failure, not the cancellations that failure caused in the rest of the export. The type is `ExportFailure` in `alpha2/export-failure`, and `alpha2/export-retry` decides from it whether to retry.

## Security

Runtime effects are WGSL that runs on the GPU. A host that takes requests from other users or over a network must decide who may send effect definitions before it passes them on: the engine checks a definition, not who sent it. `assets`, `output` and every other path are paths on the host's file system; moving files to and from the host is the caller's job.
