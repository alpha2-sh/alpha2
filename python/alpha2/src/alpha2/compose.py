"""Videos described by intent, compiled into plain project documents.

A :class:`Video` holds a compose spec (the shape of :class:`alpha2.compose_types.VideoSpec`): a
body, what is drawn over it, and what plays under it. The functions here build its nodes as plain
``dict``\\ s. :meth:`Video.compile` turns it into a :class:`Compiled` project with the compiler
every alpha2 binding shares (``alpha2-compose``, in the native module), after describing the files
it names with the host's ``conform``, which needs no GPU::

    import alpha2
    from alpha2 import compose as c

    video = c.Video(
        canvas="shorts",
        body=c.Media("talk.mp4"),
        overlays=[c.Captions("talk.srt", style="bar-bottom"), c.Logo("logo.png", size=180)],
        loudness=-14,
    )
    alpha2.export(video.compile(fonts=["DejaVuSans-Bold.ttf"]), "out.mp4")

Times are seconds and lengths pixels of the canvas. Loops, conditions and arithmetic are Python's:
build the lists the nodes take with it. The compiled project is an ordinary document; change it
before exporting, or open it in the editor.
"""

from __future__ import annotations

import json
import os
from collections import abc
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast

from . import _native
from . import client as _client
from .errors import Alpha2ComposeError

if TYPE_CHECKING:
    from .compose_types import (
        Align,
        CaptionStyle,
        Cue,
        FileKind,
        Fit,
        GridLength,
        LoudnessTarget,
        Node,
        TextRole,
        VideoSpec,
    )
    from .types import Rgba

PathLike = str | os.PathLike[str]

__all__ = [
    "COMPOSE_VERSION",
    "THEMES",
    "Audio",
    "CaptionedVideo",
    "Captions",
    "Color",
    "ComparisonGrid",
    "Compiled",
    "Crossfade",
    "EndCard",
    "Grid",
    "Layout",
    "Logo",
    "LowerThird",
    "Media",
    "MusicBed",
    "Raw",
    "Reframe",
    "Sequence",
    "Slideshow",
    "Stack",
    "Text",
    "Title",
    "TitleCard",
    "Transition",
    "Video",
    "compile",
    "gather_facts",
    "requirements",
]

COMPOSE_VERSION: int = _native.COMPOSE_VERSION
"""The compose spec version this build reads."""

THEMES: tuple[str, ...] = tuple(_native.COMPOSE_THEMES)
"""The built-in theme names."""


def _plain(value: Any) -> Any:
    """``value`` as JSON-ready data: paths as strings, tuples as lists, ``None`` fields dropped."""
    if isinstance(value, os.PathLike):
        return os.fspath(value)
    if isinstance(value, Mapping):
        return {key: _plain(item) for key, item in value.items() if item is not None}
    if isinstance(value, (list, tuple)):
        return [_plain(item) for item in value]
    return value


def _node(node_type: str, /, **fields: Any) -> Node:
    return cast("Node", _plain({"type": node_type, **fields}))


def _rename_loop(fields: dict[str, Any]) -> dict[str, Any]:
    # `loop` is a keyword argument here and a field in the spec.
    if "loop_" in fields:
        fields["loop"] = fields.pop("loop_")
    return fields


