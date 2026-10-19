# alpha2-native

Native media input and output for alpha2, on FFmpeg 8: probing, decoding video and audio (on the GPU where the `hwdecode` feature and the device allow), and encoding and muxing MP4. Its types are alpha2's own (`alpha2-media`); no FFmpeg type is part of its API. The `timeline` feature adds mixing a project's audio, and `hwencode` encoding frames the renderer packs on the GPU.

The crate also builds `alpha2`, a command for one media file at a time: `probe`, `thumbnail`, `transcode`, `export-audio` and `waveform`.

- [Native archives and commands](https://github.com/alpha2-sh/alpha2/blob/main/docs/native.md)
- [`alpha2`, the engine for Rust programs](https://crates.io/crates/alpha2)

## Building

The crate links the FFmpeg 8 on the machine that builds it, found with pkg-config (or `FFMPEG_PKG_CONFIG_PATH`; on Windows `FFMPEG_LIBS_DIR` and `FFMPEG_INCLUDE_DIR`), and a program built with it loads that FFmpeg's shared libraries when it runs. The native archives on the [GitHub Releases page](https://github.com/alpha2-sh/alpha2/releases) carry the `alpha2` command with the FFmpeg runtime alpha2 is released with.

## License

MIT, copyright Alpha2 contributors. FFmpeg is under the LGPL.
