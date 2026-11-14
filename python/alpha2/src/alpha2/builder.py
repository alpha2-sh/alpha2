"""A builder for project documents: sources, tracks, clips and caption cues.

Times are in seconds (or exact :func:`ticks`) and ids are generated deterministically::

    project = ProjectBuilder("Intro", width=1920, height=1080, frame_rate=30)
    video = project.source("video", "intro.mp4")
    project.video_track().clip(video, at=0, duration=5, source_in=12)
    document = project.build()  # a valid project.v1 dict, or ValueError
    alpha2.export(document, "out.mp4", assets=project.assets())

The result is a plain ``dict`` in the shape of :class:`alpha2.types.Project`: edit it freely.
"""

from __future__ import annotations

import json
import math
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal, cast

from . import _native

if TYPE_CHECKING:
    from .types import (
        Animated,
        AnimatedPoint,
        ClipEffect,
        CompositionNode,
        Easing,
        Generator,
        Project,
        Rgb,
        TextRun,
        TextStyle,
        TextStylePatch,
        Vec2,
        VisualProperties,
    )

__all__ = [
    "TICKS_PER_SECOND",
    "CaptionTrackBuilder",
    "Frames",
    "IdGenerator",
    "IdKind",
    "ProjectBuilder",
    "Ticks",
    "Time",
    "TrackBuilder",
    "effect",
    "frames",
    "lint_project",
    "seconds_to_ticks",
    "sequential_ids",
    "ticks",
    "ticks_to_seconds",
    "to_ticks",
    "validate_project",
    "visual",
]

TICKS_PER_SECOND: int = _native.TICKS_PER_SECOND
"""Timeline ticks in one second."""


@dataclass(frozen=True)
class Ticks:
    """An exact time in ticks, where seconds would round."""

    ticks: int


Time = float | Ticks
"""Seconds, or exact :class:`Ticks`."""


def ticks(count: int) -> Ticks:
    """``count`` ticks: an exact time. Raises ``ValueError`` unless ``count`` is an integer."""
    if not isinstance(count, int) or isinstance(count, bool):
        raise ValueError(f"ticks must be an integer, not {count!r}")
    return Ticks(count)


def seconds_to_ticks(seconds: float) -> int:
    """Seconds as the nearest tick, halves away from zero, so
    ``seconds_to_ticks(ticks_to_seconds(t)) == t``."""
    scaled = seconds * TICKS_PER_SECOND
    return int(math.copysign(math.floor(abs(scaled) + 0.5), scaled))


def ticks_to_seconds(count: int) -> float:
    """Timeline ticks as seconds."""
    return count / TICKS_PER_SECOND