def Sequence(
    children: Iterable[Node],
    *,
    transition: str | Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Children one after another. ``transition`` (:func:`Crossfade`, :func:`Transition`, a name,
    ``"theme"`` or ``"none"``) overlaps neighbours by its duration; only single pictures (media,
    text, color, layout) take transitions other than a crossfade."""
    return _node(
        "sequence", children=list(children), transition=transition, id=id, at=at, duration=duration
    )


def Stack(
    children: Iterable[Node],
    *,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """Children over each other, each over the ones before, each from its own ``at``."""
    return _node("stack", children=list(children), id=id, at=at, duration=duration, inset=inset)


def Grid(
    children: Iterable[Node],
    *,
    columns: int | None = None,
    gap: float | None = None,
    length: GridLength | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """Children in grid cells, row by row. ``length`` is ``shortest`` (default),
    ``longest_freeze`` or ``longest_loop``."""
    return _node(
        "grid",
        children=list(children),
        columns=columns,
        gap=gap,
        length=length,
        id=id,
        at=at,
        duration=duration,
        inset=inset,
    )


def Media(
    path: PathLike,
    *,
    kind: FileKind | None = None,
    trim_start: float | None = None,
    trim_end: float | None = None,
    speed: float | None = None,
    loop: bool | None = None,
    volume: float | None = None,
    muted: bool | None = None,
    opacity: float | None = None,
    fade_in: float | None = None,
    fade_out: float | None = None,
    zoom: tuple[float, float] | None = None,
    blur: float | None = None,
    label: str | None = None,
    fit: Fit | None = None,
    align: Align | None = None,
    inset: float | abc.Sequence[float] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A video or still file. ``fit`` is ``contain`` (default), ``cover``, ``fill`` or ``none``;
    ``zoom`` is ``(from, to)`` over its time; ``label`` names its cell in a comparison grid."""
    return _node(
        "media",
        **_rename_loop(
            {
                "path": path,
                "kind": kind,
                "trim_start": trim_start,
                "trim_end": trim_end,
                "speed": speed,
                "loop_": loop,
                "volume": volume,
                "muted": muted,
                "opacity": opacity,
                "fade_in": fade_in,
                "fade_out": fade_out,
                "zoom": zoom,
                "blur": blur,
                "label": label,
                "fit": fit,
                "align": align,
                "inset": inset,
                "id": id,
                "at": at,
                "duration": duration,
            }
        ),
    )


def Text(
    text: str,
    *,
    style: TextRole | None = None,
    font_size: float | None = None,
    font_family: str | None = None,
    color: str | Rgba | None = None,
    background: str | Rgba | None = None,
    outline: str | Rgba | None = None,
    max_lines: int | None = None,
    opacity: float | None = None,
    fade: float | None = None,
    align: Align | None = None,
    inset: float | abc.Sequence[float] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Text in a theme style (``title``, ``subtitle``, ``body``, ``label``). With fonts to
    measure by, text longer than ``max_lines`` shrinks until it fits."""
    return _node(
        "text",
        text=text,
        style=style,
        font_size=font_size,
        font_family=font_family,
        color=color,
        background=background,
        outline=outline,
        max_lines=max_lines,
        opacity=opacity,
        fade=fade,
        align=align,
        inset=inset,
        id=id,
        at=at,
        duration=duration,
    )


def Color(
    color: str | Rgba | None = None,
    *,
    inset: float | abc.Sequence[float] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """An area painted one color (default: the theme's surface)."""
    return _node("color", color=color, inset=inset, id=id, at=at, duration=duration)


def Captions(
    source: PathLike | Iterable[Cue] | None = None,
    *,
    words: Iterable[Cue] | None = None,
    srt: str | None = None,
    vtt: str | None = None,
    style: CaptionStyle | None = None,
    max_chars: int | None = None,
    font_size: float | None = None,
    color: str | Rgba | None = None,
    position: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Timed text: from a SubRip or WebVTT file (``source`` a path), cues (``source`` a list of
    ``{"start", "end", "text"}``), word timestamps (``words``, grouped into cues), or SubRip or
    WebVTT text. ``style`` is ``outline`` (default), ``boxed``, ``bar-bottom`` or
    ``bold-center``."""
    file: PathLike | None = None
    cues: list[Cue] | None = None
    if isinstance(source, (str, os.PathLike)):
        file = source
    elif source is not None:
        cues = list(source)
    return _node(
        "captions",
        file=file,
        cues=cues,
        words=list(words) if words is not None else None,
        srt=srt,
        vtt=vtt,
        style=style,
        max_chars=max_chars,
        font_size=font_size,
        color=color,
        position=position,
        id=id,
        at=at,
        duration=duration,
    )


def Layout(
    root: Mapping[str, Any],
    *,
    align: Align | None = None,
    inset: float | abc.Sequence[float] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A layout generator's root box, as the project writes it
    (:class:`alpha2.types.LayoutNode`)."""
    return _node("layout", root=root, align=align, inset=inset, id=id, at=at, duration=duration)


def Audio(
    path: PathLike,
    *,
    trim_start: float | None = None,
    trim_end: float | None = None,
    loop: bool | None = None,
    volume: float | None = None,
    fade_in: float | None = None,
    fade_out: float | None = None,
    duck_under: str | None = None,
    duck_depth_db: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A sound file. ``duck_under`` names a node (or ``"body"``) whose sound it ducks under."""
    return _node(
        "audio",
        **_rename_loop(
            {
                "path": path,
                "trim_start": trim_start,
                "trim_end": trim_end,
                "loop_": loop,
                "volume": volume,
                "fade_in": fade_in,
                "fade_out": fade_out,
                "duck_under": duck_under,
                "duck_depth_db": duck_depth_db,
                "id": id,
                "at": at,
                "duration": duration,
            }
        ),
    )


def MusicBed(
    path: PathLike,
    *,
    volume: float | None = None,
    fade_in: float | None = None,
    fade_out: float | None = None,
    duck_under: str | None = None,
    duck_depth_db: float | None = None,
    loop: bool | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Music under the video: as long as it (looping by default), faded in (1 s) and out (2 s)
    unless told otherwise, and ducked under ``duck_under``'s sound."""
    return _node(
        "music_bed",
        **_rename_loop(
            {
                "path": path,
                "volume": volume,
                "fade_in": fade_in,
                "fade_out": fade_out,
                "duck_under": duck_under,
                "duck_depth_db": duck_depth_db,
                "loop_": loop,
                "id": id,
                "at": at,
                "duration": duration,
            }
        ),
    )


def Raw(
    *,
    sources: Iterable[Mapping[str, Any]] = (),
    tracks: Iterable[Mapping[str, Any]] = (),
    compositions: Iterable[Mapping[str, Any]] = (),
    assets: Mapping[str, PathLike] | None = None,
) -> Node:
    """Part of a project as the project writes it, at its own timeline times: an overlay only.
    ``assets`` maps its media sources' ids to their files."""
    return _node(
        "raw",
        sources=list(sources),
        tracks=list(tracks),
        compositions=list(compositions),
        assets=assets,
    )


def Slideshow(
    items: Iterable[PathLike | Node],
    *,
    seconds: float | None = None,
    fit_to: PathLike | None = None,
    transition: str | Mapping[str, Any] | None = None,
    ken_burns: float | None = None,
    fit: Fit | None = None,
    id: str | None = None,
    at: float | None = None,
) -> Node:
    """Stills or clips in turn, ``seconds`` each (transitions included), or sharing out
    ``fit_to``'s length. ``ken_burns`` zooms each in to that multiple; ``transition`` defaults
    to the theme's."""
    return _node(
        "slideshow",
        items=list(items),
        seconds=seconds,
        fit_to=fit_to,
        transition=transition,
        ken_burns=ken_burns,
        fit=fit,
        id=id,
        at=at,
    )


def CaptionedVideo(
    video: PathLike | Node,
    captions: Node,
    *,
    fit: Fit | None = None,
    id: str | None = None,
    at: float | None = None,
) -> Node:
    """A video (``cover`` by default) under :func:`Captions`."""
    body = dict(captions)
    if body.pop("type", "captions") != "captions":
        raise ValueError("captions must be a Captions node")
    return _node("captioned_video", video=video, captions=body, fit=fit, id=id, at=at)


def _title(
    kind: str,
    text: str,
    subtitle: str | None,
    align: Align | None,
    fade: float | None,
    background: str | Rgba | None,
    id: str | None,
    at: float | None,
    duration: float | None,
) -> Node:
    return _node(
        kind,
        text=text,
        subtitle=subtitle,
        align=align,
        fade=fade,
        background=background,
        id=id,
        at=at,
        duration=duration,
    )


def Title(
    text: str,
    *,
    subtitle: str | None = None,
    align: Align | None = None,
    fade: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A title, and a subtitle under it, over what is under them."""
    return _title("title", text, subtitle, align, fade, None, id, at, duration)


def TitleCard(
    text: str,
    *,
    subtitle: str | None = None,
    align: Align | None = None,
    fade: float | None = None,
    background: str | Rgba | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A title on the theme's surface (or ``background``)."""
    return _title("title_card", text, subtitle, align, fade, background, id, at, duration)


def LowerThird(
    text: str,
    *,
    subtitle: str | None = None,
    fade: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A name, and a role under it, in the lower left."""
    return _node(
        "lower_third", text=text, subtitle=subtitle, fade=fade, id=id, at=at, duration=duration
    )


def EndCard(
    text: str,
    *,
    subtitle: str | None = None,
    logo: PathLike | None = None,
    background: str | Rgba | None = None,
    fade: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A closing card: a logo, a title and a subtitle on the theme's surface."""
    return _node(
        "end_card",
        text=text,
        subtitle=subtitle,
        logo=logo,
        background=background,
        fade=fade,
        id=id,
        at=at,
        duration=duration,
    )


def ComparisonGrid(
    items: Iterable[PathLike | Node],
    *,
    columns: int | None = None,
    gap: float | None = None,
    length: GridLength | None = None,
    audio_from: int | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Clips side by side, each labeled by its :func:`Media` ``label``. Silent unless
    ``audio_from`` names an item by index."""
    return _node(
        "comparison_grid",
        items=list(items),
        columns=columns,
        gap=gap,
        length=length,
        audio_from=audio_from,
        id=id,
        at=at,
        duration=duration,
    )


def Reframe(
    video: PathLike | Node,
    *,
    blur: float | None = None,
    id: str | None = None,
    at: float | None = None,
) -> Node:
    """A video fitted to the canvas over a blurred, cropped copy of itself."""
    return _node("reframe", video=video, blur=blur, id=id, at=at)


def Logo(
    path: PathLike,
    *,
    corner: Align | None = None,
    size: float | None = None,
    margin: float | None = None,
    opacity: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A picture ``size`` pixels on its longer side in a corner (default ``top-right``)."""
    return _node(
        "logo",
        path=path,
        corner=corner,
        size=size,
        margin=margin,
        opacity=opacity,
        id=id,
        at=at,
        duration=duration,
    )


def Crossfade(duration: float) -> dict[str, Any]:
    """A crossfade of ``duration`` seconds."""
    return {"type": "crossfade", "duration": duration}


def Transition(
    kind: str,
    duration: float,
    *,
    easing: Mapping[str, Any] | None = None,
    params: Mapping[str, Any] | None = None,
    version: int | None = None,
) -> dict[str, Any]:
    """A built-in transition by its short name (``wipe-left``, ``circle-open``, ...) or a full
    effect id, over ``duration`` seconds."""
    return cast(
        "dict[str, Any]",
        _plain(
            {
                "type": kind,
                "duration": duration,
                "easing": easing,
                "params": params,
                "version": version,
            }
        ),
    )


@dataclass(frozen=True)
class Compiled:
    """A compiled spec: the project, and what exporting it needs besides.

    :func:`alpha2.export` takes it as it takes a project, with ``assets``, ``fonts`` and
    ``loudness`` defaulting to these. ``project`` is a plain ``dict``; change it freely.
    """

    project: dict[str, Any]
    """The project document."""
    assets: dict[str, str]
    """Each media source's file, by source id."""
    loudness: _client.LoudnessTarget | None = None
    """The loudness the spec asks an export to normalize to."""
    fonts: list[str] = field(default_factory=list)
    """The font files text was measured with, for the export to draw with."""
    warnings: list[str] = field(default_factory=list)
    """What the compiler did not do as asked, and why."""

    def export_options(self) -> dict[str, Any]:
        """The export options it carries: ``assets``, ``fonts`` and ``loudness``."""
        options: dict[str, Any] = {"assets": dict(self.assets), "fonts": list(self.fonts)}
        if self.loudness is not None:
            options["loudness"] = dict(self.loudness)
        return options


def _compose_error(error: ValueError) -> Alpha2ComposeError:
    if len(error.args) == 3:
        message, path, problem = error.args
        return Alpha2ComposeError(message, path, problem)
    return Alpha2ComposeError(str(error), "spec", str(error))


def requirements(spec: Mapping[str, Any] | Video) -> dict[str, Any]:
    """The files ``spec`` names: ``{"media": [{"path", "kind"}], "texts": [path]}``."""
    try:
        return cast("dict[str, Any]", json.loads(_native.compose_requirements(_spec_json(spec))))
    except ValueError as error:
        raise _compose_error(error) from None


def _spec_json(spec: Mapping[str, Any] | Video) -> str:
    document = spec.spec if isinstance(spec, Video) else _plain(spec)
    return json.dumps(document, ensure_ascii=False, allow_nan=False)


def _media_facts(fingerprint: Mapping[str, Any]) -> dict[str, Any]:
    """What the compiler needs of a conform fingerprint (``MediaFacts::from_fingerprint``)."""
    streams = fingerprint.get("streams") or {}
    video = streams.get("video") or {}
    facts: dict[str, Any] = {"has_audio": streams.get("audio") is not None}
    for key in ("width", "height", "frame_rate"):
        if key in video:
            facts[key] = video[key]
    if "duration_ticks" in fingerprint:
        facts["duration_ticks"] = fingerprint["duration_ticks"]
    return facts


def gather_facts(
    spec: Mapping[str, Any] | Video,
    *,
    known: Mapping[str, Any] | None = None,
    client: _client.HostClient | None = None,
) -> dict[str, Any]:
    """What compiling ``spec`` needs to know about its files, beyond ``known`` (facts in the same
    shape): media described by the host's ``conform`` (no GPU), caption files read as UTF-8.

    Raises :class:`~alpha2.Alpha2ComposeError` for a file the host cannot read.
    """
    facts: dict[str, Any] = {
        "media": dict((known or {}).get("media", {})),
        "texts": dict((known or {}).get("texts", {})),
    }
    needed = requirements(spec)
    media = [entry for entry in needed["media"] if entry["path"] not in facts["media"]]
    if media:
        # A project that reads each file once, a tick apiece, for the host to describe.
        tracks: dict[str, list[dict[str, Any]]] = {"video": [], "audio": []}
        sources = []
        for index, entry in enumerate(media):
            source_id = f"m{index}"
            sources.append({"id": source_id, "kind": entry["kind"], "locator": entry["path"]})
            lane = tracks["audio" if entry["kind"] == "audio" else "video"]
            tick = len(lane)
            lane.append(
                {
                    "id": f"c{index}",
                    "source_id": source_id,
                    "timeline_range": {"start": tick, "end": tick + 1},
                    "source_range": {"start": 0, "end": 1},
                }
            )
        probe = {
            "schema_version": _native.SCHEMA_VERSION,
            "id": "compose-facts",
            "name": "compose facts",
            "settings": {
                "width": 16,
                "height": 16,
                "frame_rate": {"num": 30, "den": 1},
                "sample_rate": 48000,
            },
            "sources": sources,
            "timeline": {
                "tracks": [
                    {"id": kind, "name": kind, "kind": kind, "clips": clips}
                    for kind, clips in tracks.items()
                    if clips
                ]
            },
        }
        assets = {source["id"]: source["locator"] for source in sources}
        answer = (client.conform if client is not None else _client.conform)(probe, assets=assets)
        found = {entry["source_id"]: entry["original"] for entry in answer["catalog"]["assets"]}
        for source in sources:
            path = source["locator"]
            original = found.get(source["id"], {"state": "offline"})
            if original.get("state") != "online":
                reason = original.get("reason") or "no such file"
                raise Alpha2ComposeError(
                    f'facts: cannot read "{path}": {reason}', "facts", f'cannot read "{path}"'
                )
            facts["media"][path] = _media_facts(original["fingerprint"])
    for path in needed["texts"]:
        if path not in facts["texts"]:
            facts["texts"][path] = Path(path).read_text(encoding="utf-8-sig")
    return facts


def compile(  # noqa: A001 - the module's verb, as `re.compile`
    spec: Mapping[str, Any] | Video,
    *,
    facts: Mapping[str, Any] | None = None,
    fonts: abc.Sequence[PathLike] | None = None,
    client: _client.HostClient | None = None,
) -> Compiled:
    """Compiles ``spec`` into a project.

    ``facts`` describes files the spec names (``{"media": {path: {width, height, frame_rate,
    duration_ticks, has_audio}}, "texts": {path: text}}``); the rest are gathered with
    :func:`gather_facts`, so a spec whose facts are all given compiles without a host. ``fonts``
    are the font files the export draws with: text that must fit is measured with them, and
    :attr:`Compiled.fonts` carries them to the export.

    Raises :class:`~alpha2.Alpha2ComposeError` naming where in the spec it does not compile.
    """
    document = _spec_json(spec)
    gathered = gather_facts(json.loads(document), known=facts, client=client)
    font_paths = [os.fspath(font) for font in fonts or ()]
    font_bytes = [Path(font).read_bytes() for font in font_paths]
    try:
        result = json.loads(_native.compose(document, json.dumps(gathered), font_bytes))
    except ValueError as error:
        raise _compose_error(error) from None
    return Compiled(
        project=result["project"],
        assets=result["assets"],
        loudness=result.get("loudness"),
        fonts=font_paths,
        warnings=result["warnings"],
    )


class Video:
    """A compose spec: a ``body``, the ``overlays`` drawn over it, and the ``audio`` under it.

    ``canvas`` is a preset (``youtube-1080p``, the default; ``youtube-4k``, ``youtube-720p``,
    ``shorts``, ``square``, ``portrait``) or ``{"preset", "width", "height", "frame_rate",
    "background"}``; ``theme`` a built-in name (:data:`THEMES`) or a theme dict. ``loudness`` is
    LUFS (under -1 dBTP) or ``{"target_lufs", "true_peak_ceiling_dbtp"}``, which the export
    normalizes to. :attr:`spec` is the plain spec.
    """

    def __init__(
        self,
        body: Node,
        *,
        canvas: str | Mapping[str, Any] | None = None,
        theme: str | Mapping[str, Any] | None = None,
        overlays: Iterable[Node] = (),
        audio: Node | Iterable[Node] = (),
        duration: float | None = None,
        loudness: float | LoudnessTarget | None = None,
        name: str | None = None,
    ) -> None:
        audio_nodes = [audio] if isinstance(audio, Mapping) else list(audio)
        self.spec: VideoSpec = cast(
            "VideoSpec",
            _plain(
                {
                    "compose_version": COMPOSE_VERSION,
                    "name": name,
                    "canvas": canvas,
                    "theme": theme,
                    "body": body,
                    "overlays": list(overlays) or None,
                    "audio": audio_nodes or None,
                    "duration": duration,
                    "loudness": loudness,
                }
            ),
        )

    def requirements(self) -> dict[str, Any]:
        """The files the spec names (:func:`requirements`)."""
        return requirements(self)

    def compile(
        self,
        *,
        facts: Mapping[str, Any] | None = None,
        fonts: abc.Sequence[PathLike] | None = None,
        client: _client.HostClient | None = None,
    ) -> Compiled:
        """The spec compiled (:func:`compile`)."""
        return compile(self, facts=facts, fonts=fonts, client=client)

    def to_json(self) -> str:
        """The spec as JSON text, as every alpha2 binding reads it."""
        return _spec_json(self)
