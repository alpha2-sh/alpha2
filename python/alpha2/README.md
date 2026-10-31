# alpha2 for Python

Build, validate and render [alpha2](https://github.com/alpha2-sh/alpha2) project documents from
Python. A project is a plain `dict` in the engine's `project.v1` shape (typed as
`alpha2.types.Project`). Validation runs the engine's own validator in-process; rendering runs the
native `alpha2-host` process on the GPU, or on the CPU where there is none.

```sh
pip install alpha2
```

On Linux (glibc 2.35 or later; not musl) x86_64 and aarch64, macOS arm64 and Windows x64 this
also installs `alpha2-host`, the native renderer, as a platform wheel of the same version.
Elsewhere build `alpha2-host` from source (`cargo install alpha2-host` with FFmpeg 8 development
files) and set `ALPHA2_HOST_PATH`; see [Finding `alpha2-host`](#finding-alpha2-host).

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
print(result["output"], result["videoPipeline"], result["frames"])
```

- `alpha2.export`, `alpha2.conform` and `alpha2.capabilities` run on a shared `HostClient`; make
  your own (`alpha2.HostClient(concurrency=2)`) to bound or separate them.
- An existing `output` is refused unless you pass `overwrite=True`, which replaces it only once
  the export is whole. An output that is one of the export's inputs is always refused.
- Setting a `cancel` event (`threading.Event`) or pressing Ctrl-C cancels the request: the client
  writes a cancel line, the host stops the export, removes its partial output and answers, and the
  host ends, all before the call returns. A host that does not answer within 5 s is ended; a second
  Ctrl-C ends it at once. The host also exits when the process that started it dies.
- Every failure is an `alpha2.Alpha2HostError` with a machine-readable `code` and a classified
  `failure` dict for retry policies. A host that writes nothing within `host_start_timeout`
  (seconds, a `HostClient` argument, 30 by default, `None` to wait forever) is killed and the
  request fails with `HOST_START_TIMEOUT`; a host macOS has quarantined is refused before it starts
  with `HOST_QUARANTINED`, one Windows refuses by policy is `HOST_BLOCKED`, and a Linux host whose
  program interpreter is missing (musl) or whose glibc is too old is `HOST_LOAD_FAILED`.
- Options you write use snake_case (`loudness={"target_lufs": -14, ...}`); what the host reports
  keeps the protocol's camelCase keys (`result["effectiveRange"]`).

## Authoring helpers

The document is plain dicts, and the helpers write them: values, effects, a clip's `visual`,
layouts and shapes. They write the document's own field names, so a helper and a hand-written dict
mix freely, and `validate_project` checks the result with the engine's messages.

```python
from alpha2 import effect, fx, key, keyframes, visual
from alpha2.layout import box, layout, text
from alpha2.shapes import circle, fill, shapes

card = project.generated(
    layout(
        box(
            text("Hello", font_size="40px", line_height=1.4),
            padding="12px 20px",
            background="#1e2230",
            border_radius="16px",
        )
    )
)
project.video_track().clip(
    card,
    at=0,
    duration=3,
    visual=visual(opacity=keyframes(key(0, 0.0), key(0.5, 1.0)), scale=1.2),
    effects=[effect("alpha2.effect.gaussian-blur", radius=0.002)],
)
dot = project.generated(shapes(circle((0.5, 0.5), 0.2, picture=(1280, 720)), fill("#ffb020")))
```

- `key(at, value)` takes `at` in seconds or `alpha2.Ticks`; `keyframes(*keys)` refuses keys out of
  order instead of sorting them. Every other field takes a plain value, which becomes a constant.
- An effect parameter is typed in the document. `fx.param` (which `effect` applies) lifts a
  `float`, a `bool`, an `(x, y)` pair and a hex color; an `int` or a 4-tuple is ambiguous and raises
  `TypeError` naming the constructors (`fx.scalar(1.0)` or `fx.enum(1)`, `fx.vec4` or `fx.color`).
- `alpha2.layout` is the TypeScript `alpha2/layout` builder: `box(*children, **props)`,
  `text(content, **props)`, `shape(items, **props)`. Length and CSS strings are not parsed in
  Python; the engine reports a bad one.

### Units

| What | Unit |
| --- | --- |
| Positions: a clip's `position` and `anchor`, a shape's `center`, path vertices | Per axis: x a fraction of the width, y of the height |
| Sizes of shapes (`rect`, `ellipse`) | Per axis, as positions: `(0.5, 0.5)` is round only on a square picture |
| Lengths: corner radii, stroke widths, dashes, blur radii, layout numbers | A fraction of the picture's shorter side |
| Font sizes (text generators, layouts) | A fraction of the picture's shorter side, or `"40px"` of the canvas in a layout |
| Letter spacing, text stroke widths, text shadow offsets and blur | A fraction of the font size |

`circle(center, diameter, picture=(w, h))` gives a round ellipse: the diameter is a shorter-side
fraction, and `picture` is the size of what the shapes draw on (the canvas for a shapes generator,
the content box for a shape inside a layout box), so it has no default.

### Time bases

| Owner | Keyed in | Why |
| --- | --- | --- |
| A clip's `visual`, its masks, a transition | Timeline time, from the project's start | They place the clip on the timeline. |
| A composition's settings (a clip's `effects`) | Composition time, from the clip's start | The composition moves with the clip that shows it. |
| Shapes and layout generators | Picture time: the source time a clip selects | A generated picture is a source, trimmed and placed by its clips as a video file is. |

A keyframe in the wrong base usually falls outside the time its owner is shown.
`alpha2.lint_project(document)` reports each keyframe no frame reaches as `unreached_keyframe`,
with its owner, property and time.

### Layers

A track shows one clip at a time: clips on one track may not overlap, except where a transition
joins them. Overlapping layers are one track each, the later track drawn on top. For many layers in
one place, use `compose.Stack`, a composition `merge` node (up to 32 layers in one clip), or one
shapes or layout generator that draws them all.

### Animated text

A layout `text` takes keyframes of strings, so a count-up is one clip:

```python
count = keyframes(*(key(i / 30, str(i)) for i in range(101)))
counter = project.generated(layout(box(text(count, font_size=0.2), justify_content="center")))
```

Text holds from one key to the next: `text` writes `key`'s default `linear` as `hold`, and refuses
any other interpolation.

### Measure text

`alpha2.TextMeasurer(fonts)` measures text and layout generators with the engine's own placement
code and without a GPU. Give it the font files an export gets, in the same order: it registers the
same faces (every face of a `.ttc`).

```python
measurer = alpha2.TextMeasurer(["fonts/Inter.ttc"])
bounds = measurer.layout(card_layout, picture=(1280, 720))
title = bounds["nodes"][1]
print(title["border"], title["lineCount"], title["overflow"], title["baselines"])
words = measurer.text({"type": "text", "text": "Hi"}, canvas=(1280, 720))
```

Rectangles are fractions of the picture, `(0, 0)` top-left to `(1, 1)` bottom-right; `baselines`
holds the first and last lines' alphabetic baselines as fractions of its height, to line a text up
with another or with a shape by arithmetic. Measure at the size the picture is drawn at: at another
size lines can break elsewhere. The measurer reads its fonts once, so a loop that shrinks a text
until it fits is quick; measuring releases the GIL.

## Check a frame

`alpha2.still` renders one frame of a project to a PNG, the frame an export draws there, without
encoding a video:

```python
result = alpha2.still(project.build(), 1.5, "check.png", assets=project.assets())
print(result["stills"][0]["frameTime"])  # the grid frame that holds 1.5 s, in ticks
```

- Times are seconds or exact `alpha2.Ticks`. The frame drawn is the one of the project's frame
  grid that holds the time; `frameTime` says which.
- `alpha2.stills(project, [(0, "a.png"), (2.5, "b.png")], ...)` renders up to 256 frames with one
  host, decoding once; `size=(width, height)` renders the project at another canvas size.
- The PNG is 8-bit RGBA, the picture before any video encoding. `overwrite=True` replaces an
  existing file, as for `export`.
- From a shell: `python -m alpha2 still project.json check.png --at 1.5 --asset title=title.png`.

## Renderer and video decode

`renderer=` (`"auto"`, `"gpu"`, `"cpu"`; typed as `alpha2.client.RendererMode`) chooses what draws
the frames, and `video_decode=` (`"software"`, `"auto"`, `"hardware-required"`) how video is
decoded, on `HostClient(...)` for every request or per call to `export`, `still` and
`capabilities`. Unset, the host reads `ALPHA2_RENDERER` and `ALPHA2_VIDEO_DECODE`, else decides:
`auto` renderer, and `auto` decode on Linux, `software` on macOS and Windows.

- `auto` renders on a hardware GPU when there is one and on the CPU otherwise, never on a software
  GPU adapter (lavapipe, llvmpipe, WARP, SwiftShader). `gpu` takes whatever adapter the host finds
  and fails with `GPU_INIT_FAILED` without one; `cpu` opens no graphics driver.
- Results and capabilities report `renderer` (`"gpu"` or `"cpu"`), `rendererReason` (why the host
  renders on the CPU) and, on the CPU, `rendererThreads`; the capability report's
  `["capabilities"]["features"]` has `renderer.gpu`, `renderer.cpu` and `effects.runtime`. A
  runtime WGSL effect cannot run on the CPU renderer: such an export fails with `EFFECT_ERROR`.
- Under `auto` decode, an export whose hardware decoder stalls runs once more decoding in software.

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
`@alpha2-sh/node` uses, so one download serves both. Or unpack the archive from the GitHub release
yourself and point `ALPHA2_HOST_PATH` at its `bin/alpha2-host`. On macOS an archive downloaded
with a browser is quarantined, and the client refuses it (`HOST_QUARANTINED`): run
`xattr -dr com.apple.quarantine <unpacked dir>`, or download with `curl -LO`. To use a system
FFmpeg 8, build the host from source (`cargo install alpha2-host`).

Downloads honour `HTTPS_PROXY` (and `NO_PROXY`) and verify with the operating system's certificate
store on macOS and Windows (through `truststore`), and with OpenSSL's store on Linux, or `certifi`'s
roots where the system has none. A proxy that inspects TLS needs its root certificate in the system
store (or in `SSL_CERT_FILE` on Linux). `ALPHA2_HOST_DOWNLOAD_URL` (or the `download_url` argument)
names a mirror laid out like `https://github.com/alpha2-sh/alpha2/releases/download`. On Linux the
command refuses musl systems and glibc older than 2.35 before downloading anything. The Microsoft
Store's Python keeps what it writes under `%LOCALAPPDATA%` private to itself, so the command warns
there: set `ALPHA2_HOST_CACHE_DIR` to a folder outside `%LOCALAPPDATA%` to share the install.

## Software H.264 (OpenH264)

Where no hardware H.264 encoder is available, `alpha2-host` can encode H.264 in software with
Cisco's OpenH264 library, which it loads at run time. Neither this package nor the `alpha2-host`
wheel includes that library, and nothing downloads it unless you ask: Cisco's patent licence covers
its binary only when the user fetches it from Cisco.

```sh
python -m alpha2 install --openh264
```

This downloads Cisco's OpenH264 2.7.0 binary for the platform from
`https://ciscobinary.openh264.org`, checks the download and the library in it against their
SHA-256s, and puts the library in the shared cache, in `openh264-2.7.0-<target>/`, where
`alpha2-host` finds it. It installs only the library, not `alpha2-host`. `--force` downloads again,
`alpha2.install_openh264()` does the same from code, and `ALPHA2_OPENH264_DOWNLOAD_URL` (or the
`download_url` argument) names a mirror. Every run prints Cisco's notice:

```text
OpenH264 Video Codec provided by Cisco Systems, Inc.
```

The library is Cisco's binary, under [Cisco's terms](https://www.openh264.org/BINARY_LICENSE.txt).
Set `ALPHA2_OPENH264=0` to keep `alpha2-host` from using it.

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
