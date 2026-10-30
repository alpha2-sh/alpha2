# @alpha2-sh/node

A Node.js client for `alpha2-host`, the native alpha2 renderer: it exports alpha2 projects to MP4, MOV, Matroska or WebM files from Node.js or an Electron main process, with FFmpeg for decoding and encoding and the GPU for rendering.

## Install

```sh
npm install @alpha2-sh/node
```

The package does not include `alpha2-host`. Download it for this package's version and platform:

```sh
npx @alpha2-sh/node install
```

This fetches the platform's `bundled` [native archive](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md) from the [GitHub release](https://github.com/alpha2-sh/alpha2/releases) of the same version, checks it against the release's `checksums.sha256`, and unpacks it into the cache: `ALPHA2_HOST_CACHE_DIR` when it is set, else `alpha2` in `$XDG_CACHE_HOME` or `~/.cache` on Linux, `~/Library/Caches` on macOS and `%LOCALAPPDATA%` on Windows. `--thin` fetches the archive that loads the system's FFmpeg 8 instead (not on Windows), and `--force` downloads again. `installHost(options)` does the same from code. `ALPHA2_HOST_DOWNLOAD_URL` (or the `downloadUrl` option) names a mirror laid out like `https://github.com/alpha2-sh/alpha2/releases/download`. Archives are published for `linux-x64`, `linux-arm64`, `macos-arm64` and `win-x64`.

The client looks for `alpha2-host` (`alpha2-host.exe` on Windows) in this order:

1. the `hostPath` option;
2. the `ALPHA2_HOST_PATH` environment variable (a relative path is relative to the `cwd` option, else the working directory);
3. in Electron, `process.resourcesPath/bin/`, then `process.resourcesPath/`;
4. this package's own `bin/` directory;
5. the archive of this package's version in the cache (the `cacheDirectory` option, else the directory above), `bundled` before `thin`;
6. the directories on `PATH`.

When none has it, the client throws an `Alpha2HostError` with code `HOST_NOT_FOUND`. `installHost` rejects with an `Alpha2HostInstallError` instead, whose `code` is `UNSUPPORTED_PLATFORM`, `DOWNLOAD_FAILED`, `CHECKSUM_MISMATCH`, `EXTRACT_FAILED` or `ABORTED`.

Node.js 20 or later.

## Export a project

```js
import { Alpha2NodeClient } from "@alpha2-sh/node";

const client = new Alpha2NodeClient(); // finds the host `npx @alpha2-sh/node install` put in the cache
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

Each export runs in its own `alpha2-host` process. The project is checked with `validateProject` from `alpha2/project` before a process starts; an invalid one is refused with an `Alpha2ProjectValidationError` listing the problems. An existing output is refused unless the request sets `overwrite: true`, and an output that is one of the request's inputs is always refused: the host writes `<stem>.alpha2-partial.<ext>` beside it and renames it into place when it is complete. `exportProject` resolves with the output path once the file is complete; `onResult` receives what the host reports about the export (frames, encoder, rate control, timing, which media each source read).

The request's fields are typed and described in `ExportProjectRequest`; the [host protocol](https://github.com/alpha2-sh/alpha2/blob/main/docs/host-protocol.md) lists them with their defaults. Among them: `container` (`mp4`, `mov`, `mkv` or `webm`; by default the one the output's extension names), `videoCodec` and `audioCodec` (by default the container's: `h264` and `aac`, or `vp9` and `opus` for WebM; `hevc`, `av1`, `mpeg4-part2` and `prores` (MOV) are the other video codecs, and each container offers only some, as `encode.containers` lists), `alpha` and `bitDepth` (transparency with VP9 in WebM or ProRes 4444, and 10-bit pictures), `videoQuality` and `audioQuality` (a level, `very-low` to `very-high`, `high` by default, which the host turns into a rate for the codec, size and frame rate, or an explicit bitrate), `includeAudio`, `loudness`, `fonts`, `effectAssets`, `proxies` with `media: "proxies"`, `renderCache` and `memory`.

## Capabilities

`client.getCapabilities()` reports what the host can decode and encode, and `encode.containers` which codecs each container offers. An MP4, MOV or Matroska export without `videoCodec` gets H.264, and a host that cannot encode it refuses the export instead of writing another codec. A host encodes H.264 either with an encoder listed in `encode.video` or through its GPU-resident path, `encode.gpuOutputPath`, whose encoder takes frames on the GPU only (on Linux, `h264_vulkan`) and so is not in `encode.video`. `encode.h264` sums this up: `available` (every export can), `gpu-only` (only exports the GPU-resident path takes) or `unavailable`, with a `reason` and a `remedy`:

```js
const { encode } = (await client.getCapabilities()).capabilities;
if (encode.h264.status !== "available") console.warn(encode.h264.reason, encode.h264.remedy);
```

The first `getCapabilities` call for a decode mode and renderer starts a host; calls made while it runs share it, and its answer serves later calls for those modes.

`videoDecode` (`software`, `auto`, `hardware-required`) chooses how the host decodes video, as a client option or per export; by default the host decides, `auto` on Linux and `software` on macOS and Windows. Under `auto`, an export whose hardware decoder stalls runs once more decoding in software.

`renderer` (`auto`, `gpu`, `cpu`) chooses what draws the frames, as a client option or per export; unset, the host reads `ALPHA2_RENDERER`, else uses `auto`. `auto` renders on a hardware GPU when there is one and on the CPU otherwise, never on a software GPU adapter (lavapipe, llvmpipe, WARP, SwiftShader); `gpu` takes whatever adapter the host finds and fails with `GPU_INIT_FAILED` without one; `cpu` opens no graphics driver. Results and capabilities report `renderer` (`gpu` or `cpu`), `rendererReason` (why the host renders on the CPU) and, on the CPU, `rendererThreads`; `capabilities.features` has `renderer.gpu`, `renderer.cpu` and `effects.runtime`. A runtime WGSL effect cannot run on the CPU renderer: such an export fails with `EFFECT_ERROR`.

## Software H.264 (OpenH264)

Where no hardware H.264 encoder is available, `alpha2-host` can encode H.264 in software with Cisco's OpenH264 library, which it loads at run time. Neither this package nor the native archives include that library, and nothing downloads it unless you ask: Cisco's patent licence covers its binary only when the user fetches it from Cisco.

```sh
npx @alpha2-sh/node install --openh264
```

This downloads Cisco's OpenH264 2.7.0 binary for the platform from `https://ciscobinary.openh264.org`, checks the download and the library in it against their SHA-256s, and puts the library in the cache beside the host archives, in `openh264-2.7.0-<target>/`, where `alpha2-host` finds it. It installs only the library, not `alpha2-host`. `--force` downloads again, `installOpenH264(options)` does the same from code, and `ALPHA2_OPENH264_DOWNLOAD_URL` (or the `downloadUrl` option) names a mirror. Every run prints Cisco's notice:

