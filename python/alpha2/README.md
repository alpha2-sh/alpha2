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
- A client keeps up to `concurrency` `alpha2-host` processes and sends each one request at a
  time, so later requests skip the start. A host serves until it decides a new process would do
  better (memory, request count, idle time) and drains; the client then starts another. A host
  takes only requests that would start a host with its environment, decode and renderer modes
  included, so changing `os.environ` starts new hosts. `client.close()`, or a `with` block, ends
  them; an idle host never keeps Python from exiting.
- A forked child (`os.fork`, or `multiprocessing` with the `fork` start method) does not use the
  hosts it inherits: it starts its own on its first request, and the parent keeps its hosts.
  Capability reports the parent already had stay cached in the child.
- An existing `output` is refused unless you pass `overwrite=True`, which replaces it only once
  the export is whole. An output that is one of the export's inputs is always refused.
- Setting a `cancel` event (`threading.Event`) or pressing Ctrl-C cancels the request: the client
  sends a cancel message, and the host stops the export, removes its partial output and answers
  before the call returns. A host that does not answer within 5 s is ended; a second Ctrl-C ends
  it at once. The host also exits when the process that started it dies.
- Every failure is an `alpha2.Alpha2HostError` with a machine-readable `code` and a classified
  `failure` dict for retry policies. A host that is not ready within `host_start_timeout`
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
- Without a path (`alpha2.still(project, 1.5)`, or `None` in a `stills` pair) nothing is written:
  the PNG comes back as bytes in `result["stills"][i]["png"]`, and `output` is `None`.
- From a shell: `python -m alpha2 still project.json check.png --at 1.5 --asset title=title.png`.

## Extract data

`HostClient.extract(project, request, extractor, assets=...)` runs an extractor of your own over a
source or the timeline and returns `{"dataSets", "timings"}`, data sets ready to put in the
project's `data`. The host decodes and presents the input and calls the extractor step by step;
every value it emits is validated as the project validates data:

```python
class Brightness:
    def describe(self):
        return {
            "id": "com.example.brightness",
            "version": 1,
            "input": {"media": "video", "from": ["source"], "format": "luma8"},
            "outputs": [{"name": "level", "kind": "scalars", "channels": ["y"]}],
        }

    def video(self, frame, out):
        out.sample("level", frame.time, [sum(frame.data) / len(frame.data) / 255])


with alpha2.HostClient() as client:
    result = client.extract(
        project,
        {
            "extractor": "com.example.brightness",
            "version": 1,
            "input": {
                "from": "source",
                "source_id": "interview",
                "range": {"start": 0, "end": 600_000},
                "size": 64,
            },
            "outputs": {"level": "interview-brightness"},
        },
        Brightness(),
        assets={"interview": "interview.mp4"},
    )
```

- `begin(context)`, `pass_(index, direction)`, `audio(block, out)` and `end(out)` are the other
  steps (`alpha2.extract` has the types). `request["params"]` is checked against the
  descriptor's `params_schema` first.
- Output the project would refuse fails with `INVALID_EMIT`, a step that raises with
  `EXTRACTOR_FAILED`, and a step slower than the host's `--extract-step-timeout` with
  `EXTRACTOR_TIMEOUT`. `cancel=` and `on_progress=` work as for `export`.
- A [plugin](../../docs/plugin-abi.md) runs inside the host instead: pass
  `plugin={"manifest": ..., "sha256": ...}` and no extractor. The host fetches the manifest (an
  `https:` URL or a path), checks every file against its pinned SHA-256 and runs the wasm artifact
  sandboxed; `"allow_native": True` lets a native artifact run with the host's rights. A plugin
  that is refused, does not load, traps or runs past its limits fails with a `PLUGIN_*` code.

## Apply data with edit operations

`alpha2.edit.apply(project, request)` runs one of the engine's edit operations in this process, no
host needed, and returns `Edited(project, project_json, report)`. `remove_ranges` drops ranges
(silences from an extractor, or a list on a source or the timeline) and closes the gaps,
`split_at` cuts at times, and `bake` turns data bindings into keyframes and words into cues:

```python
edited = alpha2.edit.apply(
    project,
    {
        "op": "remove_ranges",
        "ranges": {"data": "interview-silence"},
        "target": {"track": "v1"},
        "padding": 12_000,
        "min_duration": 36_000,
    },
)
```

