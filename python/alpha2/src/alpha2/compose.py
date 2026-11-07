"""Videos described by intent, compiled into plain project documents.

A :class:`Video` holds a compose spec (:class:`alpha2.compose_types.VideoSpec`): a body, overlays
and audio, built from plain ``dict`` nodes. :meth:`Video.compile` describes the named files with
the host's ``conform`` (no GPU) and compiles a :class:`Compiled` project::

    import alpha2
    from alpha2 import compose as c

    video = c.Video(
        canvas="shorts",
        body=c.Media("talk.mp4"),
        overlays=[c.Captions("talk.srt", style="bar-bottom"), c.Logo("logo.png", size=180)],
        loudness=-14,
    )
    alpha2.export(video.compile(fonts=["DejaVuSans-Bold.ttf"]), "out.mp4")

Times are seconds and lengths are canvas pixels. The compiled project is an ordinary document that
may be edited before export.
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
from ._text import UserTextError, read_user_text
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
        SpeakerLook,
        TextRole,
        VideoSpec,
    )
    from .types import Rgba, TextShadow, TextStroke

PathLike = str | os.PathLike[str]

__all__ = [
    "COMPOSE_VERSION",
    "TEMPLATES",
    "THEMES",
    "Audio",
    "Box",
    "CaptionedVideo",
    "Captions",
    "Color",
    "ComparisonGrid",
    "Compiled",
    "Counter",
    "Crossfade",
    "Effect",
    "EndCard",
    "Fade",
    "Grid",
    "Group",
    "Item",
    "Keys",
    "Layout",
    "Logo",
    "LowerThird",
    "Mask",
    "Matte",
    "Media",
    "MusicBed",
    "Ramp",
    "Raw",
    "Reframe",
    "Reveal",
    "Rise",
    "Scale",
    "Sequence",
    "Shape",
    "Slide",
    "Slideshow",
    "Stack",
    "Text",
    "ThemeColor",
    "Title",
    "TitleCard",
    "Transition",
    "Video",
    "bind",
    "compile",
    "data_schema",
    "gather_facts",
    "lint",
    "requirements",
]

COMPOSE_VERSION: int = _native.COMPOSE_VERSION
"""The compose spec version this build reads."""

THEMES: tuple[str, ...] = tuple(_native.COMPOSE_THEMES)
"""The built-in theme names."""


def _plain(value: Any) -> Any:
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
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Children one after another. ``transition`` (:func:`Crossfade`, :func:`Transition`, a name,
    ``"theme"`` or ``"none"``) overlaps neighbours by its duration; only single pictures (media,
    text, color, layout) take transitions other than a crossfade."""
    return _node(
        "sequence",
        children=list(children),
        transition=transition,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


def Stack(
    children: Iterable[Node],
    *,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    stagger: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """Children over each other, each over the ones before, each from its own ``at``. With
    ``stagger`` seconds, ``enter`` and ``exit`` move each child, child ``i`` entering
    ``i * stagger`` after its start."""
    return _node(
        "stack",
        children=list(children),
        enter=enter,
        exit=exit,
        stagger=stagger,
        id=id,
        at=at,
        duration=duration,
        inset=inset,
    )


def Group(
    children: Iterable[Node],
    *,
    opacity: float | Mapping[str, Any] | None = None,
    blend: str | None = None,
    effects: abc.Sequence[Mapping[str, Any]] | None = None,
    transform: Mapping[str, Any] | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """Children drawn together into one picture that then takes ``opacity``, ``blend``,
    ``effects`` (:func:`Effect`) and ``transform`` (``position``, ``scale``, ``rotation``,
    ``anchor``) as one layer. A video in it plays over the group's whole time."""
    return _node(
        "group",
        children=list(children),
        opacity=opacity,
        blend=blend,
        effects=None if effects is None else [dict(effect) for effect in effects],
        transform=None if transform is None else dict(transform),
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
        inset=inset,
    )


def Shape(
    shapes: Iterable[Mapping[str, Any]],
    *,
    fill: str | Mapping[str, Any] | None = None,
    stroke: Mapping[str, Any] | None = None,
    trim: Mapping[str, Any] | None = None,
    opacity: float | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """Outlines (``{"shape": "rect" | "ellipse" | "polygon" | "star" | "path", ...}``) in the
    node's unit square, filled and stroked as one path. Paints are colors, theme colors
    (:func:`ThemeColor`) or gradients; without fill or stroke it fills with the theme's accent."""
    return _node(
        "shape",
        shapes=[dict(shape) for shape in shapes],
        fill=fill if fill is None or isinstance(fill, str) else dict(fill),
        stroke=None if stroke is None else dict(stroke),
        trim=None if trim is None else dict(trim),
        opacity=opacity,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
        inset=inset,
    )


def Matte(
    content: Node,
    by: Node,
    *,
    use: str | None = None,
    invert: bool | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """``content`` shown where ``by``'s picture is, by its ``alpha`` (default) or ``luma``;
    ``invert`` shows it where ``by`` is not. ``by`` is never drawn itself."""
    return _node(
        "matte",
        content=content,
        by=by,
        use=use,
        invert=invert or None,
        id=id,
        at=at,
        duration=duration,
        inset=inset,
    )


def Mask(
    content: Node,
    shapes: Iterable[Mapping[str, Any]],
    *,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """``content`` shown only inside ``shapes`` (outlines as :func:`Shape` takes, each with an
    optional ``mode``, ``feather`` and ``invert``), which stay put while the content moves."""
    return _node(
        "mask",
        content=content,
        shapes=[dict(shape) for shape in shapes],
        id=id,
        at=at,
        duration=duration,
        inset=inset,
    )


def Box(
    children: Iterable[Node],
    *,
    background: str | Mapping[str, Any] | None = None,
    border: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
    **css: Any,
) -> Node:
    """A CSS flexbox (or ``display="grid"``) box filling the node's area: ``css`` are the engine
    layout's container fields (``flex_direction="column"``, ``gap="8px"``,
    ``padding="12px 16px"``). Text, shapes, colors and boxes are laid out by it; any other child
    (a video, a group) is drawn in its place, clipped to its ``border_radius``. A child's item
    fields go with it through :func:`Item`."""
    return _node(
        "box",
        children=list(children),
        background=background
        if background is None or isinstance(background, str)
        else dict(background),
        border=None if border is None else dict(border),
        id=id,
        at=at,
        duration=duration,
        inset=inset,
        **css,
    )


def Item(node: Node, /, **item: Any) -> Node:
    """``node`` with box item fields (``flex_grow=1``, ``width="200px"``, ``aspect_ratio=1``,
    ``border_radius="12px"``) for the box it is in."""
    return cast("Node", {"box": _plain(item), "node": node})


def ThemeColor(token: str, alpha: float | None = None) -> dict[str, Any]:
    """The theme's ``text``, ``background``, ``surface``, ``panel`` or ``accent`` color, its
    opacity times ``alpha``."""
    return cast("dict[str, Any]", _plain({"$theme": token, "alpha": alpha}))


def Effect(effect: str, /, **params: Any) -> dict[str, Any]:
    """An effect for :func:`Group`: a built-in's short name (``"gaussian-blur"``) or a full id,
    with its parameters (and named image inputs, as nodes) by name."""
    return cast("dict[str, Any]", _plain({"effect": effect, **params}))


def Grid(
    children: Iterable[Node],
    *,
    columns: int | None = None,
    gap: float | None = None,
    length: GridLength | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    stagger: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
    inset: float | abc.Sequence[float] | None = None,
) -> Node:
    """Children in grid cells, row by row. ``length`` is ``shortest`` (default),
    ``longest_freeze`` or ``longest_loop``; ``stagger`` works as on :func:`Stack`."""
    return _node(
        "grid",
        children=list(children),
        columns=columns,
        gap=gap,
        length=length,
        enter=enter,
        exit=exit,
        stagger=stagger,
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
    preserve_pitch: bool | None = None,
    loop: bool | None = None,
    volume: float | None = None,
    muted: bool | None = None,
    opacity: float | None = None,
    fade_in: float | None = None,
    fade_out: float | None = None,
    zoom: float | Mapping[str, Any] | None = None,
    when_short: str | None = None,
    when_long: str | None = None,
    blur: float | None = None,
    label: str | None = None,
    fit: Fit | None = None,
    align: Align | None = None,
    inset: float | abc.Sequence[float] | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A video or still file. ``fit`` is ``contain`` (default), ``cover``, ``fill`` or ``none``;
    ``zoom`` is a multiple or its change (:func:`Ramp`, :func:`Keys`); ``label`` names its cell
    in a comparison grid. ``when_short`` (``freeze``, ``loop``, ``stretch``, ``blank``,
    ``error``) and ``when_long`` (``trim``, ``stretch``, ``error``) say what a video does when its
    node outlasts it or it outlasts its node. At a
    ``speed`` other than 1 its sound keeps its pitch unless ``preserve_pitch`` is false."""
    return _node(
        "media",
        **_rename_loop(
            {
                "path": path,
                "kind": kind,
                "trim_start": trim_start,
                "trim_end": trim_end,
                "speed": speed,
                "preserve_pitch": preserve_pitch,
                "loop_": loop,
                "volume": volume,
                "muted": muted,
                "opacity": opacity,
                "fade_in": fade_in,
                "fade_out": fade_out,
                "zoom": zoom,
                "when_short": when_short,
                "when_long": when_long,
                "blur": blur,
                "label": label,
                "fit": fit,
                "align": align,
                "inset": inset,
                "enter": enter,
                "exit": exit,
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
    strokes: abc.Sequence[TextStroke] | None = None,
    shadows: abc.Sequence[TextShadow] | None = None,
    max_lines: int | None = None,
    fit: Mapping[str, float] | None = None,
    ellipsis: bool | str | None = None,
    opacity: float | None = None,
    fade: float | None = None,
    align: Align | None = None,
    inset: float | abc.Sequence[float] | None = None,
    reveal: Mapping[str, Any] | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Text in a theme style (``title``, ``subtitle``, ``body``, ``label``). It shrinks within
    ``fit`` (``{"min": px, "max": px}``) to fit its area and ``max_lines``, then lines past
    ``max_lines`` are cut and end with ``ellipsis``. ``reveal`` (:func:`Reveal`) shows it a word
    or line at a time."""
    return _node(
        "text",
        text=text,
        style=style,
        font_size=font_size,
        font_family=font_family,
        color=color,
        background=background,
        strokes=None if strokes is None else list(strokes),
        shadows=None if shadows is None else list(shadows),
        max_lines=max_lines,
        fit=None if fit is None else dict(fit),
        ellipsis=ellipsis,
        opacity=opacity,
        fade=fade,
        align=align,
        inset=inset,
        reveal=reveal,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


def Color(
    color: str | Rgba | None = None,
    *,
    inset: float | abc.Sequence[float] | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """An area painted one color (default: the theme's surface)."""
    return _node(
        "color",
        color=color,
        inset=inset,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


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
    highlight: str | Rgba | None = None,
    speakers: Mapping[str, SpeakerLook] | None = None,
    position: float | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """Timed text from a SubRip or WebVTT path or text, a list of ``{"start", "end", "text"}`` cues
    (optional ``speaker``), or word timestamps (``words``). ``style`` is ``outline`` (default),
    ``boxed``, ``bar-bottom``, ``bold-center``, ``bold``, ``karaoke`` or ``pop``; the last two
    highlight words in ``highlight`` (default: the theme accent) and need word times.
    ``speakers`` gives each speaker's ``color`` and ``position``."""
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
        highlight=highlight,
        speakers=dict(speakers) if speakers is not None else None,
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
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A layout generator's root box, as the project writes it
    (:class:`alpha2.types.LayoutNode`)."""
    return _node(
        "layout",
        root=root,
        align=align,
        inset=inset,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


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
    enter: Mapping[str, Any] | None,
    exit: Mapping[str, Any] | None,
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
        enter=enter,
        exit=exit,
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
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A title, and a subtitle under it, over what is under them."""
    return _title("title", text, subtitle, align, fade, None, enter, exit, id, at, duration)


def TitleCard(
    text: str,
    *,
    subtitle: str | None = None,
    align: Align | None = None,
    fade: float | None = None,
    background: str | Rgba | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A title on the theme's surface (or ``background``)."""
    return _title(
        "title_card", text, subtitle, align, fade, background, enter, exit, id, at, duration
    )


def LowerThird(
    text: str,
    *,
    subtitle: str | None = None,
    fade: float | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A name, and a role under it, in the lower left."""
    return _node(
        "lower_third",
        text=text,
        subtitle=subtitle,
        fade=fade,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


def EndCard(
    text: str,
    *,
    subtitle: str | None = None,
    logo: PathLike | None = None,
    background: str | Rgba | None = None,
    fade: float | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
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
        enter=enter,
        exit=exit,
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
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
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
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


def Counter(
    start: float,
    end: float,
    *,
    count_duration: float | None = None,
    easing: str | Mapping[str, Any] | None = None,
    decimals: int | None = None,
    thousands: str | None = None,
    point: str | None = None,
    prefix: str | None = None,
    suffix: str | None = None,
    style: TextRole | None = None,
    font_size: float | None = None,
    font_family: str | None = None,
    color: str | Rgba | None = None,
    strokes: abc.Sequence[TextStroke] | None = None,
    shadows: abc.Sequence[TextShadow] | None = None,
    opacity: float | None = None,
    align: Align | None = None,
    inset: float | abc.Sequence[float] | None = None,
    enter: Mapping[str, Any] | None = None,
    exit: Mapping[str, Any] | None = None,
    id: str | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> Node:
    """A number counting from ``start`` to ``end`` over ``count_duration`` seconds (default: the
    node's length), changing on the frame grid and written with ``decimals``, a ``thousands``
    separator, a decimal ``point``, a ``prefix`` and a ``suffix``. Text style defaults to
    ``title``."""
    number_format = _plain(
        {
            "decimals": decimals,
            "thousands": thousands,
            "point": point,
            "prefix": prefix,
            "suffix": suffix,
        }
    )
    return _node(
        "counter",
        start=start,
        end=end,
        count_duration=count_duration,
        easing=easing,
        format=number_format or None,
        style=style,
        font_size=font_size,
        font_family=font_family,
        color=color,
        strokes=None if strokes is None else list(strokes),
        shadows=None if shadows is None else list(shadows),
        opacity=opacity,
        align=align,
        inset=inset,
        enter=enter,
        exit=exit,
        id=id,
        at=at,
        duration=duration,
    )


def _motion(kind: str, /, **fields: Any) -> dict[str, Any]:
    return cast("dict[str, Any]", _plain({"type": kind, **fields}))


def Fade(
    duration: float | None = None, *, easing: str | Mapping[str, Any] | None = None
) -> dict[str, Any]:
    """A fade, for a node's ``enter`` or ``exit``: ``duration`` seconds (default: the theme's
    fade) along ``easing`` (a CSS name such as ``"ease-out"``, or an easing; default linear)."""
    return _motion("fade", duration=duration, easing=easing)


def Rise(
    duration: float | None = None,
    *,
    distance: float | None = None,
    fade: bool | None = None,
    easing: str | Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """A slide up by ``distance`` pixels (default: the theme's margin) that also fades unless
    ``fade`` is false; an exit keeps rising out."""
    return _motion("rise", duration=duration, distance=distance, fade=fade, easing=easing)


def Slide(
    direction: str,
    duration: float | None = None,
    *,
    distance: float | None = None,
    fade: bool | None = None,
    easing: str | Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """A slide toward ``direction`` (``up``, ``down``, ``left``, ``right``): an entrance arrives
    from the opposite side, an exit leaves that way."""
    return _motion(
        "slide",
        direction=direction,
        duration=duration,
        distance=distance,
        fade=fade,
        easing=easing,
    )


def Scale(
    duration: float | None = None,
    *,
    size: float | None = None,
    fade: bool | None = None,
    easing: str | Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """A change of size about the node's own anchor: an entrance grows from ``size`` times its
    own (default 0), an exit shrinks to it."""
    return _motion("scale", duration=duration, size=size, fade=fade, easing=easing)


def Ramp(
    from_: Any,
    to: Any,
    *,
    ease: str | Mapping[str, Any] | None = None,
    at: float | None = None,
    duration: float | None = None,
) -> dict[str, Any]:
    """A value changing from ``from_`` to ``to``, starting ``at`` seconds into its node and lasting
    ``duration`` (default: to the node's end), along ``ease`` (default linear)."""
    return cast(
        "dict[str, Any]",
        _plain({"from": from_, "to": to, "ease": ease, "at": at, "duration": duration}),
    )


def Keys(*keys: tuple[float, Any] | tuple[float, Any, str | Mapping[str, Any]]) -> dict[str, Any]:
    """A value through keys ``(seconds, value)`` or ``(seconds, value, ease)``, in increasing
    time; each ``ease`` curves the change to the next key."""
    return {
        "keys": [
            cast(
                "dict[str, Any]",
                _plain({"t": key[0], "v": key[1], "ease": key[2] if len(key) > 2 else None}),
            )
            for key in keys
        ]
    }


def Reveal(
    by: str,
    *,
    kind: str | None = None,
    stagger: float | None = None,
    duration: float | None = None,
    distance: float | None = None,
) -> dict[str, Any]:
    """A :func:`Text` ``reveal`` by ``word`` or ``line``: each unit fades in (``kind="rise"``
    also moves it up ``distance`` pixels) over ``duration`` seconds, ``stagger`` seconds after the
    one before."""
    return cast(
        "dict[str, Any]",
        _plain(
            {
                "by": by,
                "type": kind,
                "stagger": stagger,
                "duration": duration,
                "distance": distance,
            }
        ),
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
    """A compiled spec. :func:`alpha2.export` accepts it as a project, with ``assets``, ``fonts``
    and ``loudness`` as defaults. ``project`` is a plain ``dict``."""

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
    try:
        return cast("dict[str, Any]", json.loads(_native.compose_requirements(_spec_json(spec))))
    except ValueError as error:
        raise _compose_error(error) from None


def _spec_json(spec: Mapping[str, Any] | Video) -> str:
    document = spec.spec if isinstance(spec, Video) else _plain(spec)
    return json.dumps(document, ensure_ascii=False, allow_nan=False)


def _media_facts(fingerprint: Mapping[str, Any]) -> dict[str, Any]:
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
    """Facts about ``spec``'s files not already in ``known``: media via the host's ``conform`` (no
    GPU), caption files read as UTF-8.

    Raises :class:`~alpha2.Alpha2ComposeError` for a file the host cannot read.
    """
    facts: dict[str, Any] = {
        "media": dict((known or {}).get("media", {})),
        "texts": dict((known or {}).get("texts", {})),
    }
    needed = requirements(spec)
    media = [entry for entry in needed["media"] if entry["path"] not in facts["media"]]
    if media:
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
            try:
                # Normalize line ends so CRLF files compile the same.
                text = read_user_text(path, "text file")
                facts["texts"][path] = text.replace("\r\n", "\n").replace("\r", "\n")
            except UserTextError as error:
                raise Alpha2ComposeError(
                    f"facts.texts: {error}", "facts.texts", str(error)
                ) from None
            except OSError as error:
                raise Alpha2ComposeError(
                    f'facts.texts: cannot read "{path}": {error}',
                    "facts.texts",
                    f'cannot read "{path}"',
                ) from None
    return facts


TEMPLATES: tuple[dict[str, Any], ...] = tuple(json.loads(_native.COMPOSE_TEMPLATES))
"""The 1st-party ``alpha2/`` templates; any spec may name them as node types."""


def bind(
    template: Mapping[str, Any],
    data: Mapping[str, Any] | None = None,
    *,
    theme: str | Mapping[str, Any] | None = None,
    templates: abc.Sequence[Mapping[str, Any]] = (),
) -> dict[str, Any]:
    """``template`` bound to ``data``: its parameters replaced and its ``owner/name`` template
    nodes (from ``templates`` and :data:`TEMPLATES`) expanded, giving a plain spec (or node).
    ``theme`` replaces a video template's theme. A spec without a ``template`` header only has
    its template nodes expanded.

    Raises :class:`~alpha2.Alpha2ComposeError` naming where the template or data does not bind.
    """
    options = {"templates": [_plain(entry) for entry in templates]}
    if theme is not None:
        options["theme"] = _plain(theme)
    try:
        bound = _native.compose_bind(
            json.dumps(_plain(template), ensure_ascii=False, allow_nan=False),
            json.dumps(_plain(data or {}), ensure_ascii=False, allow_nan=False),
            json.dumps(options, ensure_ascii=False, allow_nan=False),
        )
    except ValueError as error:
        raise _compose_error(error) from None
    return cast("dict[str, Any]", json.loads(bound))


def data_schema(template: Mapping[str, Any]) -> dict[str, Any]:
    """The JSON Schema of ``template``'s data: what a form, an API or a model fills in.

    Raises :class:`~alpha2.Alpha2ComposeError` for a template that does not parse.
    """
    try:
        schema = _native.compose_data_schema(json.dumps(_plain(template), ensure_ascii=False))
    except ValueError as error:
        raise _compose_error(error) from None
    return cast("dict[str, Any]", json.loads(schema))


def lint(
    template: Mapping[str, Any],
    data: Mapping[str, Any] | None = None,
    *,
    templates: abc.Sequence[Mapping[str, Any]] = (),
) -> list[dict[str, Any]]:
    """``template``'s problems as ``{path, code, message, severity}``: unknown or unused
    parameters, literal files, colors and words a template takes from its data and theme, and
    whether it binds (to ``data``, or to sample data) into a spec that parses."""
    issues = _native.compose_lint(
        json.dumps(_plain(template), ensure_ascii=False),
        "" if data is None else json.dumps(_plain(data), ensure_ascii=False),
        json.dumps({"templates": [_plain(entry) for entry in templates]}, ensure_ascii=False),
    )
    return cast("list[dict[str, Any]]", json.loads(issues))


def compile(  # noqa: A001 - the module's verb, as `re.compile`
    spec: Mapping[str, Any] | Video,
    *,
    facts: Mapping[str, Any] | None = None,
    fonts: abc.Sequence[PathLike] | None = None,
    client: _client.HostClient | None = None,
    templates: abc.Sequence[Mapping[str, Any]] = (),
) -> Compiled:
    """Compiles ``spec`` into a project.

    ``facts`` describes files the spec names (``{"media": {path: {width, height, frame_rate,
    duration_ticks, has_audio}}, "texts": {path: text}}``); missing ones are gathered with
    :func:`gather_facts`, which needs a host. ``fonts`` measure text that must fit and pass to the
    export via :attr:`Compiled.fonts`.

    The spec may name templates (``owner/name`` node types) from ``templates`` and
    :data:`TEMPLATES`; bind a template's data with :func:`bind` first.

    Raises :class:`~alpha2.Alpha2ComposeError` naming where in the spec it does not compile.
    """
    if templates:
        spec = bind(json.loads(_spec_json(spec)), templates=templates)
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
    LUFS (under -1 dBTP) or ``{"target_lufs", "true_peak_ceiling_dbtp"}``. :attr:`spec` is the
    plain spec.
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