```text
OpenH264 Video Codec provided by Cisco Systems, Inc.
```

The library is Cisco's binary, under [Cisco's terms](https://www.openh264.org/BINARY_LICENSE.txt). Set `ALPHA2_OPENH264=0` to keep `alpha2-host` from using it. `encode.openh264` in the capabilities says whether the host loaded it, and an export that used it names it as `videoEncoder: "openh264"` with `videoEncoderLibrary`.

## Errors

Every rejection is an `Alpha2HostError` with a `code`, the protocol's error codes and the client's own (`HOST_NOT_FOUND`, `HOST_EXITED`, `HOST_LOAD_FAILED`, `ABORTED`, and others). Its `failure` classifies it for a retry policy (`alpha2/export-failure`, `alpha2/export-retry`): the step and frame it failed at, and whether it is evidence of a lost device, running out of memory or bad input. `stderr` holds the end of the host's standard error. Subclasses:

- `Alpha2ProjectValidationError`: the project is invalid (`problems`).
- `Alpha2ConformError`: the media is not ready, so nothing was rendered (`report` says, per source, what is missing, offline or not the media the project recorded). `client.conform({ project, assets })` asks the same question without exporting.
- `Alpha2GpuInitializationError`: the host could not create its GPU device (only with `renderer: "gpu"`).
- `Alpha2AbortError`: the request's `signal` aborted it.

## Cancelling

`signal` (an `AbortSignal`) cancels an export. The host stops, removes its partial file and exits, and `exportProject` rejects with `Alpha2AbortError` once the process is gone. On Windows the client ends the host process at once, which leaves the partial file; the next export to the same `output` takes it over.

## Concurrency

`client.still({ project, assets, time, output })` renders one frame to a PNG, the frame an export draws at that time (the frame of the project's grid that holds it, `stills[0].frameTime` in the result), without encoding a video; `client.stills({ ..., stills: [{ time, output }, ...] })` renders up to 256 with one host. `time` is in ticks.

`concurrency` (default 1) is how many host processes a client runs at once, and so how many exports share the GPU; more requests wait for a slot. The module-level `exportProject(request, options)`, `still`, `stills` and `getCapabilities(options)` share one client per host binary and `concurrency`.

## Electron

Ship the archive's `bin/` and `lib/` outside the ASAR archive, for example as `bin/` and `lib/` in the application's resources directory (`process.resourcesPath`), where the client finds the host in an Electron main process. A host path inside `app.asar` is read from `app.asar.unpacked` instead.

## License

MIT, copyright Alpha2 contributors. The native archive's FFmpeg is under the LGPL; see [native archives](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md).
