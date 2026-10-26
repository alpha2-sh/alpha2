# alpha2 for Python

Build, validate and render [alpha2](https://github.com/alpha2-sh/alpha2) project documents from
Python. A project is a plain `dict` in the engine's `project.v1` shape (typed as
`alpha2.types.Project`). Validation runs the engine's own validator in-process; rendering runs the
native `alpha2-host` process on the GPU.

```sh
pip install alpha2
```

On Linux x86_64 and aarch64, macOS arm64 and Windows x64 this also installs `alpha2-host`, the
native renderer, as a platform wheel of the same version. Elsewhere, or where that wheel cannot be
installed (a Linux older than its glibc baseline), see [Finding `alpha2-host`](#finding-alpha2-host).

```python
import alpha2

project = alpha2.ProjectBuilder("Intro", width=1280, height=720, frame_rate=30)
title = project.source("image", "title.png")
project.video_track().clip(title, at=0, duration=3)

result = alpha2.export(
    project.build(),
    "intro.mp4",
    assets=project.assets(),
    on_progress=lambda p: print(f"{p.completed}/{p.total}"),
)
print(result["videoOutputPath"], result["frames"])
```

- `alpha2.export`, `alpha2.conform` and `alpha2.capabilities` run on a shared `HostClient`; make
  your own (`alpha2.HostClient(concurrency=2)`) to bound or separate them.
- Setting a `cancel` event (`threading.Event`) or pressing Ctrl-C ends the host, which removes its
  partial output, before the call returns.
- Every failure is an `alpha2.Alpha2HostError` with a machine-readable `code` and a classified
  `failure` dict for retry policies.
- Options you write use snake_case (`loudness={"target_lufs": -14, ...}`); what the host reports
  keeps the protocol's camelCase keys (`result["effectiveRange"]`).

## Compose: videos by intent

`alpha2.compose` describes a video by what it is made of and compiles it into a project. Times are
seconds, lengths are pixels of the canvas, and files are paths: the compiled project carries its
assets, fonts and loudness to `alpha2.export`.

```python
from pathlib import Path

import alpha2
from alpha2 import compose as c

video = c.Video(
    canvas="youtube-1080p",
    body=c.Slideshow(
        sorted(Path("photos").glob("*.jpg")), seconds=6, transition=c.Crossfade(1), ken_burns=1.15
    ),
    overlays=[c.Title("Summer 2026", at=0.5, duration=4)],
    audio=c.MusicBed("music.m4a", fade_out=3),
)
compiled = video.compile(fonts=["DejaVuSans-Bold.ttf"])  # compiled.project is a plain dict
alpha2.export(compiled, "summer.mp4")
```

- Nodes: `Sequence`, `Stack`, `Grid`, `Media`, `Text`, `Color`, `Captions` (SubRip, WebVTT, cues
  or word timestamps), `Layout`, `Audio`, `Raw` (part of a project, as an overlay); components
  `Slideshow`, `CaptionedVideo`, `Title`, `TitleCard`, `LowerThird`, `EndCard`,
  `ComparisonGrid`, `Reframe`, `Logo`, `MusicBed`. Themes `clean-dark` and `clean-light`, or your
  own (`theme={"base": "clean-light", "accent": "#e63946"}`).
- Compiling needs each file's size and length. `compile` asks the host's `conform` for them (no
  GPU), or takes them as `facts=`; with every fact given it runs without a host. The same spec,
  facts and fonts compile to the same project.
- Templates are Python: loops, conditions and arithmetic build the lists the nodes take. A spec is
  JSON (`video.to_json()`, typed as `alpha2.compose_types.VideoSpec`) that every binding compiles
  the same way.
- An error names where in the spec it lies (`alpha2.Alpha2ComposeError`, `.path ==
  "spec.body.items[3].path"`).

## Finding `alpha2-host`

In this order: the `host_path` argument, `ALPHA2_HOST_PATH`, the `alpha2-host` wheel of this
version, the archive of this version in the shared cache, then `PATH`.

`python -m alpha2 install` downloads the release archive into the cache (`ALPHA2_HOST_CACHE_DIR`,
else `~/.cache/alpha2`, `~/Library/Caches/alpha2` or `%LOCALAPPDATA%\alpha2`), the one
`@alpha2-sh/node` uses, so one download serves both; `--thin` installs the archive that loads the
system's FFmpeg 8. Or unpack the archive from the GitHub release yourself and point
`ALPHA2_HOST_PATH` at its `bin/alpha2-host`.

## Development

The extension module is the Rust crate `bindings/alpha2-python`; maturin builds it into the package.

```sh
cd python/alpha2
uv venv -p 3.11 && uv pip install --group dev
uv pip install --no-deps -e .         # rebuild after changing the Rust crate
.venv/bin/pytest                       # end-to-end tests use ALPHA2_HOST_BINARY or target/debug/alpha2-host
.venv/bin/mypy && .venv/bin/ruff check
```

`--no-deps` because the pinned `alpha2-host` wheel exists only once a release is published; the
tests find a host through `ALPHA2_HOST_BINARY`, `ALPHA2_HOST_PATH` or the cache instead.
`src/alpha2/types.py` and `src/alpha2/compose_types.py` are generated from the project and compose
schemas by `npm run generate:schema`.
`python/alpha2-host` builds the `alpha2-host` wheel from a release archive.
