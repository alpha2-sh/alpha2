# alpha2-host

`alpha2-host` is the native alpha2 renderer: a program that renders alpha2 projects on the GPU and encodes them to MP4 files with FFmpeg 8. It exports one project from the command line (`alpha2-host export`), or serves export requests over standard input and output (`alpha2-host serve`), which is how [`@alpha2-sh/node`](https://www.npmjs.com/package/@alpha2-sh/node) and programs in other languages use it.

- [Native archives and commands](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md)
- [Host protocol](https://github.com/alpha2-sh/alpha2/blob/main/docs/host-protocol.md)

## Installing

The native archives on the [GitHub Releases page](https://github.com/alpha2-sh/alpha2/releases) carry `alpha2-host` with the FFmpeg runtime alpha2 is released with.

`cargo install alpha2-host` builds it against the FFmpeg 8 on the machine instead, found with pkg-config (or `FFMPEG_PKG_CONFIG_PATH`; on Windows `FFMPEG_LIBS_DIR` and `FFMPEG_INCLUDE_DIR`), and the program loads that FFmpeg's shared libraries when it runs.

## License

MIT, copyright Alpha2 contributors. FFmpeg is under the LGPL.
