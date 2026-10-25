# Native archives and commands

alpha2's native programs come as archives on the [GitHub Releases page](https://github.com/alpha2-sh/alpha2/releases): `alpha2-host`, which renders and exports projects, and `alpha2`, a media command. Both use FFmpeg 8 to decode and encode. `@alpha2-sh/node` runs `alpha2-host` from an archive; other languages talk to it over its [stdio protocol](host-protocol.md).

## Archives

Each release has an archive per target and kind, `alpha2-<version>-<target>-<kind>.tar.gz` (`.zip` on Windows), and `checksums.sha256` with the SHA-256 of every archive.

| Target | Kinds |
| --- | --- |
| `linux-x64` | `bundled`, `thin` |
| `linux-arm64` | `bundled`, `thin` |
| `macos-arm64` | `bundled`, `thin` |
| `win-x64` | `bundled` |

- **`bundled`** carries the LGPL FFmpeg shared libraries (`libavcodec`, `libavdevice`, `libavfilter`, `libavformat`, `libavutil`, `libswresample`, `libswscale`) and loads them from the archive: from `lib/` on Linux and macOS, from `bin/` beside the programs on Windows.
- **`thin`** carries no FFmpeg and loads an FFmpeg 8 installed on the system: on Linux from the system loader's search path, on macOS from `/opt/homebrew/lib`, `/usr/local/lib` or `/opt/local/lib`, in that order.

An archive unpacks into one directory:

```
alpha2-<version>-<target>-<kind>/
  bin/alpha2          the media command (alpha2.exe on Windows)
  bin/alpha2-host     the project renderer (alpha2-host.exe on Windows)
  lib/                the FFmpeg libraries (bundled, Linux and macOS)
  compliance/         FFmpeg runtime lock, build configuration, source location, notices
  licenses/           license texts of FFmpeg, the libraries it is built with, and the Rust crates
  LICENSE.md
```

Keep `bin/` and `lib/` together when the archive is moved or packaged into an application: the programs find the libraries relative to themselves.

### The host cache

`npx @alpha2-sh/node install` downloads the archive of the package's own version for the platform, checks it against `checksums.sha256`, and unpacks it into a cache that clients share. A client for another language that installs the host should use the same layout, so one download serves both:

- The cache is `ALPHA2_HOST_CACHE_DIR` when it is set; else `alpha2` in `$XDG_CACHE_HOME` (when it is an absolute path) or `~/.cache` on Linux, in `~/Library/Caches` on macOS, and in `%LOCALAPPDATA%` on Windows.
- Each archive is unpacked under its own name, `<cache>/alpha2-<version>-<target>-<kind>/`, and appears there whole: it is unpacked beside it and renamed into place. A client looks for `bin/alpha2-host` of its own version, `bundled` before `thin`.
- The archives come from `<base>/v<version>/<archive>` with `<base>` `https://github.com/alpha2-sh/alpha2/releases/download`, or `ALPHA2_HOST_DOWNLOAD_URL`, a mirror with the same layout.

## `alpha2-host`

`alpha2-host` renders a project on the GPU and encodes it to MP4, MOV, Matroska or WebM. Run it without arguments to print every flag.

