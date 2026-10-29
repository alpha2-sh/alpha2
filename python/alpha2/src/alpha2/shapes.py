"""Shapes generators: vector shapes drawn from a content tree, as an After Effects or Lottie shape
layer draws them::

    from alpha2.shapes import circle, fill, rect, shapes, stroke

    badge = shapes(
        rect((0.5, 0.5), (0.4, 0.2), corner_radius=0.02),
        fill("#1e2230"),
        stroke(0.004, "#ffb020", dash=[0.02, 0.01]),
    )
    project.generated(badge)

Geometry accumulates in its group; a :func:`fill` or :func:`stroke` paints the geometry listed
before it in the same group, and a :func:`trim` cuts it. A paint listed earlier is drawn on top of
one listed later. Every value may be animated (:func:`alpha2.keyframes`, keyed in picture time);
plain values are lifted to constants, and ``(x, y)`` tuples written ``{"x", "y"}``.

Units, on the picture the shapes draw on (the canvas, or a layout box's content box):

================================================== =========================================
Field                                              Unit
================================================== =========================================
``center``, a transform's ``position``, vertices   per axis: x a fraction of the width, y of
                                                   the height
``size`` of a rectangle or an ellipse              per axis, as positions
``corner_radius``, polystar radii, stroke ``width``a fraction of the shorter side
and ``dash`` lengths
================================================== =========================================

So ``ellipse(center, (0.5, 0.5))`` is a circle only on a square picture: :func:`circle` takes the
picture's size and gives the per-axis size of a round one.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import TYPE_CHECKING, Any, cast

from .values import Point, is_animated, map_values, point

if TYPE_CHECKING:
    from .types import (
        Animated,
        AnimatedPoint,
        FillRule,
        GeneratorShapes,
        LineCap,
        LineJoin,
        Paint,
        PathVertex,
        PolystarKind,
        ShapeItem,
        ShapeItemEllipse,
        ShapeItemFill,
        ShapeItemGroup,
        ShapeItemPath,
        ShapeItemPolystar,
        ShapeItemRectangle,
        ShapeItemStroke,
        ShapeItemTrim,
        ShapeTransform,
        Subpath,
        TrimMode,
        Vec2,
    )
    from .types import (
        Path as PathValue,
    )

    Number = float | Animated[float]
    """A number, or an ``Animated`` of one."""

    PointValue = Point | Vec2 | Animated[Vec2] | Animated[Point]
    """``(x, y)``, ``{"x", "y"}``, or an ``Animated`` of either."""

__all__ = [
    "circle",
    "ellipse",
    "fill",
    "group",
    "path",
    "polystar",
    "rect",
    "shapes",
    "stroke",
    "subpath",
    "transform",
    "trim",
    "vertex",
]


def _same(value: Any) -> Any:
    return value


def _lifted(value: Any) -> Any:
    return map_values(value, _same)


def _points(value: Any) -> Any:
    return map_values(value, point)


def _set(item: dict[str, Any], name: str, value: Any) -> None:
    if value is not None:
        item[name] = _lifted(value)


def shapes(*items: ShapeItem) -> GeneratorShapes:
    """A shapes generator drawing ``items``, front first."""
    return {"type": "shapes", "items": list(items)}


def rect(
    center: PointValue,
    size: PointValue,
    corner_radius: Number | None = None,
    *,
    rotation: Number | None = None,
) -> ShapeItemRectangle:
    """A rectangle of full ``size`` (per axis) about ``center``, its corners rounded by
    ``corner_radius`` (a fraction of the shorter side), turned ``rotation`` degrees clockwise."""
    item: dict[str, Any] = {"type": "rectangle", "center": _points(center), "size": _points(size)}
    _set(item, "corner_radius", corner_radius)
    _set(item, "rotation_degrees", rotation)
    return cast("ShapeItemRectangle", item)


def ellipse(
    center: PointValue, size: PointValue, *, rotation: Number | None = None
) -> ShapeItemEllipse:
    """The ellipse inscribed in a full ``size`` (per axis) about ``center``."""
    item: dict[str, Any] = {"type": "ellipse", "center": _points(center), "size": _points(size)}
    _set(item, "rotation_degrees", rotation)
    return cast("ShapeItemEllipse", item)


def circle(center: PointValue, diameter: Number, *, picture: tuple[int, int]) -> ShapeItemEllipse:
    """A circle of ``diameter`` (a fraction of the shorter side) about ``center``, on a picture of
    ``picture`` (width, height) pixels: an :func:`ellipse` of size
    ``(diameter * s / width, diameter * s / height)`` with ``s = min(width, height)``. An animated
    diameter converts each keyframe's value the same way.

    ``picture`` has no default, because the shapes do not know what they draw on: a shapes
    generator draws on the canvas, but a shape inside a layout box draws on that box's content
    box. Pass the size of whichever it is.
    """
    if (
        not isinstance(picture, (tuple, list))
        or len(picture) != 2
        or not all(
            isinstance(side, (int, float)) and not isinstance(side, bool) and side > 0
            for side in picture
        )
    ):
        raise ValueError(f"picture must be (width, height), positive, not {picture!r}")
    width, height = picture
    shorter = min(width, height)

    def size(value: Any) -> Vec2:
        return {"x": value * shorter / width, "y": value * shorter / height}

    return ellipse(center, map_values(diameter, size))


def polystar(
    center: PointValue,
    points: Number,
    outer_radius: Number,
    *,
    kind: PolystarKind = "polygon",
    inner_radius: Number | None = None,
    outer_roundness: Number | None = None,
    inner_roundness: Number | None = None,
    rotation: Number | None = None,
) -> ShapeItemPolystar:
    """A regular polygon (or, with ``kind="star"`` and an ``inner_radius``, a star) of ``points``
    corners at ``outer_radius`` (a fraction of the shorter side) from ``center``, its first point
    straight up before ``rotation``."""
    item: dict[str, Any] = {
        "type": "polystar",
        "kind": kind,
        "center": _points(center),
        "points": _lifted(points),
        "outer_radius": _lifted(outer_radius),
    }
    _set(item, "inner_radius", inner_radius)
    _set(item, "outer_roundness", outer_roundness)
    _set(item, "inner_roundness", inner_roundness)
    _set(item, "rotation_degrees", rotation)
    return cast("ShapeItemPolystar", item)


def vertex(
    at: Point | Vec2,
    *,
    in_tangent: Point | Vec2 | None = None,
    out_tangent: Point | Vec2 | None = None,
) -> PathVertex:
    """A path vertex at ``at``, with the control points of the segments ending and starting here,
    each relative to ``at`` (none: straight segments)."""
    result: dict[str, Any] = {"point": point(at)}
    if in_tangent is not None:
        result["in_tangent"] = point(in_tangent)
    if out_tangent is not None:
        result["out_tangent"] = point(out_tangent)
    return cast("PathVertex", result)


def subpath(*vertices: Point | Vec2 | PathVertex, closed: bool = False) -> Subpath:
    """A run of ``vertices`` (points, or :func:`vertex` for curves), back to the first one when
    ``closed``."""
    written = [
        cast("PathVertex", each) if isinstance(each, Mapping) and "point" in each else vertex(each)
        for each in vertices
    ]
    result: dict[str, Any] = {"vertices": written}
    if closed:
        result["closed"] = True
    return cast("Subpath", result)


def path(*subpaths: Subpath | Animated[PathValue]) -> ShapeItemPath:
    """A Bezier path of ``subpaths`` (:func:`subpath`), or one keyframed path: an ``Animated`` whose
    values are ``{"subpaths": [...]}``."""
    if len(subpaths) == 1 and is_animated(subpaths[0]):
        return {"type": "path", "path": cast("Animated[PathValue]", subpaths[0])}
    return cast(
        "ShapeItemPath",
        {"type": "path", "path": {"mode": "constant", "value": {"subpaths": list(subpaths)}}},
    )


def transform(
    *,
    position: PointValue | AnimatedPoint | None = None,
    anchor: PointValue | None = None,
    scale: float | PointValue | Animated[float] | None = None,
    rotation: Number | None = None,
    opacity: Number | None = None,
) -> ShapeTransform:
    """A group's placement: its ``anchor`` point placed at ``position``, after a ``scale`` (one
    number for both axes, or ``(x, y)``) and a clockwise ``rotation`` in degrees about it, and the
    group's ``opacity`` as a whole. Fields left out leave the items as they are."""

    def scaled(value: Any) -> Vec2:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return {"x": value, "y": value}
        return point(value)

    result: dict[str, Any] = {}
    if position is not None:
        result["position"] = _points(position)
    if anchor is not None:
        result["anchor"] = _points(anchor)
    if scale is not None:
        result["scale"] = map_values(scale, scaled)
    _set(result, "rotation_degrees", rotation)
    _set(result, "opacity", opacity)
    return cast("ShapeTransform", result)


