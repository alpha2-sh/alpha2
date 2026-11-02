"""Animated values, as a project document writes them::

    from alpha2 import constant, key, keyframes

    opacity = keyframes(key(0, 0.0), key(0.5, 1.0))   # fades in over half a second
    width = constant(0.4)

Every animatable field is an ``Animated``: ``{"mode": "constant", "value": ...}`` or
``{"mode": "keyframes", "keyframes": [...]}``. The builders :func:`lift` plain values to constants
themselves.
"""

from __future__ import annotations

import itertools
from collections.abc import Callable, Mapping
from typing import TYPE_CHECKING, Any, Literal, TypeVar, cast, overload

from .builder import Time, to_ticks

if TYPE_CHECKING:
    from collections.abc import Sequence

    from .types import (
        Animated,
        AnimatedConstant,
        AnimatedKeyframes,
        AnimatedPointKeyframes,
        Easing,
        GradientStop,
        Interpolation,
        Keyframe,
        PaintLinearGradient,
        PaintRadialGradient,
        PointKeyframe,
        Spread,
        Vec2,
    )

__all__ = [
    "EASING_PRESETS",
    "InterpolationName",
    "Point",
    "constant",
    "cubic_bezier",
    "is_animated",
    "key",
    "keyframes",
    "lift",
    "linear_gradient",
    "point",
    "point_key",
    "point_keyframes",
    "radial_gradient",
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

    :func:`alpha2.lint_project` reports keys no frame reaches (``unreached_keyframe``), the usual
    sign of a wrong time base.
    """
    return {
        "time": to_ticks(at),
        "value": value,
        "interpolation": _interpolation(interpolation, easing),
    }


def keyframes(*keys: Keyframe[T]) -> AnimatedKeyframes[T]:
    """The value moving through ``keys``, which must be in strictly ascending time.

    Raises ``ValueError`` for no keys or keys out of order or at one time; they are not sorted,
    because sorting would hide the mistake.
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
    """A :func:`key` of a position that may curve: with tangents, it travels along the cubic Bezier
    through its ``out_tangent`` and the next key's ``in_tangent`` (each relative to its key)."""
    frame: dict[str, Any] = dict(key(at, point(value), interpolation, easing))
    if in_tangent is not None:
        frame["in_tangent"] = point(in_tangent)
    if out_tangent is not None:
        frame["out_tangent"] = point(out_tangent)
    return cast("PointKeyframe", frame)


def point_keyframes(*keys: PointKeyframe) -> AnimatedPointKeyframes:
    """A position moving through ``keys`` (:func:`point_key`), as :func:`keyframes` checks them."""
    return cast("AnimatedPointKeyframes", keyframes(*cast("tuple[Keyframe[Vec2], ...]", keys)))


def cubic_bezier(x1: float, y1: float, x2: float, y2: float) -> Easing:
    """A CSS ``cubic-bezier(x1, y1, x2, y2)`` easing; ``x1`` and ``x2`` lie in ``0..=1``."""
    return {"type": "cubic_bezier", "x1": x1, "y1": y1, "x2": x2, "y2": y2}


EASING_PRESETS: Mapping[str, Easing] = {
    "linear": {"type": "linear"},
    "ease": cubic_bezier(0.25, 0.1, 0.25, 1.0),
    "ease_in": cubic_bezier(0.42, 0.0, 1.0, 1.0),
    "ease_out": cubic_bezier(0.0, 0.0, 0.58, 1.0),
    "ease_in_out": cubic_bezier(0.42, 0.0, 0.58, 1.0),
}
"""The CSS Easing Functions Level 1 named curves."""


def _stops(colors: Sequence[Any]) -> list[GradientStop]:
    if len(colors) < 2:
        raise ValueError("a gradient has at least two stops")
    stops: list[GradientStop] = []
    for index, color in enumerate(colors):
        if isinstance(color, tuple):
            offset, value = color
        else:
            offset, value = index / (len(colors) - 1), color
        stops.append(cast("GradientStop", {"offset": offset, "color": value}))
    return stops


def linear_gradient(
    colors: Sequence[Any],
    start: Point | Vec2 = (0.0, 0.0),
    end: Point | Vec2 = (1.0, 1.0),
    *,
    spread: Spread | None = None,
) -> PaintLinearGradient:
    """A paint along the line from ``start`` to ``end`` (normalized). ``colors`` are colors
    (``"#rrggbb"`` or ``{red, green, blue, alpha}``) spread evenly, or ``(offset, color)``
    pairs."""
    paint: dict[str, Any] = {
        "type": "linear_gradient",
        "start": point(start),
        "end": point(end),
        "stops": _stops(colors),
    }
    if spread is not None:
        paint["spread"] = spread
    return cast("PaintLinearGradient", paint)


def radial_gradient(
    colors: Sequence[Any],
    center: Point | Vec2 = (0.5, 0.5),
    radius: float = 0.5,
    *,
    spread: Spread | None = None,
) -> PaintRadialGradient:
    """A paint along the radius of a circle at ``center``, ``radius`` a fraction of the shorter
    side; ``colors`` as :func:`linear_gradient` takes them."""
    paint: dict[str, Any] = {
        "type": "radial_gradient",
        "center": point(center),
        "radius": radius,
        "stops": _stops(colors),
    }
    if spread is not None:
        paint["spread"] = spread
    return cast("PaintRadialGradient", paint)
