# alpha2 LGPL FFmpeg 8 runtime

alpha2 does not track FFmpeg archives in Git. `manifest.toml` is the lock file for the generic shared-SDK release used to build and package native alpha2 releases. It pins the release tag, every platform archive and report hash, FFmpeg source and signature identity, seven-library ABI, license policy, and supported OS floors. `patches` lists the upstream FFmpeg commits the SDK applies to `source_tag`, in order; each patch file is an asset of the pinned release and part of the corresponding source, and the runtime still reports `source_tag`.

The source of truth for FFmpeg binaries is the `release_tag` release of the independent GitHub repository named by `repository` in `manifest.toml`, the only place that names it; the fetch script, the archives' source record and the release notes all read it from there. `scripts/fetch-ffmpeg-sdk.sh` downloads and verifies exactly the locked asset into `target/ffmpeg-sdk-cache/`, which is never committed.

## What enforces this, and where

`verify-runtime.sh` **reads the policy out of `manifest.toml`** and holds a downloaded or packaged runtime to it: the required and forbidden configure options, the seven-library ABI for the target, and the notices that must ship beside it. `scripts/fetch-ffmpeg-sdk.sh` runs it on every SDK download and `scripts/build-native-release.sh` runs it again on the packaged archive. It is the only enforcement, so editing the lock file changes what is enforced.

## Local use

```sh
# Prints the extracted SDK directory. The target may be omitted for the host.
scripts/fetch-ffmpeg-sdk.sh linux-x64

# Build the self-contained (bundled) archive, the only kind released.
scripts/build-native-release.sh bundled linux-x64

# Name the archive a target releases, without building.
ALPHA2_RELEASE_PLAN=1 scripts/build-native-release.sh bundled win-x64
```

There is no archive that loads a system FFmpeg: no supported system ships FFmpeg's ABI 62 (Homebrew is at 63, and no LTS Linux distribution has it), and on Windows such a program would find the FFmpeg DLLs through the default search order, where the current folder comes before `PATH`. A host for a system FFmpeg 8 is built from source (`cargo install alpha2-host`).

The bundled archive contains the seven locked LGPL shared libraries and uses a private runtime directory:

- Linux: `bin/alpha2` uses `$ORIGIN/../lib` RUNPATH.
- macOS: `bin/alpha2` uses `@executable_path/../lib`, and every FFmpeg install name, in the executables and in the dylibs, is `@rpath/`; release signing must cover the dylibs before signing the executable/archive.
- Windows: the seven DLLs are placed beside `alpha2.exe` in `bin/`, the application folder Windows searches first.

`smoke-archive.sh` holds every bundled archive to that. It unpacks the archive into a fresh directory and starts both executables from there with no loader environment (`LD_LIBRARY_PATH`, `DYLD_*` unset; on Linux also without the `ld.so` cache, and with every FFmpeg library required to resolve inside the archive; on macOS after asserting the `@rpath/` install names; on Windows with only the Windows directories on `PATH`). A smoke run that sets the loader path proves only that the binaries run once told where the libraries are: that path is searched before RUNPATH, also for the libraries' own dependencies, which the executables' RUNPATH does not cover.

Headers, pkg-config files, and import libraries are build inputs only. They stay in the generic SDK release and are not copied into an alpha2 runtime archive.

Every native archive carries the exact lock file, platform configure/runtime audit, alpha2 third-party notices, FFmpeg, zlib, dav1d, libopus, libvpx and SVT-AV1 notices (and, on Linux, Vulkan-Headers), and a source/provenance record. Consumers may replace the bundled libraries with ABI-compatible FFmpeg 8 builds. Patent and codec distribution rules remain the distributor's responsibility even when the software license is LGPL.