def group(*items: ShapeItem, transform: ShapeTransform | None = None) -> ShapeItemGroup:
    """``items`` drawn together, through one :func:`transform`."""
    result: dict[str, Any] = {"type": "group", "items": list(items)}
    if transform is not None:
        result["transform"] = transform
    return cast("ShapeItemGroup", result)


def fill(
    paint: Paint | Animated[Paint],
    opacity: Number | None = None,
    *,
    rule: FillRule | None = None,
) -> ShapeItemFill:
    """Paints the inside of the geometry before it in its group with ``paint`` (a color, hex or
    object, or a gradient), at ``opacity``."""
    result: dict[str, Any] = {"type": "fill", "paint": _lifted(paint)}
    _set(result, "opacity", opacity)
    if rule is not None:
        result["rule"] = rule
    return cast("ShapeItemFill", result)


def stroke(
    width: Number,
    paint: Paint | Animated[Paint],
    *,
    dash: Sequence[Number] | None = None,
    dash_offset: Number | None = None,
    join: LineJoin | None = None,
    cap: LineCap | None = None,
    miter_limit: float | None = None,
    opacity: Number | None = None,
) -> ShapeItemStroke:
    """Strokes the geometry before it in its group, ``width`` wide (a fraction of the shorter
    side), with ``paint``.

    ``dash`` is the alternating dash and gap lengths (fractions of the shorter side), each its own
    value that may be animated, as Lottie keys them; ``dash_offset`` is how far into the pattern
    the stroke starts.
    """
    result: dict[str, Any] = {"type": "stroke", "width": _lifted(width), "paint": _lifted(paint)}
    if dash is not None:
        if isinstance(dash, Mapping):
            raise ValueError("dash is a list of lengths, such as [0.02, 0.01]")
        pattern: dict[str, Any] = {"pattern": [_lifted(length) for length in dash]}
        _set(pattern, "offset", dash_offset)
        result["dash"] = pattern
    elif dash_offset is not None:
        raise ValueError("dash_offset needs a dash")
    if join is not None:
        result["join"] = join
    if cap is not None:
        result["cap"] = cap
    if miter_limit is not None:
        result["miter_limit"] = miter_limit
    _set(result, "opacity", opacity)
    return cast("ShapeItemStroke", result)


def trim(
    start: Number,
    end: Number,
    offset: Number | None = None,
    *,
    mode: TrimMode | None = None,
) -> ShapeItemTrim:
    """Keeps the part of the geometry before it in its group from ``start`` to ``end`` (fractions
    of its length), shifted by ``offset`` turns; ``mode`` trims all of it together (the default) or
    each part on its own."""
    result: dict[str, Any] = {"type": "trim", "start": _lifted(start), "end": _lifted(end)}
    _set(result, "offset", offset)
    if mode is not None:
        result["mode"] = mode
    return cast("ShapeItemTrim", result)