Kept moments render as before for clip picture and sound, clip keyframes, captions, hold data
and linear automation; a matte or a binding read through another clip follows the part beside it
or the edit is refused (`matte_source`, `read_through`). The host protocol lists what is not exact
in this release. `project_json` is the same bytes on every surface.
A refusal raises `alpha2.Alpha2EditError`, whose `refusal` names why (`unknown_data`,
`empty_target`, ...). [The host protocol](../../docs/host-protocol.md#edit) lists the fields.

## Renderer and video decode

`renderer=` (`"auto"`, `"gpu"`, `"cpu"`; typed as `alpha2.client.RendererMode`) chooses what draws
the frames, and `video_decode=` (`"software"`, `"auto"`, `"hardware-required"`) how video is
decoded, on `HostClient(...)` for every request or per call to `export`, `still` and
`capabilities`. Unset, the host reads `ALPHA2_RENDERER` and `ALPHA2_VIDEO_DECODE`, else decides:
`auto` renderer, and `auto` decode on Linux and macOS, `software` on Windows.

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
- Wrappers combine any nodes: `Group` (one layer, with `effects=[Effect("gaussian-blur",
  radius=8)]`, a transform and a blend mode), `Matte` (one node shown through another's alpha or
  luma), `Mask` (shapes cut out of a node), `Shape` (rectangles, ellipses, polygons and SVG paths)
  and `Box` (CSS flexbox or grid around text, shapes and any other node). Any value may move:
  `Ramp(0, 1, ease="ease-out")` or `Keys((0, 0), (1, 1))`.
- Motion: `enter=` and `exit=` on visual nodes and containers take `Fade`, `Rise`, `Slide` or
  `Scale`, timed from the node's own start and back from its own end; `Stack` and `Grid` take
  `stagger=` to move children one after another; `Text` takes `reveal=Reveal("word")` (or
  `"line"`); `Counter(0, 1200, thousands=",")` counts on the frame grid. Easings are CSS names
  (`"ease-out"`) or easing objects. All of it compiles to ordinary keyframes.
- Helpers for hand-built projects: `alpha2.frames(30).snap(1.234)` (frame-exact seconds),
  `alpha2.EASING_PRESETS`, `alpha2.linear_gradient` and `alpha2.radial_gradient`.
- Compiling needs each file's size and length. `compile` asks the host's `conform` for them (no
  GPU), or takes them as `facts=`; with every fact given it runs without a host. The same spec,
  facts and fonts compile to the same project.
- A spec is JSON (`video.to_json()`, typed as `alpha2.compose_types.VideoSpec`, schema
  `https://alpha2.sh/schema/compose.v1.json`) that every binding compiles the same way.
- An error names where in the spec it lies (`alpha2.Alpha2ComposeError`, `.path ==
  "spec.body.items[3].path"`).

## Templates

A template is a spec with parameters: `{"template": {"id": "acme/promo", "version": 1}, "params":
[...], ...}`, its text, files and colors taken from its data. `c.bind(template, data)` fills it in
and checks the data, `c.data_schema(template)` is the JSON Schema of its data (for a form, an API
or a model), and `c.lint(template)` finds what a template should take from its data but does not.
A `node` template is a node type of its own, `{"type": "acme/card", "with": {...}}`, in any spec
compiled with `templates=[...]`. `c.TEMPLATES` are the built-in `alpha2/` ones, such as
`alpha2/circle-reveal`.

```python
spec = c.bind(promo, {"headline": "Summer sale", "photo": "beach.jpg"})
alpha2.export(c.compile(spec, fonts=["Inter.ttf"]), "promo.mp4")
```

## MCP server

`python -m alpha2 mcp` serves compose, templates and rendering as MCP tools on standard input and
output, for an agent to list templates, fill in their data, check, look at a frame and render the
video, or write a spec from the compose schema one node type at a time.

```sh
claude mcp add alpha2 -- python -m alpha2 mcp --font Inter.ttf --templates ./templates
```

`--font` gives the fonts text is drawn with (text needs one), `--templates` a directory of template
JSON files beside the built-in ones, and `--host` the `alpha2-host` binary. Relative paths in specs
are read from the server's working directory. `render_frame` (given an `output`) and `render_video`
write files, replacing one only with `overwrite`, and are marked destructive; the other tools only
read. A call that fails, however malformed, is answered with an error and the server keeps serving.

## Finding `alpha2-host`

In this order: the `host_path` argument, `ALPHA2_HOST_PATH`, the `alpha2-host` wheel of this
version, the archive of this version in the shared cache, then `PATH`.

`python -m alpha2 install` downloads the release archive into the cache (`ALPHA2_HOST_CACHE_DIR`,
else `~/.cache/alpha2`, `~/Library/Caches/alpha2` or `%LOCALAPPDATA%\alpha2`), the one
`@alpha2-sh/node` uses, so one download serves both. It checks the archive against the SHA-256
the published wheel pins for it, so a mirror cannot substitute one; a source checkout, which has no
pin, checks against the release's `checksums.sha256` instead (`InstallResult.verified_by` says
which). Or unpack the archive from the GitHub release
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
