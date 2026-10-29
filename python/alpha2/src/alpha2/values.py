"""Animated values, as a project document writes them::

    from alpha2 import constant, key, keyframes

    opacity = keyframes(key(0, 0.0), key(0.5, 1.0))   # fades in over half a second
    width = constant(0.4)

Every animatable field of a document is an ``Animated``: ``{"mode": "constant", "value": ...}`` or
``{"mode": "keyframes", "keyframes": [...]}``. The builders (:func:`alpha2.visual`,
:mod:`alpha2.layout`, :mod:`alpha2.shapes`, :func:`alpha2.effect`) :func:`lift` a plain value into
a constant themselves, so a constant is only written by hand in a raw document.
"""

from __future__ import annotations

import itertools
from collections.abc import Callable, Mapping
from typing import TYPE_CHECKING, Any, Literal, TypeVar, cast, overload

from .builder import Time, to_ticks

if TYPE_CHECKING:
    from .types import (
        Animated,
        AnimatedConstant,
        AnimatedKeyframes,
        AnimatedPointKeyframes,
        Easing,
        Interpolation,
        Keyframe,
        PointKeyframe,
        Vec2,
    )

__all__ = [
    "InterpolationName",
    "Point",
    "constant",
    "is_animated",
    "key",
    "keyframes",
    "lift",
    "point",
    "point_key",
    "point_keyframes",
]

T = TypeVar("T")

InterpolationName = Literal["linear", "hold"]
"""An interpolation without parameters, by name."""

Point = tuple[float, float]
"""``(x, y)``, written ``{"x", "y"}``."""


def constant(value: T) -> AnimatedConstant[T]:
    """``value`` at every time."""
    return {"mode": "constant", "value": value}


def _interpolation(
    interpolation: InterpolationName | Interpolation, easing: Easing | None
) -> Interpolation:
    if easing is not None:
        if interpolation != "linear":
            raise ValueError("give an easing or an interpolation other than linear, not both")
        return cast("Interpolation", dict(easing))
    if interpolation == "linear" or interpolation == "hold":
        return cast("Interpolation", {"type": interpolation})
    if isinstance(interpolation, Mapping) and isinstance(interpolation.get("type"), str):
        return cast("Interpolation", dict(interpolation))
    raise ValueError(f"{interpolation!r} is not an interpolation")


def key(
    at: Time,
    value: T,
    interpolation: InterpolationName | Interpolation = "linear",
    easing: Easing | None = None,
) -> Keyframe[T]:
    """``value`` at time ``at`` (seconds, or exact :class:`~alpha2.Ticks`), and how the value moves
    on from here to the next key: ``"linear"``, ``"hold"``, or along ``easing`` (an ``Easing``,
    such as ``{"type": "cubic_bezier", "x1": 0.2, "y1": 0, "x2": 0, "y2": 1}``).

    ``at`` is in the time of whatever holds the value:

    ========================================== ================================================
    Owner                                      Keyed in
    ========================================== ================================================
    A clip's ``visual`` and masks, transitions Timeline time: 0 is the project's start
    A composition's settings (effect params)   Composition time: 0 is the clip's start
    Shapes and layout generators               Picture time: the source time a clip selects
    ========================================== ================================================

    A clip's transform is placed on the timeline, so it keys there. An effect belongs to the
    composition a clip shows, which moves with the clip. A generated picture is a source, its
    animation trimmed and placed by its clips as a video file's is. A key in the wrong base
    usually lies outside the time its owner is shown: :func:`alpha2.lint_project` reports
    keyframes no frame reaches (``unreached_keyframe``).
    """
    return {
        "time": to_ticks(at),
        "value": value,
        "interpolation": _interpolation(interpolation, easing),
    }


def keyframes(*keys: Keyframe[T]) -> AnimatedKeyframes[T]:
    """The value moving through ``keys``, which must be in strictly ascending time.

    Raises ``ValueError`` for no keys and for keys out of order or at one time: they are not
    sorted, because sorting would hide the mistake. The engine checks them again when it validates
    the document.
    """
    if not keys:
        raise ValueError("keyframes needs at least one key")
    for before, after in itertools.pairwise(keys):
        if after["time"] <= before["time"]:
            raise ValueError(
                f"keys must be in strictly ascending time: {after['time']} ticks follows "
                f"{before['time']}"
            )
    return {"mode": "keyframes", "keyframes": list(keys)}


def is_animated(value: object) -> bool:
    """Whether ``value`` is already an ``Animated``: a mapping whose ``mode`` is ``constant`` or
    ``keyframes``."""
    return isinstance(value, Mapping) and value.get("mode") in ("constant", "keyframes")


@overload
def lift(value: AnimatedConstant[T] | AnimatedKeyframes[T]) -> Animated[T]: ...


@overload
def lift(value: T) -> Animated[T]: ...


def lift(value: Any) -> Any:
    """``value`` as an ``Animated``: an ``Animated`` as it is, anything else as a constant."""
    if is_animated(value):
        return value
    return constant(value)


def map_values(value: Any, convert: Callable[[Any], Any]) -> Any:
    """``value`` lifted, with ``convert`` applied to the constant or to every keyframe's value."""
    if not is_animated(value):
        return {"mode": "constant", "value": convert(value)}
    if value["mode"] == "constant":
        return {**value, "value": convert(value["value"])}
    return {
        **value,
        "keyframes": [{**frame, "value": convert(frame["value"])} for frame in value["keyframes"]],
    }


def point(value: Point | Vec2) -> Vec2:
    """``(x, y)`` as ``{"x", "y"}``; a ``{"x", "y"}`` mapping as it is."""
    if isinstance(value, Mapping):
        return value
    if isinstance(value, (tuple, list)) and len(value) == 2:
        x, y = value
        return {"x": x, "y": y}
    raise ValueError(f"{value!r} is not a point: write (x, y)")


def point_key(
    at: Time,
    value: Point | Vec2,
    interpolation: InterpolationName | Interpolation = "linear",
    easing: Easing | None = None,
    *,
    in_tangent: Point | Vec2 | None = None,
    out_tangent: Point | Vec2 | None = None,
) -> PointKeyframe:
    """A :func:`key` of a position, which may also curve: with tangents, the position travels from
    one key along the cubic Bezier through its ``out_tangent`` and the next key's ``in_tangent``
    (each relative to its own key's value), instead of in a straight line."""
    frame: dict[str, Any] = dict(key(at, point(value), interpolation, easing))
    if in_tangent is not None:
        frame["in_tangent"] = point(in_tangent)
    if out_tangent is not None:
        frame["out_tangent"] = point(out_tangent)
    return cast("PointKeyframe", frame)


def point_keyframes(*keys: PointKeyframe) -> AnimatedPointKeyframes:
    """A position moving through ``keys`` (:func:`point_key`), as :func:`keyframes` checks them."""
    return cast("AnimatedPointKeyframes", keyframes(*cast("tuple[Keyframe[Vec2], ...]", keys)))
