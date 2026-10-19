# @alpha2-sh/node

A Node.js client for `alpha2-host`, the native alpha2 renderer: it exports alpha2 projects to MP4 files from Node.js or an Electron main process, with FFmpeg for decoding and encoding and the GPU for rendering.

## Install

```sh
npm install @alpha2-sh/node
```

The package does not include `alpha2-host`. Download the native archive for the platform from the [GitHub Releases page](https://github.com/alpha2-sh/alpha2/releases) and keep its `bin/` and `lib/` directories together; [native archives](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md) describes them. The client looks for `alpha2-host` (`alpha2-host.exe` on Windows) in this order:

1. the `hostPath` option;
2. the `ALPHA2_HOST_PATH` environment variable (a relative path is relative to the `cwd` option, else the working directory);
3. in Electron, `process.resourcesPath/bin/`, then `process.resourcesPath/`;
4. this package's own `bin/` directory;
5. the directories on `PATH`.

When none has it, the client throws an `Alpha2HostError` with code `HOST_NOT_FOUND`.

Node.js 20 or later.

## Export a project

```js
import { Alpha2NodeClient } from "@alpha2-sh/node";

const client = new Alpha2NodeClient({ hostPath: "/opt/alpha2/bin/alpha2-host" });
const output = await client.exportProject({
  project, // an alpha2 project document
  assets: { "image-1": "/media/image.png" }, // the file of each source
  startTicks: 0,
  endTicks: 120_000, // ticks: 120,000 a second
  frameRate: { num: 30, den: 1 },
  videoQuality: "high",
  output: "/exports/tutorial.mp4",
  onProgress: ({ phase, completed, total }) => console.log(phase, completed, total),
});
```

Each export runs in its own `alpha2-host` process. The project is checked with `validateProject` from `alpha2/project` before a process starts; an invalid one is refused with an `Alpha2ProjectValidationError` listing the problems. The output must not exist yet: the host writes `<stem>.alpha2-partial.<ext>` beside it and renames it into place when it is complete. `exportProject` resolves with the output path once the file is complete; `onResult` receives what the host reports about the export (frames, encoder, rate control, timing, which media each source read).

The request's fields are typed and described in `ExportProjectRequest`; the [host protocol](https://github.com/alpha2-sh/alpha2/blob/main/docs/host-protocol.md) lists them with their defaults. Among them: `videoCodec` (`h264` by default; `hevc`, `vp9`, `av1`, `mpeg4-part2`), `videoQuality` and `audioQuality` (a level, `very-low` to `very-high`, `high` by default, which the host turns into a rate for the codec, size and frame rate, or an explicit bitrate), `includeAudio`, `loudness`, `fonts`, `effectAssets`, `proxies` with `media: "proxies"`, `renderCache` and `memory`.

## Capabilities

`client.getCapabilities()` reports what the host can decode and encode. An export without `videoCodec` gets H.264, and a host that cannot encode it refuses the export instead of writing another codec. A host encodes H.264 either with an encoder listed in `encode.video` or through its GPU-resident path, `encode.gpuOutputPath`, whose encoder takes frames on the GPU only (on Linux, `h264_vulkan`) and so is not in `encode.video`:

```js
const { encode } = (await client.getCapabilities()).capabilities;
const h264 =
  encode.video.some((encoder) => encoder.id === "h264") ||
  (encode.gpuOutputPath.codecs ?? []).some((codec) => codec.videoCodec === "h264");
```

The first `getCapabilities` call for a decode mode starts a host; calls made while it runs share it, and its answer serves later calls for that mode.

`videoDecode` (`software`, `auto`, `hardware-required`) chooses how the host decodes video, as a client option or per export; by default the host decides, `auto` on Linux and `software` on macOS and Windows. Under `auto`, an export whose hardware decoder stalls runs once more decoding in software.

## Errors

Every rejection is an `Alpha2HostError` with a `code`, the protocol's error codes and the client's own (`HOST_NOT_FOUND`, `HOST_EXITED`, `HOST_LOAD_FAILED`, `ABORTED`, and others). Its `failure` classifies it for a retry policy (`alpha2/export-failure`, `alpha2/export-retry`): the step and frame it failed at, and whether it is evidence of a lost device, running out of memory or bad input. `stderr` holds the end of the host's standard error. Subclasses:

- `Alpha2ProjectValidationError`: the project is invalid (`problems`).
- `Alpha2ConformError`: the media is not ready, so nothing was rendered (`report` says, per source, what is missing, offline or not the media the project recorded). `client.conform({ project, assets })` asks the same question without exporting.
- `Alpha2GpuInitializationError`: the host could not create its GPU device.
- `Alpha2AbortError`: the request's `signal` aborted it.

## Cancelling

`signal` (an `AbortSignal`) cancels an export. The host stops, removes its partial file and exits, and `exportProject` rejects with `Alpha2AbortError` once the process is gone. On Windows the client ends the host process at once, which leaves the partial file; the next export to the same `output` takes it over.

## Concurrency

`concurrency` (default 1) is how many host processes a client runs at once, and so how many exports share the GPU; more requests wait for a slot. The module-level `exportProject(request, options)` and `getCapabilities(options)` share one client per host binary and `concurrency`.

## Electron

Ship the archive's `bin/` and `lib/` outside the ASAR archive, for example as `bin/` and `lib/` in the application's resources directory (`process.resourcesPath`), where the client finds the host in an Electron main process. A host path inside `app.asar` is read from `app.asar.unpacked` instead.

## License

MIT, copyright Alpha2 contributors. The native archive's FFmpeg is under the LGPL; see [native archives](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md).
