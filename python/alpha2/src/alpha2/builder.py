"""A builder for project documents: sources, tracks, clips and caption cues.

Times are in seconds (or exact :func:`ticks`) and ids are generated deterministically::

    project = ProjectBuilder("Intro", width=1920, height=1080, frame_rate=30)
    video = project.source("video", "intro.mp4")
    project.video_track().clip(video, at=0, duration=5, source_in=12)
    document = project.build()  # a valid project.v1 dict, or ValueError

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
        ClipEffect,
        CompositionNode,
        Easing,
        Generator,
        Project,
        Rgb,
        TextStyle,
    )

__all__ = [
    "TICKS_PER_SECOND",
    "CaptionTrackBuilder",
    "IdGenerator",
    "IdKind",
    "ProjectBuilder",
    "Ticks",
    "Time",
    "TrackBuilder",
    "seconds_to_ticks",
    "sequential_ids",
    "ticks",
    "ticks_to_seconds",
    "to_ticks",
    "validate_project",
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
    """Seconds as the nearest tick, halves away from zero.

    Seconds rarely hold a tick exactly (1.001 s is just under tick 120120), so rounding is what
    gives a time back its own tick: ``seconds_to_ticks(ticks_to_seconds(t)) == t``.
    """
    scaled = seconds * TICKS_PER_SECOND
    return int(math.copysign(math.floor(abs(scaled) + 0.5), scaled))


def ticks_to_seconds(count: int) -> float:
    """Timeline ticks as seconds."""
    return count / TICKS_PER_SECOND


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
    """What the engine refuses in ``project``: empty when it is valid.

    It runs the engine's own validator, the one the host applies, so the messages are the host's.
    """
    try:
        text = json.dumps(project, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
    except (TypeError, ValueError) as error:
        return [f"project cannot be written as JSON: {error}"]
    return list(_native.validate_project_json(text))


IdKind = Literal["source", "track", "clip", "composition"]
"""What a generated id names."""

IdGenerator = Callable[[IdKind], str]
"""Makes ids: called with what the id names, in build order. A builder skips an id it already holds
of that kind, given explicitly or generated, and calls again."""


def sequential_ids() -> IdGenerator:
    """``source-1``, ``track-1``, ``clip-1``, ..., counted per kind."""
    counts: dict[str, int] = {}

    def next_id(kind: IdKind) -> str:
        counts[kind] = counts.get(kind, 0) + 1
        return f"{kind}-{counts[kind]}"

    return next_id


# Clip fields the builder sets itself, which `fields` may not name.
_PLACED_FIELDS = frozenset({"id", "source_id", "timeline_range", "source_range"})


class ProjectBuilder:
    """Builds one project document. Nothing is checked until :meth:`build` validates the whole.

    ``frame_rate`` is frames per second or an exact rational ``(num, den)``, such as
    ``(30000, 1001)``. ``ids`` is how generated ids are made (default :func:`sequential_ids`); an id
    the builder already holds is skipped, so explicit ids such as ``clip-2`` and generated ones mix
    freely.
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
    ) -> CaptionTrackBuilder:
        """Adds a caption track in ``style`` (the defaults where absent)."""
        track_id = self._claim("track", id)
        track = CaptionTrackBuilder(track_id, name or track_id, style or {})
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
        """``explicit``, or the generator's next id of ``kind`` this builder does not hold yet.

        Either way the id is held from now on. An explicit id is taken as given: a repeat is for
        :meth:`build` to report. Raises ``ValueError`` when the generator offers only ids already
        held.
        """
        used = self._used.setdefault(kind, set())
        if explicit is not None:
            used.add(explicit)
            return explicit
        # A generator that counts reaches a free id within one call per id held.
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
        **fields: Any,
    ) -> str:
        """Places a clip of ``source``; returns its id.

        ``source_duration`` defaults to ``duration`` (untouched speed); twice ``duration`` plays
        twice as fast. With ``effects``, a composition chaining them is added to the project and
        the clip shows it. ``fields`` are any other clip fields: ``visual``, ``composition``,
        ``blend_mode``, ``masks``, and so on.

        Raises ``ValueError`` for ``effects`` together with a ``composition``, a field the builder
        sets itself, or a time that is not finite; nothing is added then.
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

    def __init__(self, track_id: str, name: str, style: TextStyle) -> None:
        self._id = track_id
        self._name = name
        self._style = style
        self._cues: list[dict[str, Any]] = []

    @property
    def id(self) -> str:
        """The track's id."""
        return self._id

    def cue(self, text: str, *, at: Time, duration: Time) -> CaptionTrackBuilder:
        """Shows ``text`` from ``at`` for ``duration``. Cues may be added in any order."""
        start = to_ticks(at)
        self._cues.append(
            {"timeline_range": {"start": start, "end": start + to_ticks(duration)}, "text": text}
        )
        return self

    def _build(self) -> dict[str, Any]:
        cues = sorted(self._cues, key=lambda cue: cue["timeline_range"]["start"])
        return {
            "id": self._id,
            "name": self._name,
            "kind": "caption",
            "clips": [],
            "captions": {"style": self._style, "cues": cues},
        }