- `alpha2-host export` exports one project and prints the result as JSON on standard output; progress goes to standard error.

  ```sh
  alpha2-host export --project project.json --asset image-1=image.png --out tutorial.mp4
  ```

  Each `--asset <source-id>=<path>` gives the file of one source of the project. The output must not exist yet: the export writes `<stem>.alpha2-partial.<ext>` beside it and renames that into place when it is finished. `--start-ticks` and `--end-ticks` choose the range (the whole timeline by default), `--frame-rate <num>/<den>` the output rate (the project's by default), the output's extension (`.mp4`, `.mov`, `.mkv`, `.webm`) or `--container` the container, `--video-codec` and `--audio-codec` the codecs (the container's by default: H.264 and AAC, or VP9 and Opus for WebM), `--video-quality` a level (`very-low`, `low`, `medium`, `high`, `very-high`) or a bitrate in bits per second, and `--no-audio` leaves the audio out.
- `alpha2-host conform` checks a project's media without rendering: it prints what each source would read and what is missing or unusable, and exits with status 1 when something blocks the export.
- `alpha2-host serve` reads requests on standard input and answers on standard output, one JSON object per line: the [host protocol](host-protocol.md).

### Hardware decoding and encoding

`alpha2-host` decodes video in one of three modes, chosen when it starts: `software` decodes every frame on the CPU; `auto` decodes on the GPU where the device and the stream allow it and on the CPU otherwise; `hardware-required` fails instead of decoding a frame on the CPU. The default is `auto` on Linux and `software` on macOS and Windows. `export` and `conform` take `--video-decode <mode>`; `@alpha2-sh/node` takes the `videoDecode` option.

An export hands its frames to the encoder either by reading them back from the GPU or, where the host has a GPU-resident encode path that takes the codec and the output size, without leaving the GPU. The `capabilities` request reports the decode mode and the hardware decoders (`decode`), and the encoders and the GPU-resident path (`encode`); every export result reports the decode mode it ran (`videoDecodeMode`) and the output path it took (`videoOutputPath`).

### Encoders

The pinned FFmpeg runtime encodes on the CPU with `libvpx-vp9` (VP9), `libsvtav1` (AV1), `mpeg4` (MPEG-4 Part 2), `aac` and `libopus` (Opus), so a host without a GPU encoder still writes VP9 or AV1 in MP4, Matroska or WebM. H.264 and HEVC come from the platform's hardware encoders: Vulkan Video on Linux, VideoToolbox on macOS, Media Foundation and D3D12 Video on Windows. VP9 and AV1 run at one fixed speed setting each, so the quality level chooses only the rate; software AV1 is slow at 4K.

## `alpha2`

`alpha2` works on one media file at a time. Run it without arguments to print every command.

| Command | What it does |
| --- | --- |
| `alpha2 probe <media>` | Prints the container, its duration and its streams (codec, size, frame rate, color). |
| `alpha2 thumbnail <media> <out.png> [--at <seconds>]` | Writes one frame as PNG: the frame at a time on the source's own timeline, or by default the sharpest of nine frames spread over the video that is neither black nor white. |
| `alpha2 transcode <media> <out.mp4\|.mov\|.mkv\|.webm> [--proxy half\|quarter] [--video-codec <id>]` | Encodes the video again at its own size, or at half or a quarter of it for a proxy, into the container the output's extension names. Without `--video-codec` it uses the container's default codec (H.264, or VP9 for WebM) when the runtime has an encoder for it that takes frames from memory, and otherwise another codec the runtime encodes that the container offers; it prints which. |
| `alpha2 export-audio <media> <out.wav>` | Decodes the audio, resampled to 48 kHz, into a WAV file that starts at the source's time zero. |
| `alpha2 waveform <media> [bucket-ms] [levels]` | Prints a waveform summary as JSON: the minimum, maximum and RMS of each bucket of samples (10 ms by default), at that size and at coarser levels each four times the last (6 levels by default). |

The commands that decode take `--decode-policy strict|tolerant`: `strict` (the default) fails at the first damaged input, `tolerant` skips damaged input and reports what it skipped when it ends.

## Installing from crates.io

`cargo install alpha2-host` (or `alpha2-native`, which installs `alpha2`) builds the programs against the FFmpeg on the machine that installs them, and the programs load that FFmpeg when they run:

- The build finds FFmpeg with pkg-config; `FFMPEG_PKG_CONFIG_PATH` names the `lib/pkgconfig` directory of another FFmpeg. On Windows it needs `FFMPEG_LIBS_DIR` and `FFMPEG_INCLUDE_DIR`, and `FFMPEG_LINK_MODE=dynamic`: without it the build links those libraries statically.
- The FFmpeg must be version 8 (libavutil 60, libavcodec 62).
- The programs link FFmpeg's shared libraries. On Linux the system loader must find them, on macOS they are found where pkg-config said they were, and on Windows their DLLs must be on `PATH`.

The archives are the way to get the programs with the FFmpeg runtime alpha2 is released with.

## FFmpeg and the LGPL

The FFmpeg in the archives is FFmpeg 8 built as shared libraries under the LGPL 2.1 or later, with its GPL and nonfree components disabled. alpha2 links it dynamically, and the libraries of a `bundled` archive can be replaced with an ABI-compatible FFmpeg 8 build.

Each archive's `compliance/` directory holds:

- `SOURCE.md`: where to get the corresponding FFmpeg source and the patches applied to it.
- `FFMPEG-RUNTIME.toml`: the lock file of the FFmpeg runtime the archive was built with (version, source checksum, build configuration, library names, checksums).
- the FFmpeg build's configuration report, the dynamic libraries each program links (`DYNAMIC-LIBRARIES.txt`), the Rust toolchain (`TOOLCHAIN.txt`), and alpha2's third-party notices (`THIRD_PARTY_NOTICES.md`).