@dataclass(frozen=True)
class Frames:
    """The frame grid of ``num / den`` frames per second, with frame starts floored to ticks as
    the engine places them. Make one with :func:`frames`."""

    num: int
    den: int

    def start(self, index: int) -> float:
        """Seconds at which frame ``index`` starts."""
        return ticks_to_seconds(self.start_ticks(index))

    def start_ticks(self, index: int) -> int:
        """Ticks at which frame ``index`` starts."""
        return index * TICKS_PER_SECOND * self.den // self.num

    def index(self, seconds: float) -> int:
        """The frame whose start is nearest ``seconds`` (a tie takes the later frame)."""
        scale = TICKS_PER_SECOND * self.den
        return (2 * seconds_to_ticks(seconds) * self.num + scale) // (2 * scale)

    def snap(self, seconds: float) -> float:
        """``seconds`` moved to the nearest frame start."""
        return self.start(self.index(seconds))

    def last(self, end: float) -> float:
        """The start of the last frame shown before an exclusive ``end`` in seconds."""
        before = seconds_to_ticks(end) - 1
        index = -(-(before + 1) * self.num // (TICKS_PER_SECOND * self.den)) - 1
        return self.start(index)


def frames(rate: int | tuple[int, int] | Mapping[str, int]) -> Frames:
    """The frame grid of ``rate``: whole frames per second, ``(num, den)`` or ``{"num", "den"}``
    (``(30000, 1001)`` for 29.97). Raises ``ValueError`` for a rate that is not positive."""
    if isinstance(rate, Mapping):
        num, den = rate.get("num"), rate.get("den")
    elif isinstance(rate, tuple):
        num, den = rate
    else:
        num, den = rate, 1
    valid = all(isinstance(part, int) and not isinstance(part, bool) for part in (num, den))
    if not valid or cast("int", num) <= 0 or cast("int", den) <= 0:
        raise ValueError(f"{rate!r} is not a frame rate: write fps, (num, den) or {{num, den}}")
    return Frames(cast("int", num), cast("int", den))


def to_ticks(time: Time) -> int:
    """A time in ticks: seconds rounded to the nearest tick. Raises ``ValueError`` for seconds that
    are not finite."""
    if isinstance(time, Ticks):
        return time.ticks
    if isinstance(time, bool) or not isinstance(time, (int, float)):
        raise ValueError(f"{time!r} is not a time")
    if not math.isfinite(time):
        raise ValueError(f"{time} is not a time")
    return seconds_to_ticks(time)


def validate_project(project: Mapping[str, Any]) -> list[str]:
    """What the engine's validator refuses in ``project``; empty when it is valid."""
    try:
        text = json.dumps(project, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
    except (TypeError, ValueError) as error:
        return [f"project cannot be written as JSON: {error}"]
    return list(_native.validate_project_json(text))


def lint_project(project: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Likely mistakes in a valid ``project``, from the engine's ``Project::lint``. Each issue is a
    dict whose ``kind`` names it, such as ``unreached_keyframe`` (often a keyframe in the wrong
    time base), ``hidden_clip`` or ``short_transition``. Issues never block an export.

    Raises ``ValueError`` naming the problem for a project that does not validate.
    """
    problems = validate_project(project)
    if problems:
        raise ValueError(f"invalid project: {'; '.join(problems)}")
    text = json.dumps(project, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
    return cast("list[dict[str, Any]]", json.loads(_native.lint_project_json(text)))


def visual(
    *,
    position: tuple[float, float]
    | Vec2
    | AnimatedPoint
    | Animated[tuple[float, float]]
    | None = None,
    anchor: tuple[float, float] | Vec2 | None = None,
    scale: float
    | tuple[float, float]
    | Vec2
    | Animated[Vec2]
    | Animated[float]
    | Animated[tuple[float, float]]
    | None = None,
    rotation: float | Animated[float] | None = None,
    opacity: float | Animated[float] | None = None,
) -> VisualProperties:
    """A clip's ``visual`` with only the given fields; the rest default to centred, unscaled,
    unrotated and opaque.

    ``position`` (normalized canvas coordinates) and ``anchor`` (normalized source coordinates)
    take ``(x, y)``; ``scale`` takes a number or ``(x, y)``; ``rotation`` is clockwise degrees.
    Every field but ``anchor`` may be animated (:func:`alpha2.keyframes`, in timeline time).
    """
    from .values import map_values, point  # here: values imports this module

    def scaled(value: Any) -> Vec2:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return {"x": value, "y": value}
        return point(value)

    properties: dict[str, Any] = {}
    if position is not None:
        properties["position"] = map_values(position, point)
    if anchor is not None:
        properties["anchor"] = point(anchor)
    if scale is not None:
        properties["scale"] = map_values(scale, scaled)
    if rotation is not None:
        properties["rotation_degrees"] = map_values(rotation, lambda degrees: degrees)
    if opacity is not None:
        properties["opacity"] = map_values(opacity, lambda alpha: alpha)
    return cast("VisualProperties", properties)


def effect(
    id: str,
    /,
    version: int = 1,
    *,
    resources: Mapping[str, str] | None = None,
    **params: Any,
) -> ClipEffect:
    """An invocation of effect ``id`` at definition ``version`` (built-ins are at 1), for
    :meth:`TrackBuilder.clip`'s ``effects``, :meth:`TrackBuilder.transition` or a composition's
    effect node.

    ``params`` go through :func:`alpha2.fx.param`: an ``fx`` value or an unambiguous plain value
    (``float``, ``bool``, ``(x, y)``, hex color), constant or keyed (composition time in clip
    effects, timeline time in transitions). Omitted parameters take their defaults. ``resources``
    maps the effect's external resources (a LUT) to project resource ids.
    """
    from . import effect_values

    invocation: dict[str, Any] = {"id": id, "version": version}
    if params:
        invocation["params"] = {name: effect_values.param(value) for name, value in params.items()}
    if resources:
        invocation["resources"] = dict(resources)
    return cast("ClipEffect", invocation)


IdKind = Literal["source", "track", "clip", "composition"]
"""What a generated id names."""

IdGenerator = Callable[[IdKind], str]
"""Makes ids, called with the kind in build order. A builder skips ids it already holds."""


def sequential_ids() -> IdGenerator:
    """``source-1``, ``track-1``, ``clip-1``, ..., counted per kind."""
    counts: dict[str, int] = {}

    def next_id(kind: IdKind) -> str:
        counts[kind] = counts.get(kind, 0) + 1
        return f"{kind}-{counts[kind]}"

    return next_id


_PLACED_FIELDS = frozenset({"id", "source_id", "timeline_range", "source_range"})


class ProjectBuilder:
    """Builds one project document. Nothing is checked until :meth:`build` validates the whole.

    ``frame_rate`` is frames per second or an exact rational ``(num, den)``, such as
    ``(30000, 1001)``. ``ids`` makes generated ids (default :func:`sequential_ids`); explicit and
    generated ids mix freely.
    """

    def __init__(
        self,
        name: str,
        *,
        width: int,
        height: int,
        frame_rate: int | tuple[int, int],
        id: str = "project",
        sample_rate: int = 48_000,
        background: Rgb | None = None,
        ids: IdGenerator | None = None,
    ) -> None:
        num, den = frame_rate if isinstance(frame_rate, tuple) else (frame_rate, 1)
        self._settings: dict[str, Any] = {
            "width": width,
            "height": height,
            "frame_rate": {"num": num, "den": den},
            "sample_rate": sample_rate,
        }
        if background is not None:
            self._settings["background"] = background
        self._id = id
        self._name = name
        self._ids = ids or sequential_ids()
        self._used: dict[str, set[str]] = {}
        self._sources: list[dict[str, Any]] = []
        self._compositions: list[dict[str, Any]] = []
        self._tracks: list[TrackBuilder | CaptionTrackBuilder] = []

    def source(
        self,
        kind: Literal["video", "audio", "image"],
        locator: str,
        *,
        id: str | None = None,
        has_audio: bool | None = None,
    ) -> str:
        """Adds a media source the host resolves; returns its id."""
        source_id = self._claim("source", id)
        source: dict[str, Any] = {"id": source_id, "kind": kind, "locator": locator}
        if has_audio is not None:
            source["has_audio"] = has_audio
        self._sources.append(source)
        return source_id

    def assets(self) -> dict[str, str]:
        """Each media source's id mapped to its locator, for an export's ``assets``. Relative paths
        resolve against the working directory at the time of the call."""
        return {source["id"]: source["locator"] for source in self._sources if "locator" in source}

    def generated(self, generator: Generator, *, id: str | None = None) -> str:
        """Adds a source the engine draws itself; returns its id."""
        source_id = self._claim("source", id)
        self._sources.append({"id": source_id, "kind": "generated", "generator": generator})
        return source_id

    def composition(
        self,
        nodes: Sequence[CompositionNode],
        output: str | None,
        *,
        id: str | None = None,
    ) -> str:
        """Adds a composition graph for clips to show (a clip's ``composition`` field); returns its
        id."""
        composition_id = self._claim("composition", id)
        composition: dict[str, Any] = {"id": composition_id, "nodes": list(nodes)}
        if output is not None:
            composition["output"] = output
        self._compositions.append(composition)
        return composition_id

    def video_track(self, *, id: str | None = None, name: str | None = None) -> TrackBuilder:
        """Adds a visual track, drawn over the tracks added before it."""
        return self._track("video", id, name)

    def audio_track(self, *, id: str | None = None, name: str | None = None) -> TrackBuilder:
        """Adds an audio track."""
        return self._track("audio", id, name)

    def caption_track(
        self,
        *,
        id: str | None = None,
        name: str | None = None,
        style: TextStyle | None = None,
        styles: Mapping[str, TextStylePatch] | None = None,
    ) -> CaptionTrackBuilder:
        """Adds a caption track in ``style`` (the defaults where absent), with named ``styles``
        its cues take by ``style``, or by ``speaker`` when a style has the speaker's name."""
        track_id = self._claim("track", id)
        track = CaptionTrackBuilder(track_id, name or track_id, style or {}, dict(styles or {}))
        self._tracks.append(track)
        return track

    def build(self) -> Project:
        """The document. Raises ``ValueError`` naming the problem when it does not validate."""
        project: dict[str, Any] = {
            "schema_version": _native.SCHEMA_VERSION,
            "id": self._id,
            "name": self._name,
            "settings": dict(self._settings),
            "sources": list(self._sources),
        }
        if self._compositions:
            project["compositions"] = list(self._compositions)
        project["timeline"] = {"tracks": [track._build() for track in self._tracks]}
        problems = validate_project(project)
        if problems:
            raise ValueError(f"invalid project: {'; '.join(problems)}")
        return cast("Project", project)

    def _track(
        self, kind: Literal["video", "audio"], id: str | None, name: str | None
    ) -> TrackBuilder:
        track_id = self._claim("track", id)
        track = TrackBuilder(track_id, name or track_id, kind, self)
        self._tracks.append(track)
        return track

    def _effect_chain(self, effects: Sequence[ClipEffect]) -> str:
        """A composition of the clip node through one effect node each; returns its id."""
        nodes: list[dict[str, Any]] = [{"id": "clip", "op": {"type": "clip"}}]
        for index, effect in enumerate(effects):
            nodes.append(
                {
                    "id": f"effect-{index}",
                    "op": {
                        "type": "effect",
                        "input": "clip" if index == 0 else f"effect-{index - 1}",
                        "effect": effect,
                    },
                }
            )
        return self.composition(cast("list[CompositionNode]", nodes), nodes[-1]["id"])

    def _claim(self, kind: IdKind, explicit: str | None) -> str:
        """``explicit``, or the generator's next unheld id of ``kind``; repeats of an explicit id
        are left for :meth:`build` to report."""
        used = self._used.setdefault(kind, set())
        if explicit is not None:
            used.add(explicit)
            return explicit
        candidate = ""
        for _ in range(len(used) + 1):
            candidate = self._ids(kind)
            if candidate not in used:
                used.add(candidate)
                return candidate
        raise ValueError(
            f"the id generator offered only {kind} ids already in use, last {candidate}"
        )


class TrackBuilder:
    """Clips and transitions of one visual or audio track."""

    def __init__(
        self, track_id: str, name: str, kind: Literal["video", "audio"], project: ProjectBuilder
    ) -> None:
        self._id = track_id
        self._name = name
        self._kind = kind
        self._project = project
        self._clips: list[dict[str, Any]] = []
        self._transitions: list[dict[str, Any]] = []

    @property
    def id(self) -> str:
        """The track's id."""
        return self._id

    def clip(
        self,
        source: str,
        *,
        at: Time,
        duration: Time,
        source_in: Time = 0,
        source_duration: Time | None = None,
        id: str | None = None,
        effects: Sequence[ClipEffect] | None = None,
        preserve_pitch: bool | None = None,
        **fields: Any,
    ) -> str:
        """Places a clip of ``source``; returns its id.

        ``source_duration`` defaults to ``duration``; twice ``duration`` plays twice as fast, and
        ``preserve_pitch`` then time-stretches instead of resampling. ``effects``
        (:func:`alpha2.effect`) adds a composition chaining them. ``fields`` are other clip fields:
        ``visual`` (:func:`alpha2.visual`), ``composition``, ``blend_mode``, ``masks``, and so on.

        Keyframe time bases: ``visual`` and ``masks`` in timeline time; ``effects`` in composition
        time, from the clip's start; a generated source's shapes or layout in picture time.

        Raises ``ValueError``, adding nothing, for ``effects`` with a ``composition``, a field the
        builder sets itself, or a non-finite time.
        """
        placed = _PLACED_FIELDS.intersection(fields)
        if placed:
            raise ValueError(f"clip sets {', '.join(sorted(placed))} itself")
        if effects and fields.get("composition") is not None:
            raise ValueError("a clip shows one composition: give effects or composition, not both")
        start = to_ticks(at)
        length = to_ticks(duration)
        source_start = to_ticks(source_in)
        source_length = to_ticks(source_duration if source_duration is not None else duration)
        clip_id = self._project._claim("clip", id)
        clip: dict[str, Any] = dict(fields)
        if effects:
            clip["composition"] = {"id": self._project._effect_chain(effects)}
        if preserve_pitch is not None:
            clip["audio"] = {**clip.get("audio", {}), "preserve_pitch": preserve_pitch}
        clip.update(
            {
                "id": clip_id,
                "source_id": source,
                "timeline_range": {"start": start, "end": start + length},
                "source_range": {"start": source_start, "end": source_start + source_length},
            }
        )
        self._clips.append(clip)
        return clip_id

    def transition(
        self,
        from_clip: str,
        to_clip: str,
        effect: ClipEffect,
        easing: Easing | None = None,
    ) -> TrackBuilder:
        """A transition from ``from_clip`` to ``to_clip`` over the time the two overlap, which must
        be where they meet.

        Raises ``ValueError`` when either clip is not on this track. An overlap that is not valid
        is reported by :meth:`ProjectBuilder.build`.
        """
        first = next((clip for clip in self._clips if clip["id"] == from_clip), None)
        second = next((clip for clip in self._clips if clip["id"] == to_clip), None)
        if first is None or second is None:
            raise ValueError(
                f"transition {from_clip} -> {to_clip} names a clip not on track {self._id}"
            )
        self._transitions.append(
            {
                "from_clip": from_clip,
                "to_clip": to_clip,
                "timeline_range": {
                    "start": second["timeline_range"]["start"],
                    "end": first["timeline_range"]["end"],
                },
                "easing": easing if easing is not None else {"type": "linear"},
                "effect": effect,
            }
        )
        return self

    def _build(self) -> dict[str, Any]:
        return {
            "id": self._id,
            "name": self._name,
            "kind": self._kind,
            "clips": list(self._clips),
            "transitions": list(self._transitions),
        }


class CaptionTrackBuilder:
    """Cues of one caption track."""

    def __init__(
        self,
        track_id: str,
        name: str,
        style: TextStyle,
        styles: dict[str, TextStylePatch],
    ) -> None:
        self._id = track_id
        self._name = name
        self._style = style
        self._styles = styles
        self._cues: list[dict[str, Any]] = []

    @property
    def id(self) -> str:
        """The track's id."""
        return self._id

    def cue(
        self,
        text: str | Sequence[TextRun],
        *,
        at: Time,
        duration: Time,
        speaker: str | None = None,
        style: str | None = None,
    ) -> CaptionTrackBuilder:
        """Shows ``text`` from ``at`` for ``duration``. ``text`` is a string or runs (a run's
        ``time`` is ticks from the cue's start, for karaoke). ``style`` names one of the track's
        styles. Cues may be added in any order."""
        start = to_ticks(at)
        cue: dict[str, Any] = {
            "timeline_range": {"start": start, "end": start + to_ticks(duration)},
            "text": text if isinstance(text, str) else [dict(run) for run in text],
        }
        if speaker is not None:
            cue["speaker"] = speaker
        if style is not None:
            cue["style"] = style
        self._cues.append(cue)
        return self

    def _build(self) -> dict[str, Any]:
        cues = sorted(self._cues, key=lambda cue: cue["timeline_range"]["start"])
        return {
            "id": self._id,
            "name": self._name,
            "kind": "caption",
            "clips": [],
            "captions": {
                "style": self._style,
                **({"styles": dict(self._styles)} if self._styles else {}),
                "cues": cues,
            },
        }
