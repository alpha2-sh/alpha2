# Host protocol

`alpha2-host serve` takes requests on standard input and answers on standard output. A program in any language can start it and export projects through it; `@alpha2-sh/node` is a client of this protocol, and Node.js programs use that instead. This page is version 2 of the protocol. [`native.md`](native.md) says where `alpha2-host` comes from.

## Messages

Every message, both ways, is a frame:

| Bytes | |
| --- | --- |
| 4 | the header's length, a little-endian unsigned 32-bit integer |
| 4 | the attachments' total length, the same way |
| header length | the header: one JSON object in UTF-8, with `"protocolVersion": 2` |
| attachments length | the attachments, one after another |

A frame with attachments lists them in its header, in order, as `"attachments": [{"type": "<media type>", "bytes": <length>}, ...]`, whose lengths add up to the attachments' length. A reader finds the next frame from the two lengths alone, so a frame it cannot read is skipped whole. The host writes text in a header as UTF-8, unescaped.

The host owns standard output: before its first frame it writes the 8 bytes `89 41 32 48 4F 53 54 0A` (`\x89A2HOST\n`), and from then on whatever a library in the process prints to standard output goes to standard error. A client reads past those 8 bytes, keeping whatever came before them as diagnostics.

The first frame the host writes, before it reads any input, is its `ready` state (see [Host state](#host-state)): `{"protocolVersion":2,"type":"state","state":"ready","reason":"started","details":{...}}`. A host that writes nothing for long is held before it runs (a Gatekeeper check on macOS, an antivirus scan or an App Control policy on Windows, a stopped process); `@alpha2-sh/node` and the Python client end it after 30 seconds and fail with `HOST_START_TIMEOUT`, whose `stderr` holds what it wrote. A host that dies of `SIGKILL` while its cgroup's `oom_kill` count (cgroup v2 `memory.events`, Linux) rises fails with `HOST_OOM_KILLED` in both clients.

A request has a `requestId`, a string that is not empty, and an `operation`: `capabilities`, `conform`, `export`, `still`, `compose`, `edit`, `extract`, `cancel` or `status`; a client running an extractor also sends `extract.ack` (see [`extract`](#extract)).

```json
{"protocolVersion":2,"requestId":"1","operation":"capabilities"}
```

The host answers each request with one response that carries its `requestId`:

```json
{"protocolVersion":2,"requestId":"1","ok":true,"result":{...},"hostState":"ready"}
{"protocolVersion":2,"requestId":"1","ok":false,"error":{"code":"INVALID_INPUT","message":"..."},"hostState":"ready"}
```

`hostState` is the host's state once the request has ended, after the checks of [Host state](#host-state).

- Requests can be written before earlier ones are answered. Match each response to its request by `requestId`, and keep the ids unique: the host does not refuse one it has seen.
- A field the operation does not take is refused with `INVALID_REQUEST`, on every operation.
- Read standard output and standard error while writing requests: a host whose output pipe is full stops until it is read.
- The host runs up to `--max-running <count>` requests at once (a `serve` flag, 1 by default), each on its own thread against its one GPU device, and answers each as it ends, so responses come in any order. With a higher count, requests share the device: budgets and codec threads are not divided (the renderer's result cache holds the sum of the running requests' budgets), and an export's GPU counters cover the requests that ran with it. A request that starts beside a running one leaves the renderer's cache and pools alone; once the last running request ends, the host releases them, so an idle host holds no rendered frames. A request past that count waits for a running one to end. A runtime effect `id`@`version` or an effect resource id that a running request defines differently waits until that request ends; requests that define it alike share it. When the memory watchdog of `export` stops an export, it stops the newest one first. An export's GPU timing counters (`gpuSubmissions`, `gpuRenderMsPerFrame`, `gpuPackMsPerFrame`, `pipelineCompileMs`) cover the GPU work of the requests that ran with it.
- A request the host cannot read is answered with `"requestId": "unknown"` when it has no usable `requestId`.
- A request's header is at most 128 MiB, the `project` in it at most 64 MiB, and its attachments at most 256 MiB. No request or acknowledgement takes attachments; one with some is refused with `INVALID_REQUEST`.
- A response's header is at most 256 MiB (`MAX_RESPONSE_HEADER_BYTES`). The host does not write a longer one: it answers the request `RESOURCE_LIMIT` instead, with `details` `{ "headerBytes", "limit" }`, and keeps serving. Both clients read headers up to that size.
- While an export runs, the host also writes progress messages: `{"protocolVersion":2,"type":"progress","requestId":"...","phase":"frames","completed":12,"total":30}`. `phase` is `frames` while frames are encoded and `finalizing` once every frame is and the file is being completed. A message with `"restarted": true` and `completed` 0 says the export started over on the readback path, because the GPU-resident encoder rejected a frame on a device that was still usable; the result's `gpuOutputPathFallback` names the encoder, the frame and the reason. The export is finished only when its response arrives.
- When the host cannot start its GPU device (or, in `hardware-required` decode mode, its decode device) for the first request that needs it, it writes `{"protocolVersion":2,"type":"fatal","error":{...}}` and exits; that request and the ones after it get no response. `conform`, `compose` and `edit` need no GPU. A setting it does not take is `fatal` too, with `INVALID_INPUT`: an unknown `ALPHA2_VIDEO_DECODE` right after `ready`, an unknown `ALPHA2_RENDERER` or an `ALPHA2_GPU_ADAPTER` that names no adapter at the first request that needs a device.
- When the host's device is lost or a decoder or encoder thread is stuck, the host goes `draining`, answers those it has read but not started with `HOST_RETIRING`, stops the requests it runs and answers them `CANCELLED` with `details.hostRetiring` naming the reason (an export stopped this way leaves no output), and exits. It waits for those answers for the stall limit plus 5 seconds, as a termination does (see [Cancelling](#cancelling)); a request still running then gets no response.
- A request whose own thread stays in one step for three times the stall limit (120 seconds by default) is answered by the host with the code of that step's side: `DECODE_STALLED` while it reads media (conform, audio, the loudness pass), `GPU_TIMEOUT` while it renders or reads back, `ENCODE_STALLED` while it probes or opens an encoder, encodes, publishes or writes its answer, `PLUGIN_LIMIT` in a native plugin call, `INTERNAL` otherwise. The message names the step and the time; an export's answer carries a `failure` report. The thread never comes back, so the host then retires as for `threads-abandoned`. Waits on a client (an extraction step) or on a bound of their own do not count.
- The end of standard input means no more requests: the host answers every request it has read, then exits. A failure to read standard input, unlike its end, is a `fatal` with `IO_ERROR` once the running requests have answered; requests written after the failure get no response. A client that may cancel keeps standard input open until it has its answer.
- The process exits with status 0 once it has drained or its input has ended, 70 after a `fatal` with `GPU_INIT_FAILED`, 1 after any other `fatal` and when it retires after `device-lost` or `threads-abandoned`, the status the signal gives after a termination signal (see [Cancelling](#cancelling)), and 101 after a defect outside every request.

Standard error carries human-readable diagnostics, not protocol messages. Responses can carry attachments: a [`still`](#still) without an `output` returns its PNG as one.

## Host state

A host is `ready`, taking requests, or `draining`, taking none and about to exit. It sends a `state` message when it starts and whenever its state changes:

```json
{"protocolVersion":2,"type":"state","state":"draining","reason":"memory","details":{...}}
```

The host decides when a new process would serve better than itself and drains; the client only follows. After each request it compares itself with its soft limits, set by `serve` flags (`0` turns a limit off). `memory`, `memory-growth` and `handles` measure the whole process, so they are compared only after an answer that leaves no request running:

| Reason | Flag | Default | The host drains when |
| --- | --- | --- | --- |
| `memory` | `--max-memory-share <fraction>` | `0.5` | its footprint (the memory the operating system charges it: `RssAnon + RssShmem` on Linux, the physical footprint on macOS, private bytes on Windows) is over this share of the memory it may use (the lower of its cgroup or job limit and the physical memory) |
| `memory-growth` | `--max-memory-growth <bytes>` | 1 GiB | its footprint grew by more than this since its baseline, taken at the first answer that left no request running and taken again at the first such answer after the host made its renderer |
| `requests` | `--max-requests <count>` | `1000` | it has answered this many requests |
| `handles` | `--max-handles <count>` | `4096`, and at most half the open-file limit on Linux and macOS | it holds more open handles (file descriptors on Linux and macOS, kernel handles on Windows) than this |
| `degraded` | none | | it has a degradation a new process would not have, such as a hardware encoder it turned off for the rest of its life, or a render cache directory it stopped using because a thread got stuck in its file system; `details.degradations` names it |
| `idle` | `--idle-timeout <ms>` | `120000` | it has run nothing for this long |

It also drains, with no limit to compare, when its GPU device is lost (`device-lost`), when a decoder or encoder thread is stuck and abandoned (`threads-abandoned`), when it is asked to terminate (`terminating`), when its input ends (`input-closed`) and after a request panicked (`defect`: its locks and caches may hold what the unwind left, so running requests finish and new ones go to a new process). The memory watchdog of `export` still stops an export that runs out of memory while it runs; the soft limits are only compared between requests.

The response of the request that moved the host to `draining` carries `"hostState": "draining"` and comes after the `state` message; a response another running request writes at that moment can carry `"hostState": "draining"` before the `state` message arrives. A request the host reads while `draining`, or has read but not started when it drains, is answered `HOST_RETIRING` without running; send it to another host. A draining host exits once no request runs and either its input has ended or 5 seconds have passed since it started draining; it waits for running requests without a time limit. After `device-lost` and `threads-abandoned` it answers what it has read first, then stops the requests it runs, answers them `CANCELLED` with `details.hostRetiring`, and exits once they have answered or the stall limit plus 5 seconds have passed, when a request still running gets no response.

`details` is for observation, and may gain fields; only `hostVersion` is meant for a decision:

| Field | |
| --- | --- |
| `hostVersion` | the alpha2 release of this host, such as `0.6.1`; both clients refuse a host whose `ready` state names another release than their own, or none, with `HOST_VERSION_MISMATCH` |
| `activity` | `idle` or `busy` |
| `runningRequests` | requests running now |
| `requestsServed` | requests answered so far |
| `memory` | `{ "residentBytes", "footprintBytes", "baselineBytes", "capacityBytes" }`: now, the `memory-growth` baseline (`null` until it is taken), and what the host may use; `null` where unknown |
| `openHandles` | open handles (file descriptors, or kernel handles on Windows), `null` where unknown |
| `renderer` | `gpu`, `cpu`, or `null` before a request needed one |
| `rendererFallback` | why the host renders on the CPU, when `auto` chose it |
| `hardwareDecodeFallback` | why `auto` decode runs in software |
| `degradations` | what this process has lost that a new one would not have |
| `limits` | `{ "maxMemoryShare", "maxMemoryGrowthBytes", "maxRequests", "maxHandles", "idleTimeoutMs" }`, `null` for one turned off |

## `status`

Takes no other fields, and is answered as soon as the host reads it, without waiting for running requests, as `cancel` is: `result` is `{ "state", "reason", "details" }`, as the last `state` message would say now. It is for diagnostics and monitoring; a client keeping hosts does not need it.

## Keeping hosts

Starting a host costs a process, a GPU device and its probes, so a client keeps hosts and sends them one request after another. `@alpha2-sh/node` and the Python client follow these rules, which are all a client in another language needs:

- Run at most `concurrency` hosts, and send each one request at a time.
- Use only a host of the client's own release: refuse one whose `ready` state names another `details.hostVersion`, or none, and one whose first line before the magic is the `ready` line of another protocol version (`{"protocolVersion":1,"type":"ready"}` from 0.5.x). Both clients end such a host and fail at once with `HOST_VERSION_MISMATCH`, which no retry mends.
- Serve requests in the order they were made, and consider only the oldest waiting one: send it to an idle `ready` host of its modes. With none, start a new host for it, which counts as started once its `ready` message arrives; at the limit, end one idle host of other modes to make room, or wait for a host to come free. A younger request never takes a host the oldest one is waiting for.
- A host takes its settings from the environment it was started with, the renderer and decode modes (`ALPHA2_RENDERER`, `ALPHA2_VIDEO_DECODE`) among them, so hosts are kept apart by the whole environment they start with: a request goes only to a host started with the environment it would start one with now.
- Stop sending to a host when it reports `draining`, in a `state` message or a response's `hostState`, or when it exits. Close its standard input once it runs nothing.
- Send a request answered `HOST_RETIRING` again, to another host.
- A host resolves a relative path against the directory it started in, so send every path absolute: resolve a relative one against the client's working directory when the call is made. A plugin manifest given as an `https:` or `file:` URL is sent as it is.
- A request written in full whose host exits before answering fails with what the exit says (`HOST_EXITED`, `HOST_OOM_KILLED`, ...) and is never sent again: an export writes files, and only its caller knows whether running it again is safe. A request whose write had not completed when its host exited was not read, so it never ran: send it to another host, as `HOST_RETIRING` is, within the same bound of three resends, after which it fails with the last exit's error. "Written in full" means the operating system took every byte of the frame; a host can still die before reading bytes the pipe holds, and that request fails as above. Do not write to a host whose process the operating system already reports as ended: on Windows its pipe takes bytes until its handles close, though nothing reads them, so such a request counts as not written. When a write fails, take the exit from the process, not from the end of its output: a process the host started can hold its standard output open.

Everything else stays as it is for one process: the start timeout, the classification of an exit, and cancelling. The client has no memory checks, health polls or idle timers of its own.

## `capabilities`

Takes no other fields. The result reports what this host can do: `status: "ready"`, `capabilities` (the GPU and its tier, with `gpu.memory.budgetBytes`, the device memory budget the adapter's graphics API reports (DXGI's local segment budget, Vulkan's `VK_EXT_memory_budget` device-local heaps, Metal's recommended working set) or `null` where it reports none, to a quarter of which the host holds its idle texture pools and a default in-flight budget, `decode`: the decode mode, the hardware decoders and the decoders a source may use (`sourceDecoders`, and `everyDecoder` when the host was started with `--allow-every-decoder`; see [Which decoders a source may use](native.md#which-decoders-a-source-may-use)), `encode`: the containers an export can write with the codecs each offers, the video and audio encoders, and the GPU-resident encode path, `features`, and `adapters`: which GPU adapter the host renders on, under which `ALPHA2_GPU_ADAPTER` policy (unset: `auto` on Linux and macOS, `performance` on Windows) and why, with every adapter it considered, `null` on the CPU renderer; see [Which GPU adapter](native.md#which-gpu-adapter)), `effectCatalog` (the built-in effects and transitions), and the adapter and FFmpeg versions with the operating system's (`osVersion`: `27.0.1 (26A434)` on macOS, `Ubuntu 22.04.5 LTS, kernel 6.8.0-136-generic` on Linux, `10.0.26300.9711` on Windows), which every result and a failure report's `environment` also carry. The result is typed as `HostCapabilities` in `@alpha2-sh/node`.

## `conform`

Reports which media each source of a project would read, and what blocks it, without rendering and without a GPU.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | `{}` | The original file of each source. |
| `proxies` | `{ "<source-id>": [{ "path": "<path>", "madeFrom"?: <fingerprint>, "colorPipeline"?: "<string>" }] }` | `{}` | Proxies made from each original, in order of preference. |
| `frames` | `{ "<source-id>": ["<path>" or null, ...] }` | `{}` | The frame files of each image sequence, in order; `null` where the sequence has a gap. |
| `purpose` | `preview`, `preview_proxy`, `thumbnail`, `remote_edit`, `export_original`, `export_proxy` | `export_original` | What the media is read for, in the spelling the report's `purpose` has. |

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
| `bitDepth` | `8`, `10` | `10` for `prores`, else `8` | 10-bit pictures, for `hevc`, `av1`, `vp9` (profile 2) and `prores`. Refused with `INVALID_INPUT` on a host whose renderer stores 8 bits (`features["output.tenBit"]`). |
| `videoEncoder` | FFmpeg encoder name | chosen by the host | An encoder `capabilities` reports for the codec, in `encode.video` or as the GPU-resident path's. |
| `videoQuality` | a level, or `{ "bitrate", "mode"?, "quantizer"? }` | `high` | Levels are `very-low`, `low`, `medium`, `high`, `very-high`; the host turns a level into a rate for the codec, size and frame rate. `bitrate` is in bits per second, `mode` is `constant` or `variable`. |
| `audioCodec` | `aac`, `opus` | the container's | One the container offers (below). |
| `audioQuality` | a level, or `{ "bitrate" }` | `high` | |
| `loudness` | `{ "targetLufs", "truePeakCeilingDbtp"? }` | none | Normalizes the audio to an integrated loudness (-60 to 0 LUFS) under a true-peak ceiling (-20 to 0 dBTP, -1 when absent). Needs `includeAudio`. A `compose` `compile` answer's `loudness` is this object. |
| `inputPolicy` | `auto`, `gpu-required`, `cpu-readback` | `auto` | How frames reach the encoder: `auto` stays on the GPU where the host can and reads back otherwise; `gpu-required` fails with `GPU_OUTPUT_UNAVAILABLE` instead; `cpu-readback` always reads back. |
| `strategy` | `render`, `prefer-copy`, `require-copy` | `render` | `prefer-copy` copies the sources' compressed packets instead of rendering when the range shows them untouched, and renders otherwise; `require-copy` fails with `COPY_INELIGIBLE` instead. |
| `renderCache` | `{ "directory", "quotaBytes"?, "backlog"? }` | none | A directory to keep rendered frames in, so a later export of the same project, files and effects by the same host build on the same GPU takes them instead of rendering them again. `quotaBytes` is 4 GiB by default; `backlog` is `skip` (default) or `wait`. |
| `memory` | `{ "inFlightBytes"?, "decodeQueueBytes"?, "reverseCacheBytes"?, "audioWindowBytes"?, "providerSourceBytes"?, "decoderSessionBytes"?, "scale"? }` | host defaults | Byte budgets for the export. `inFlightBytes` holds the readbacks and the frames queued for the encoder, priced in the output's pixel layout (each readback maps the planes of that layout, packed on the GPU). An `inFlightBytes` the request or `ALPHA2_EXPORT_IN_FLIGHT_BYTES` set too small for one readback and two packed frames fails with `RESOURCE_LIMIT` before any output opens; a default one is raised to that least; a decoded frame set (the pictures one output frame reads) larger than `decodeQueueBytes` as it is before `scale` applies waits for the queue to empty and is held alone (`performance.decodeQueueOversizedSets` counts them); the machine-memory check below counts one such set. `decoderSessionBytes` bounds how many video decoders stay open by an estimate of each (its pictures in their decoded format, so a 10-bit stream counts twice an 8-bit one), and never refuses an export itself: a session too large for it runs with fewer decoder threads, and past it, videos take turns on fewer decoders (see `native.md`). The machine-memory check below counts one session per video read at once (at most 32) whatever `decoderSessionBytes` is. On a machine or container with less memory than the defaults assume, the budgets left at their defaults shrink together to half of what the host may use (the lower of its cgroup or job limit and the physical memory); a default `inFlightBytes` above a quarter of the GPU's memory budget (`capabilities.gpu.memory.budgetBytes`) is lowered to that quarter; the result's `memoryPolicy` reports each budget's source (`request`, `env`, `default`, `machine` or `gpu`) and the machine memory (`machine`: `{ "effective", "source", "available" }`). |

Each container offers only the codecs its mainstream players decode there; the first of each is the default:

| Container | Video | Audio |
| --- | --- | --- |
| `mp4` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `mov` | `h264`, `hevc`, `mpeg4-part2`, `prores` | `aac` |
| `mkv` | `h264`, `hevc`, `vp9`, `av1`, `mpeg4-part2` | `aac`, `opus` |
| `webm` | `vp9`, `av1` | `opus` |

An unknown container, an extension that names none or disagrees with `container`, and a codec the container does not offer are refused with `INVALID_INPUT` before anything opens, as is a codec this host's FFmpeg cannot encode. `capabilities` reports the same table as `encode.containers` (`[{ "id", "extensions", "video", "audio", "defaultVideoCodec", "defaultAudioCodec" }]`), beside the encoders this host has (`encode.video`, `encode.audio`) and the codecs it writes with alpha and at 10 bits (`encode.output`: `{ "alpha", "tenBit" }`). Matroska and WebM store each frame's time to the millisecond, so their frames sit on the frame grid only to within half a millisecond.

Every export is SDR and tagged BT.709 (primaries, transfer and matrix, limited range) in the container and in the coded stream wherever the codec has a place for it (MPEG-4 Part 2 has none). Its pixels are the renderer's picture as stored, coded with the sRGB curve, which is how a player that shows BT.709 video on an sRGB display without a curve change reads it. A source's transfer tag does not change its codes: sources tagged BT.709 and sources tagged sRGB are the same picture and export with the same codes. Other matrices and ranges are converted, BT.2020 and Display P3 primaries are converted into BT.709 (other primaries are read as BT.709), and PQ and HLG sources are tone-mapped.

The fields and the result are typed and described one by one in `@alpha2-sh/node` (`ExportProjectRequest`, `NativeExportResult`). The result reports, among others, the frames, the encoder and rate control used, how frames reached the encoder (`videoPipeline`: `readback`, `gpu-resident` or `packet-copy`), the encoders tried before the one that wrote the video (`videoEncoderFallbacks`: `{ "encoder", "hardwareAccelerated", "kind", "reason", "detail" }`, `kind` being `absent`, `busy`, `unsuitable` or `unknown`), the audio encoder that ran (`audioEncoder`), the font families the project's texts name that no supplied font has (`unmatchedFontFamilies`, sorted; a family matches any name a font's name table gives it, in any language, after NFC and lowercasing, and a text naming one that matches none draws in the fallback order; a still result has it too), the absolute path of the file written (`output`), how it was moved there (`publish`, below), what each source read (`media`) and timing (`renderMs`, `fps`, both over the whole export). `performance` splits `renderMs`: `setupMs` from the start of the export to the first turn of its frame loop, which includes `encoderProbeMs` (choosing the video encoder), `encoderOpenMs` (opening it) and `loudnessMs` (the loudness pass, 0 without one); `firstFrameMs` to frame 0 handed to the encoder; `finishMs` from the last frame handed to the encoder to the answer (the encoder's drain, the mux and the publish); and `steadyFps`, the encode rate between the completions of the 10th and the 95th percentile frames, `null` below three frames. A copy times its first and last packet written and reports `null` for the two encoder times and `steadyFps`. Among the `performance` counters: on the GPU-resident path `encoderSurfaceWaitMs` (waits for an encoder surface after a frame's sources were installed) and `encoderPlaneWaitMs` (waits for the plane pair the previous hand-off read); `sourceDownloadBytes` and `sourceDownloadMs`, decoded surfaces the renderer could not import and downloaded to the CPU instead; `encoderInputCopyBytes` and `encoderInputCopyOperations`, the copies into the encoder's input, counted on the GPU-resident path and, for a hardware encoder fed CPU frames, estimated as the frames' bytes (`encoderInputCopyEstimated: true`); and the GPU time of every eighth frame from timestamp queries, `gpuRenderMsPerFrame` (the graph) and `gpuPackMsPerFrame` (the pack into the encoder's planes or the readback), means over `gpuTimeSampledFrames`, or `gpuTimeUnavailable` saying why there are none; `gpuSubmissions` (`{ "frames", "splitFrames", "maxPerFrame", "maxPredictedMs", "overBudget" }`, `null` on the CPU renderer), how frames were cut into GPU submissions that each stay under an eighth of the GPU watchdog (a frame predicted heavy is several submissions, a single heavy pass several bands and tiles; the pixels are the same); `overBudget` counts submissions still predicted over that budget, which the host also reports with a warning line on stderr. GPU waits also show in backend-specific fields: `readbackWaitMs` on the readback path, `encoderPlaneWaitMs` on D3D12 and Vulkan, `encoderInputReadyWaitMs` on VideoToolbox. `decodeDiagnostics` lists, besides each source's damaged-input counts, a `{ "type": "layerDropped", "code": "alpha-dropped" | "extra-layers-dropped", "source", "reason" }` entry for a source whose alpha or extra HEVC layers were not decoded (HEVC's auxiliary alpha layer; VP9 alpha on a runtime without the `libvpx-vp9` decoder), whose frames decode opaque.

The host writes the file as `<stem>.a2partial.<ext>` beside `output` and moves it into place when it is complete. A partial file another running export is writing makes the request fail with `OUTPUT_IN_USE` (after waiting a quarter of a second for it), with or without `overwrite`; one left by a process that was killed is removed and created again, never reused. An `output` that is the same file as any of the request's inputs (an asset, a proxy, a frame, a resource, an effect asset or a font; compared by file identity, so a hard link counts) is refused with `INVALID_REQUEST` before any work, with or without `overwrite`. A name too long for its folder once the tag is added gives every such file the name `<stem prefix>~<8 hex digits>.<tag>.<ext>`, the digits a hash of the output's file name.

Before it renders, the host probes the output folder with two small files, `<stem>.a2probe1.<ext>` and `<stem>.a2probe2.<ext>`, and removes them: whether a file there can be rewritten at an earlier offset (an MP4's index is), and whether a file can be renamed there without replacing one. A folder where no such rename works fails the request with `IO_ERROR` before anything renders. A folder that only writes files in order (a FUSE mount such as rclone without a cache) gets the export written to a staging file in the temporary directory, which is copied into the partial file once finished; that needs as much free space in the temporary directory as the output takes.

While it is written, the output is synced to the disk after every 256 MiB, so the final sync flushes at most that much and a destination that stops answering is found while writing. A finished partial file is synced, moved into place, and its folder synced. Without `overwrite` the move never replaces a file: it is one rename that refuses an existing file (`renameat2` with `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS, `MoveFileExW` without replacing on Windows). A file system that refuses that rename (exFAT on macOS, FUSE servers) gets an empty file created at `output` in a way that fails when a file is there, and the partial renamed over it. On those file systems only, a program that removes that empty file and saves its own at `output` in the moment before the rename loses its file, and a crash in that moment leaves an empty file at `output`. On Windows, a move another program blocks by holding a file open without sharing deletion is tried again for up to 10 seconds. `publish` in the result says how the file was moved: `{ "method": "rename-no-replace" | "placeholder-rename" | "rename-replace", "stagedLocally": <bool>, "retries": <count> }`. Once the file is in place the export succeeds: a failure after it, such as an encoder whose teardown stalls or panics, is reported in `encoderTeardownFailure` (`null` otherwise) and, when it leaves a thread stuck, drains the host with `threads-abandoned`. Once the encoder is told to finish, a progress message the host cannot write no longer stops the export.

A finished render is never deleted. When it cannot be moved into place (a file appeared at `output` while the export ran, the folder refused, a program held the file past the wait), it is moved beside `output` as `<stem>.a2kept.<ext>` (or `a2kept2` to `a2kept99` when that name is taken) and the request fails with `OUTPUT_PUBLISH_FAILED`, whose message names `output`, the operating system's error and the kept file, and whose `details.keptPath` is the kept file's absolute path. The kept file is the caller's to move or remove. When even that move fails, the message says so and the file stays as the partial file, which the next export to `output` removes.

Before it renders, the host conforms the media for `export_original` (or `export_proxy` with `media: "proxies"`); an export whose media is not ready fails with `CONFORM_FAILED`, and the error's `details` is the conform report.

Before any output opens, a render whose least working set (one frame in flight, one software decoder session per video read at once at the decoder thread count the export fits, 1 to 6, one decoded frame set with every video read at once at the largest source frame, and the renderer's pictures where they are in system memory) is more than the machine can give the host now (the lower of the memory the operating system reports available and what its cgroup or job limit has left, not counting file cache) fails with `RESOURCE_LIMIT`, `failure.stage` `machine-memory` and a message naming both numbers. On Linux the host also looks at the lowest cgroup memory limit on its path (v2 or v1) every 50 ms while it renders, and stops the export with `RESOURCE_LIMIT`, `failure.stage` `memory-limit-reached`, once the working set there (the charge less the `inactive_file` and `active_file` page cache the kernel reclaims first, the figure admission uses) reaches 95 % of the limit. It keeps looking after it stopped one: of the exports running at once the newest stops first, also before it has started rendering, and the next newest when the working set stays over once that one has ended. Neither catches every allocation: a host the kernel's out-of-memory killer ends gets no response, and the clients report it as `HOST_OOM_KILLED` (class `OUT_OF_MEMORY`) when the cgroup's `oom_kill` count rose while it ran.

## `still`

Renders frames of a project to PNG images, each the frame an export of it draws there, written to files or returned in the response. One request decodes once for all its frames.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | required | As for `export`. |
| `stills` | `[{ "time": <ticks>, "output"?: "<path>" }, ...]` | required | 1 to 256 frames, each to its own file, or, without `output`, to an attachment of the response. `time` is an integer tick count from 0; the frame drawn is the one of the project's frame grid that holds it, as an export snaps its range. They are rendered in time order whatever order they are given in. Two outputs whose absolute paths are equal once composed (Unicode NFC) and lower-cased are one file, as a Windows or macOS file system sees them, and the request is `INVALID_INPUT` (`two stills write one file: <a> and <b>`); so is a still whose output turns out, when it is about to be written, to be a file an earlier still of the request wrote (a second name through a link). |
| `size` | `{ "width", "height" }` | the canvas | Renders the project at this canvas size, 1 to 16384 pixels each way; odd sizes too, since nothing is encoded. |
| `overwrite` | boolean | `false` | As for `export`, for every output. |
| `alpha` | boolean | `false` | Draws each still without the project background, its layers over transparent pixels. |
| `proxies`, `frames`, `media`, `resources`, `effectAssets`, `fonts`, `effects` | as for `export` | | |

Each PNG is 8-bit RGBA: the renderer's picture before any video encoding, display-encoded (sRGB) with straight alpha. Every frame is opaque over the project's `background`. Every output is SDR BT.709. The PNG is written beside its output as a partial file and moved into place, like an export's file, and a PNG that cannot be moved is kept the same way (`OUTPUT_PUBLISH_FAILED`). An output that is one of the request's inputs is refused with `INVALID_REQUEST`, and an empty list, more than 256 frames, two frames to one file, a negative time or a size out of range with `INVALID_INPUT`, before anything renders.

The result is `{ "stills": [{ "output", "frameTime", "width", "height", "publish" }], "renderer", "renderMs", "timings": { "decodeMs", "renderMs", "writeMs" }, "media", "decodeDiagnostics" }`, the frames in the order they were asked for. `output` is the absolute path written and `frameTime` the time of the frame drawn, in ticks. A still without an `output` has `"output": null`, no `publish`, and `attachment`, the index of its PNG among the response's attachments (type `image/png`), which follow the order of the stills; their PNGs together may be at most 4 GiB, past which the request fails with `RESOURCE_LIMIT`. A still writes no progress messages.

## `compose`

Compiles compose specs and binds templates, without a GPU and without reading media; the same functions as `alpha2.compose` in Python and `alpha2/compose` in the browser. `action` picks one:

| `action` | Fields | Result |
| --- | --- | --- |
| `compile` | `spec`, `facts` (`{}`), `fonts` (font file paths, `[]`), `templates` (`[]`) | `{project, assets, loudness, warnings}`; `loudness` is `null` or `{targetLufs, truePeakCeilingDbtp}`, as `export` takes it |
| `bind` | `template`, `data` (`{}`), `theme`, `templates` (`[]`) | `{spec}`: the template bound to its data, or a spec with its template nodes expanded |
| `dataSchema` | `template` | `{schema}`: the JSON Schema of the template's data |
| `lint` | `template`, `data`, `templates` (`[]`) | `{issues}`: `{path, code, message, severity}` each |
| `requirements` | `spec` | `{media, texts}`: the files to gather `facts` about with `conform` |
| `templates` | none | `{templates}`: the built-in `alpha2/` templates |

`facts` describes the files a spec names (`{"media": {path: {width, height, frame_rate, duration_ticks, has_audio}}, "texts": {path: text}}`; a video a spec plays unmuted needs `has_audio`). A spec or data that does not compile or bind fails with `INVALID_INPUT`, its `details` `{path, problem}` naming where in the spec, template or data (`spec.body.children[2].path`, `data.title`).

## `edit`

Applies one edit operation to a project, without a GPU and without reading media; the same function as `alpha2.edit.apply` in Python, `applyEdit` in `alpha2/apply-edit` in the browser and `applyEdit` in the mobile library. Fields: `project` and `edit`, the operation:

| `op` | Fields |
| --- | --- |
| `remove_ranges` | `ranges` (`{"data": id}` of an `intervals` data set, or `{"list": [{start, end}], "anchor": {...}}`), `target` (`{"track": id}` or `{"clips": [id]}`), `ripple` (`none`, `track`, `timeline`; default `timeline`), `invert` (`false`), `padding` (ticks, `0`), `min_duration` (ticks, `0`) |
| `split_at` | `times` (`{"data": id}`, the starts of an `intervals` data set, or `{"list": [ticks], "anchor": {...}}`), `target` |
| `bake` | `targets`: `{clip, property}`, `{composition, node, property}` or `{track, property: "captions"}`, with lint property paths |

Kept moments render as before for clip picture and sound, clip keyframes, captions, timeline-anchored data and track and master automation: split clips continue their source and time map, timeline keyframes and timeline-anchored data are cut and shifted, captions are trimmed, transitions move with their keyed parameters, and transitions whose cut is removed are removed. A track matte, or a binding read through another clip, follows the part of a cut clip beside it, and the edit is refused (`matte_source`, `read_through`) where no single part can. `split_at` leaves out a cut inside one of the clip's transitions or fades, one that would start a loop part inside a transition, and one of a matte source or a clip read through whose counterpart is not cut alike; `report.skipped` lists them. `remove_ranges` refuses the same situations instead, since a partial removal is another edit. A negative `padding` widens each range, never before time 0. `bake` writes each value clamped to the range its property takes, as rendering clamps the live value, with a key where a linear set crosses an end. Not exact in this release: parts of a reversed clip whose edges are off the frame grid, and sound at a loop wrap a cut turns into a part boundary (the wrap's 5 ms fade is not kept).

The result is `{projectJson, report}`. `projectJson` is the edited project as the engine's canonical text, the same bytes on every surface; `report` has `removed` (timeline ranges, in the old timeline's time), `moved` (`{from, to}` ranges), `created` (`{id, from}`: each new clip and the clip it was cut from) and, when `split_at` left cuts out, `skipped` (`{clip, time, reason}`, reason `in_transition`, `in_fade`, `matte_source` or `read_through`). A refusal fails with `INVALID_INPUT`, its `details` `{refusal}` naming why: `invalid_project`, `invalid_request`, `unknown_data`, `wrong_kind`, `unknown_target`, `empty_target`, `missing_anchor`, `invalid_range`, `nothing_to_bake`, `correction_not_bakeable`, `unbounded_parameter` (a bake of a host effect's parameter, whose range only the renderer knows), `matte_source` (a cut no single part of a matte source can follow), `read_through` (a cut no single part of a clip another clip reads through with `via` can follow), `in_transition` (a removed range's edge, or a loop part boundary it makes, inside a clip's transition), `in_fade` (a removed range's edge inside a clip's fade), `too_many_clips` or `too_many_cues` (a caption bake whose cues do not fit a track: more than 4,096, or words past the 65,536 cues or 200,000 mapped words that cues from words allow), or `result_too_large` (the edited project would be longer than the 64 MiB a project document may be, for example after a `bake` of long data). An edited project that fails validation is a defect and fails with `INTERNAL` (`invalid_result`).

## `extract`

Runs an extractor over a source or the timeline and answers with the data sets it made, validated as the project validates data. The extractor is the client's own, which sees the input step by step and sends its values back, or a [plugin](plugin-abi.md) the host fetches and runs itself, so frames never cross the pipe. Any language can be a client this way: `@alpha2-sh/node` has `extract(request, extractor)` and the Python client `HostClient.extract(project, request, extractor)`; both take a plugin instead of an extractor.

| Field | Type | Default | |
| --- | --- | --- | --- |
| `project` | project document | required | |
| `assets` | `{ "<source-id>": "<path>" }` | required | As for `export`. A source input reads only its own source, whether or not the timeline uses it. |
| `request` | extract request | required | `{ "extractor", "version", "params"?, "input", "outputs" }`: the descriptor's id and version, its parameters, what to read (`input.from` is `source` with `source_id`, or `timeline`; `range` in ticks on that clock; for video `size`, the longer side, and for the timeline `frame_rate`; `passes` as `[{ "start", "direction" }]`) and the data set id of each output by name. |
| `descriptor` | extractor descriptor | | For a client-side extractor: what it reads and emits, as its `describe()` returns: `id`, `version`, `input` (`media` `video` with `format` `rgba8` or `luma8`, or `audio` with `format` `f32`), `params_schema` and `outputs` (`name`, `kind`, `channels`, `interpolation`). |
| `plugin` | `{ "manifest", "sha256", "allow_native"?, "force_wasm"? }` | | Instead of `descriptor`: the plugin to run (see [Plugins](#plugins)). A request has exactly one of the two. |
| `proxies`, `frames`, `media`, `resources`, `effectAssets`, `fonts`, `effects` | as for `export` | | `made_from` is the original's fingerprint even when proxies are read. |

Any other field is refused with `INVALID_REQUEST`. This host does not supply timeline audio: an audio extractor over the timeline fails with `INVALID_INPUT`; extract from the source instead.

The host answers a request the session refuses (an unknown output, an empty range or one that starts before 0, a pass that starts outside the range, a source the descriptor cannot read, a source range that ends past the source's fingerprinted duration `fingerprint.duration_ticks`) with `INVALID_REQUEST` before anything is decoded; a range is never clamped. For a source without a fingerprinted duration a range past the end of the stream is read as silence and no frames. Then it sends steps, one at a time, and waits for the acknowledgement of each before the next:

```json
{"protocolVersion":2,"type":"extract.step","requestId":"7","seq":3,"step":"video","time":8000,"width":96,"height":54,"format":"luma8","attachments":[{"type":"application/octet-stream","bytes":5184}]}
{"protocolVersion":2,"operation":"extract.ack","requestId":"7","seq":3,"emits":[{"output":"faces","time":8000,"values":[0.41,0.37,null]}]}
```

| `step` | Fields | |
| --- | --- | --- |
| `begin` | `context`: `range`, `passes`, `from`, `video` (`size`, `format`) or `audio` (`rate`, `channels`, `blockFrames`) | Once, first. Load models here. |
| `pass` | `pass`, `direction` (`forward` or `backward`) | At the start of each pass. |
| `video` | `time`, `width`, `height`, `format`; one attachment | A frame: tightly packed `rgba8` (straight alpha, display-encoded sRGB) or `luma8`, top row first, at the displayed size scaled so its longer side is at most `size`. `time` is its presentation time in ticks. |
| `audio` | `start`, `startSample`, `frames`, `rate`, `channels`; one attachment | A block of little-endian `f32` samples, planar, channel after channel. |
| `end` | | Once, last. Whole-range work, such as speech recognition, emits here. |

An acknowledgement carries the `requestId` and `seq` of its step and either `emits` or `"error": { "message" }`. It is not a request and gets no response of its own; one for a request that has ended or for another `seq` is ignored. Each emit names an `output` of the descriptor:

- `{ "output", "time", "values": [...] }` for `points`, `scalars` and `similarity`, one finite number per channel, `null` for a channel lost at that time;
- `{ "output", "start", "end", "text", "speaker"? }` for `words`;
- `{ "output", "start", "end", "label"?, "score"? }` for `intervals`, the score finite.

Times are integer ticks on the input's clock, inside the range. The clients refuse an emit that breaks these types (a time that is not an integer, a value or score that is not finite) inside the step that made it, which fails the request with `EXTRACTOR_FAILED` naming the emit. The data limits are checked as values are emitted: an emit that takes a requested output past one ends the request with `INVALID_EMIT`, `details` naming that emit. During a pass an emit reaches no further along the pass than what it has delivered; `end` may emit anywhere in the range.

- A client that does not acknowledge a `pass`, `video` or `audio` step within `--extract-step-timeout <ms>` (a `serve` flag, 60000 by default, 0 for none) ends the request with `EXTRACTOR_TIMEOUT` and frees its place among the running requests. `begin` and `end` have no timeout.
- An emit the session refuses ends the request with `INVALID_EMIT`, `details` `{ "index", "reason" }` naming the emit in the acknowledgement and why. An acknowledgement with `error` ends it with `EXTRACTOR_FAILED` and that message.
- Progress messages have `"phase": "extract"`, the `pass`, and its `completed` and `total` deliveries; `total` is `null` for a source pass, whose frame count is not known ahead.
- A `cancel` ends the request with `CANCELLED` between steps; a client that exits ends it with the host.

Limits, each by the constant that sets it:

| Limit | Constant | Value |
| --- | --- | --- |
| Outputs per descriptor | `alpha2_engine::extract::MAX_EXTRACTOR_OUTPUTS` | 32 |
| Passes per request | `alpha2_engine::extract::MAX_EXTRACTOR_PASSES` | 64 |
| `size` when the descriptor names no `max_size` | `alpha2_engine::extract::DEFAULT_MAX_SIZE` | 960 |
| Audio `block_frames` | `alpha2_engine::extract::BLOCK_FRAMES_RANGE`, default `DEFAULT_BLOCK_FRAMES` | 64 to 65536, default 1024 |
| Data sets per project | `alpha2_model::MAX_DATA_SETS` | 256 |
| Channels per output | `alpha2_model::MAX_DATA_CHANNELS` | 64 |
| Values per sampled data set (numbers or `null`s) | `alpha2_model::MAX_DATA_VALUES` | 4,000,000 |
| Words, intervals per data set | `alpha2_model::MAX_DATA_WORDS`, `MAX_DATA_INTERVALS` | 200,000 each |
| Data set id; label, text, speaker or channel name | `alpha2_model::MAX_DATA_ID_BYTES`, `MAX_DATA_LABEL_BYTES` | 64, 256 bytes |
| Points and similarity translation; scalars and interval scores | `alpha2_model::MAX_DATA_COORDINATE`, `MAX_DATA_SCALAR` | 16, 1e9 in magnitude |
| Similarity rotation; scale | `alpha2_model::MAX_DATA_ROTATION_DEGREES`, `MAX_DATA_SCALE` | 36000 degrees, 1000 |
| `--extract-step-timeout` default | `alpha2-host` `DEFAULT_STEP_TIMEOUT` | 60 s |

The result is `{ "dataSets": [...], "timings": { "totalMs", "clientMs" } }`: one data set per output the request names in `outputs`, in the order of the output names, each anchored to the source read (`made_from` its original's fingerprint) or to the timeline, with `made_by` recording the extractor, version and params, and for a plugin `made_by.plugin` (the package `version` and the `artifact_sha256` that ran). `clientMs` is the time spent waiting for acknowledgements, 0 for a plugin. Putting the data sets in the project, replacing older ones with the same id, is the caller's edit.

### Plugins

With `plugin`, the host runs the extractor itself; no steps go to the client, and progress and `cancel` work as above. A `cancel` stops a plugin download between body chunks (connecting and waiting for a response head take at most 30 s each) and a wasm call within about 50 ms; a native call or a module compile that is running ends first, and the cancel takes effect when it returns.

- `manifest` is an `https:` URL, a `file:` URL or a path on the host's file system, and `sha256` the SHA-256 of its bytes; a manifest with another hash is refused with `PLUGIN_HASH_MISMATCH`. Each artifact and resource `url` is a path under the manifest's own directory or an absolute `https:` URL (fetched through the platform's certificate verifier and `HTTPS_PROXY` / `NO_PROXY`); a manifest with any other `url` is refused with `PLUGIN_LOAD_FAILED`. A mismatch message states the length and digest found only for a local file.
- Downloaded files are kept under their SHA-256 in `ALPHA2_PLUGIN_CACHE`, else in `plugins` under the host's cache directory (`ALPHA2_HOST_CACHE_DIR`, else `~/.cache/alpha2/plugins`, `~/Library/Caches/alpha2/plugins`, `%LOCALAPPDATA%\alpha2\plugins`). A host with neither a home directory nor `ALPHA2_PLUGIN_CACHE` does not download plugin files (`PLUGIN_FETCH_FAILED`). On Unix the directory is created with mode 0700, and one that another user owns or that group or others can write is refused. A download is streamed to a temporary file and stored under its name only once its length and SHA-256 match; every file is checked again against the hash and length its manifest pins before it is used, cached or not. When the cache holds more than `ALPHA2_PLUGIN_CACHE_QUOTA` bytes (4 GiB by default), the least recently used files are removed, except files used within the last hour.
- The host takes the native artifact for its own target when it was started with `--plugins native`, `allow_native` is true and `force_wasm` is not, else the wasm artifact. A native artifact runs with the host's rights, inside this host process: if it crashes, the host exits and the request gets no response. It is loaded from a copy of its checked bytes under `native/` in the plugin cache (a host without a cache directory refuses it with `PLUGIN_LOAD_FAILED`), once per host process, and stays loaded until the host exits. A wasm artifact is sandboxed: it has one memory, capped by `--plugin-memory-limit <bytes>` (a `serve` flag, 1 GiB by default, at most 2147483648), its tables together hold at most 1,048,576 elements, and each call is bounded by `--plugin-call-timeout <ms>` (300000 by default, measured in wall-clock time); past any of these it fails with `PLUGIN_LIMIT` and the host goes on serving. What it emits is refused at the data limits of its outputs as it is emitted (`INVALID_EMIT`).
- The artifact's descriptor must equal the manifest's. The request's `params` go to the plugin as they are: the host does not check them against `params_schema`, so the plugin must check what it reads.
- A manifest is at most `alpha2_engine::extract::MAX_MANIFEST_BYTES` (1 MiB) with at most `MAX_PLUGIN_ARTIFACTS` artifacts and `MAX_PLUGIN_RESOURCES` resources (64 each). It pins an artifact of at most `MAX_PLUGIN_ARTIFACT_BYTES` (256 MiB), a resource of at most `MAX_PLUGIN_RESOURCE_BYTES` (2 GiB) and resources of at most `MAX_PLUGIN_RESOURCES_TOTAL_BYTES` (4 GiB) together; a manifest past these is refused with `PLUGIN_LOAD_FAILED`. A local manifest must be a regular file. The `--plugin-memory-limit` and `--plugin-call-timeout` defaults are `alpha2_host_core::plugins::DEFAULT_MEMORY_BYTES` and `DEFAULT_CALL_DEADLINE`.
- Whoever starts the host decides, once, what plugins may do; a request chooses only inside that, and a manifest names files only inside its own location:

  | `serve` flag | Values | Default | Refusal |
  | --- | --- | --- | --- |
  | `--plugins` | `off`, `wasm`, `native` | `wasm` | `off`: every `plugin` request is `PLUGIN_REFUSED` before anything is read. `wasm`: `allow_native` has no effect, and a manifest with only a native artifact is `PLUGIN_NATIVE_REFUSED` |
  | `--plugin-origins` | `none`, or a comma list of `https://host[:port]` | a manifest from any `https:` origin; its files and redirects on that same origin | `none`: nothing is fetched over the network. A list: manifests, files and redirects only on listed origins, and across them. Otherwise `PLUGIN_REFUSED` naming the origin and the flag, before it is contacted |
  | `--plugin-root` | `any`, `none`, or a directory | `any` | A local manifest outside the directory (after resolving links), or any local manifest under `none`, is `PLUGIN_REFUSED` before it is read |

  A local manifest has no origin, so an `https:` file it names is fetched only from a listed origin. Redirects are followed for at most five hops, each to an `https:` URL the rule allows; a manifest's relative URLs resolve against the URL the request named. `@alpha2-sh/node` and the Python client take the policy as a constructor option, `plugins` (`mode`, `origins`, `root`), passed as these flags to every host they start.
- What a plugin can and cannot do: a wasm artifact cannot read a file, open a connection or read a clock; it sees the frames or audio of the run, its `params` and the resources its own manifest declares. Its memory and tables stay under the limits above, what it emits is refused at the data limits of its outputs, and each call ends at its deadline; past any of these the request fails with a typed error and the host goes on serving. It can use the CPU for as long as its calls may run and return any data the limits allow, including data derived from the frames it saw: its output is as trustworthy as the plugin. A native artifact has the host's rights and runs only under `--plugins native` with `allow_native`.
- The `PLUGIN_*` codes below say why a plugin did not load or run. A plugin call that fails is `EXTRACTOR_FAILED` with the plugin's message, and output the session refuses is `INVALID_EMIT`, as for a client.

## Cancelling

To cancel a request, send a `cancel` naming it in `target`:

```json
{"protocolVersion":2,"requestId":"1:cancel","operation":"cancel","target":"1"}
```

It is answered as soon as the host reads it, without waiting for running requests, with `{"protocolVersion":2,"requestId":"1:cancel","ok":true,"result":{"target":"1","state":"running"},"hostState":"ready"}`:

- `running`: `target` is the request being handled. An export or still stops, removes its partial file and is answered `CANCELLED`; `capabilities` and `conform` have nothing to stop and are answered as usual. The host goes on serving.
- `pending`: no request with that id is being handled. When a request with that id is read or starts later, it is answered `CANCELLED` without running. A cancel for a request that has already been answered is kept until 1024 newer ones push it out.

A `cancel` without a `target` is answered `INVALID_REQUEST`. The answer to a `cancel` or `status` is written after any frame the host is writing then, so a client that is slow to read a large frame also gets it late. A `cancel` or `status` header over 64 KiB is not answered at once: it waits its turn among the requests and is refused with `INVALID_REQUEST`. The host reads cancels while it holds at most 256 MiB of requests it has not started; past that it reads, and answers, a cancel only once it starts the next request.

`SIGTERM`, `SIGINT` and `SIGHUP` (a console control event on Windows) cancel the running export the same way; the process then exits, with the status the signal would have given it. An idle process exits at once. When the process that started `serve` exits, the host cancels what it runs and exits too (status 143), so a client that is killed does not leave its export running. A request that cannot stop in time does not hold the exit: the process ends anyway, without writing more, once the stall limit plus 5 seconds have passed after a signal (45 seconds by default), or 5 seconds after its parent's exit, and an export ended that way leaves its partial file. When the input ends, an extraction waiting for an acknowledgement is answered `CANCELLED`. A second signal of the same kind (on Windows, a second Ctrl+C or Ctrl+Break), or `SIGKILL`, ends the process immediately; an export ended that way leaves its partial file and gets no response.

## Errors

| Code | Meaning |
| --- | --- |
| `UNSUPPORTED_PROTOCOL` | `protocolVersion` is missing or not 2. |
| `INVALID_REQUEST` | The frame is not a request: a header that is not UTF-8 or not a JSON object, a frame too long, attachments the header does not list as they are, an unknown or missing `operation`, a missing or mistyped field, an empty `requestId`, a `cancel` without a `target`; or an output is one of the request's inputs. |
| `INVALID_INPUT` | A value is not accepted: an invalid project, range, rate or setting, or settings that do not go together. |
| `CONFORM_FAILED` | The export's media is not ready; `details` is the conform report. |
| `MISSING_INPUT` | A file the render needs was not given: an asset, an effect resource, a font or a frame. |
| `INCOMPLETE_COMPOSITION` | A composition graph has an input or output that is not connected. An export whose range shows one is refused before its first frame. |
| `EFFECT_ERROR` | A runtime effect's definition, parameters or shader failed. |
| `GPU_INIT_FAILED` | The host could not create its GPU device. |
| `HARDWARE_DECODE_UNAVAILABLE` | The decode mode is `hardware-required` and the host cannot decode in hardware. |
| `GPU_DEVICE_LOST` | The GPU device was lost, or no longer runs the work it is given. |
| `GPU_DEVICE_HUNG` | The GPU device was lost to the export's own work: a submission shortly before the loss kept the GPU busy for at least half the GPU watchdog (2000 ms, `ALPHA2_GPU_WATCHDOG_MS`), or the platform reported a reset for a command that ran too long. Submissions whose completions were seen together (one wait saw a whole frame finish) tell only their total, so such a group counts only when it also ran more than twice its predicted time; otherwise the loss stays `GPU_DEVICE_LOST` and is retried, and a loss that recurs at the same frame stops as hung. Running it on the GPU again stops the GPU again. On a GPU loss `failure.diagnostics.gpuSubmission` names the submission that made it hung, else the longest recent one: `gpuMs`, `measured` (false when a completion was seen late, so `gpuMs` is an upper bound), `submissions` (more than 1 for a group, whose `gpuMs` is their total), `watchdogMs`, `frameIndex` (the first one's), `effects`, and `predictedMs` when its frame was planned. |
| `GPU_TIMEOUT` | GPU work did not complete in time, or the request stayed in a render or readback step. |
| `DECODE_STALLED` | A decoder stopped making progress, or the request stayed in a step that reads media. |
| `ENCODE_STALLED` | The encoder or muxer stopped making progress, or the request stayed in an output step (an encoder probe or open, the publish, its answer). |
| `COPY_INELIGIBLE` | `strategy: "require-copy"` and the range cannot be copied. |
| `GPU_OUTPUT_UNAVAILABLE` | `inputPolicy: "gpu-required"` and the GPU-resident encode path cannot take the export. |
| `ENCODE_ERROR` | Encoding or muxing failed. |
| `ENCODER_BUSY` | No video encoder opened, and a hardware one refused because other exports hold its sessions; the same export can succeed once they end. |
| `IO_ERROR` | A file could not be read or written, or `output` already exists and the request does not overwrite it. |
| `OUTPUT_IN_USE` | Another running export is writing the same `output`. |
| `OUTPUT_PUBLISH_FAILED` | The output was rendered in full but could not be moved into place; `details.keptPath` names where the finished file was kept. |
| `CANCELLED` | The request was cancelled: by a `cancel`, before or while it ran, or by a termination signal. |
| `RESOURCE_LIMIT` | A budget or limit refused the work, such as a memory budget too small for the export; `failure.stage` is `machine-memory` or `memory-limit-reached` when the machine's memory refused it (see `export`). |
| `INVALID_EMIT` | An `extract` client emitted a value the session refuses; `details` is `{ "index", "reason" }`. |
| `EXTRACTOR_FAILED` | An `extract` client acknowledged a step with an `error`. |
| `EXTRACTOR_TIMEOUT` | An `extract` client did not acknowledge a step within `--extract-step-timeout`. |
| `PLUGIN_FETCH_FAILED` | A plugin's manifest, artifact or resource could not be read or downloaded. |
| `PLUGIN_HASH_MISMATCH` | A plugin file's SHA-256 or length differs from what the request or the manifest pins. |
| `PLUGIN_ABI_UNSUPPORTED` | The plugin implements an ABI this host does not. |
| `PLUGIN_NO_ARTIFACT` | The manifest has no artifact this host runs; the message lists the targets it offers. |
| `PLUGIN_NATIVE_REFUSED` | Only a native artifact runs here, and the request does not allow native code or the host was not started with `--plugins native`. |
| `PLUGIN_REFUSED` | The host's plugin policy (`--plugins`, `--plugin-origins`, `--plugin-root`) refuses the plugin or one of its files. |
| `PLUGIN_DESCRIPTOR_MISMATCH` | The artifact describes another extractor than the manifest. |
| `PLUGIN_UNSUPPORTED_FEATURE` | The wasm module uses relaxed SIMD, threads, shared memory, more than one memory or 64-bit memory, or imports something other than the host functions. |
| `PLUGIN_LOAD_FAILED` | The manifest is not one (including a `url` outside its directory), or the artifact did not load (including a native table shorter than this host's or with a null entry). |
| `PLUGIN_TRAPPED` | The wasm module trapped, or the native plugin caught a panic at its boundary. |
| `PLUGIN_LIMIT` | A wasm plugin ran past its memory cap or table bound, or a call past its deadline, or a native plugin call did not return within three stall limits. |
| `HOST_RETIRING` | The host is `draining` and did not start the request; send it to another host. |
| `INTERNAL` | A defect in the host or the engine failed the request. When the request panicked, the host drains with reason `defect`: requests running finish, and those it has read but not started and later ones are answered `HOST_RETIRING`. |
| `ALPHA2_ERROR` | A failure no other code describes. |

The codes the engine itself reports (`INVALID_INPUT`, `MISSING_INPUT`, `INCOMPLETE_COMPOSITION`, `EFFECT_ERROR`, `ENCODE_ERROR`, `GPU_DEVICE_LOST`, `GPU_DEVICE_HUNG`, `GPU_TIMEOUT`, `INTERNAL`, `ALPHA2_ERROR`) are the ones the browser engine reports too.

An `export` that fails, before or while it renders, also has `error.failure`: the class of the failure (`code`, such as `OUT_OF_MEMORY`, `DEVICE_LOST`, `INVALID_INPUT` or `CANCELLED`), the step it failed at (`phase`, `stage`) and the frame (`frameIndex`), the protocol code (`hostCode`), and the device, memory and path diagnostics recorded up to then (`diagnostics`). It reports the first failure, not the cancellations that failure caused in the rest of the export. A `RESOURCE_BUSY` failure (`ENCODER_BUSY`) also has `retryAfterMs`, how long to wait before trying again. The type is `ExportFailure` in `alpha2/export-failure`, and `alpha2/export-retry` decides from it whether to retry. A caught defect (`INTERNAL`) has the class `DEFECT`, which it retries once, at once, on a fresh host.

## Security

[Security](security.md) describes what alpha2 treats as hostile and how to run a host for input or senders it does not trust.

Runtime effects are WGSL that runs on the GPU. A host that takes requests from other users or over a network must decide who may send effect definitions before it passes them on: the engine checks a definition, not who sent it. `assets`, `output` and every other path are paths on the host's file system; moving files to and from the host is the caller's job.

A request can also make the host fetch an `https:` URL (a plugin manifest and the files it names, unless `--plugin-origins` or `--plugins off` forbids it), read a local file of at most 1 MiB as a plugin manifest and learn its SHA-256 (unless `--plugin-root` or `--plugins off` forbids it), run native code inside the host process (a plugin with `allow_native`, on a host started with `--plugins native`), write and evict files under `renderCache.directory`, replace a file at `output` (`overwrite`), and learn whether a path exists (`conform` reports a missing file). Error messages, `failure.diagnostics` and a client's standard error tail carry host paths and device details: they are for the operator, not for whoever sent the request. A host that takes requests from senders it does not trust sets `--plugins off`, or `--plugin-origins` and `--plugin-root`.
