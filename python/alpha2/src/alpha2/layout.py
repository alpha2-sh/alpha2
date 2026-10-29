"""Layout generators: boxes laid out by CSS flexbox or grid around their text and shapes, as a
project's ``{"type": "layout"}`` generator holds them::

    from alpha2.layout import box, layout, text

    card = layout(
        box(
            text("Hello", font_size="40px", line_height=1.4),
            padding="12px 20px",
            background="#1e2230",
            border_radius="16px",
            justify_content="center",
            align_items="center",
        )
    )

The props are the document's field names, as ``alpha2/layout``'s in TypeScript are. A value that is
not animated becomes a constant, a ``(top, right, bottom, left)`` tuple the sides object, an
``(x, y)`` tuple a vector, and one number ``scale`` both axes. Lengths are written as the document
writes them: a number is a fraction of the picture's shorter side, ``"12px"`` pixels of the project
canvas, ``"50%"`` a percent of the parent's content box. Strings are not parsed here: a length or a
CSS value the engine cannot read is reported by :func:`alpha2.validate_project` with the engine's
own message (``invalid length "5 px" (write 5px)``), so there is one grammar, in the engine.

Text style (``font_family``, ``font_size``, ``font_weight``, ``color``, ``line_height``,
``letter_spacing``, ``text_align``, ``outline``) may be set on any box and is inherited. Keyframes
in a layout are keyed in picture time (see :func:`alpha2.key`).
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import TYPE_CHECKING, Any, TypedDict, Unpack, cast

from .values import is_animated, map_values

if TYPE_CHECKING:
    from .types import (
        AlignContent,
        AlignItems,
        Animated,
        Display,
        FlexDirection,
        FlexWrap,
        GeneratorLayout,
        GridAutoFlow,
        JustifyContent,
        LayoutNode,
        Overflow,
        Paint,
        Position,
        Rgba,
        ShapeItem,
        TextAlign,
        TextOutline,
        Vec2,
    )

__all__ = ["BoxProps", "ItemProps", "box", "layout", "shape", "text"]

Size = float | str
"""A fraction of the picture's shorter side, or ``"12px"``."""

Length = float | str
"""A :data:`Size`, or ``"50%"`` of the parent's content box."""

Sides = float | str | tuple[Size, Size, Size, Size] | Mapping[str, Size]
"""One size for every side, the CSS shorthand (``"8px 16px"``), ``(top, right, bottom, left)``,
or the sides object (missing sides 0)."""

Offset = str | tuple[Size, Size] | Mapping[str, Size]
"""The CSS ``translate`` string (``"10px 20px"``), ``(x, y)``, or ``{"x", "y"}``."""


class Border(TypedDict):
    width: Size | Animated[Size]
    paint: Paint | Animated[Paint]


class Inset(TypedDict, total=False):
    left: Length | Animated[Length]
    top: Length | Animated[Length]
    right: Length | Animated[Length]
    bottom: Length | Animated[Length]


class ItemProps(TypedDict, total=False):
    """The fields of any box: its text style, its box and its place in its parent."""

    font_family: str
    font_size: Size | Animated[Size]
    font_weight: int
    color: Rgba
    line_height: float
    letter_spacing: float
    text_align: TextAlign
    outline: TextOutline
    padding: Sides | Animated[Any]
    background: Paint | Animated[Paint]
    border: Border
    border_radius: Size | Animated[Size]
    overflow: Overflow
    width: Length | Animated[Length]
    height: Length | Animated[Length]
    min_width: Length | Animated[Length]
    min_height: Length | Animated[Length]
    max_width: Length | Animated[Length]
    max_height: Length | Animated[Length]
    aspect_ratio: float
    margin: Sides | Animated[Any]
    flex_grow: float | Animated[float]
    flex_shrink: float | Animated[float]
    flex_basis: Length | Animated[Length]
    align_self: AlignItems
    justify_self: AlignItems
    grid_column: int | str
    grid_row: int | str
    position: Position
    inset: Inset
    opacity: float | Animated[float]
    translate: Offset | Animated[Any]
    scale: float | tuple[float, float] | Vec2 | Animated[Any]
    rotation_degrees: float | Animated[float]


class BoxProps(ItemProps, total=False):
    """A container's fields besides :class:`ItemProps`: how it lays its children out."""

    display: Display
    flex_direction: FlexDirection
    flex_wrap: FlexWrap
    grid_template_columns: str
    grid_template_rows: str
    grid_auto_columns: str
    grid_auto_rows: str
    grid_auto_flow: GridAutoFlow
    gap: Size | Animated[Size]
    row_gap: Size | Animated[Size]
    column_gap: Size | Animated[Size]
    justify_content: JustifyContent
    align_items: AlignItems
    align_content: AlignContent
    justify_items: AlignItems


def layout(root: LayoutNode) -> GeneratorLayout:
    """A layout generator whose root is ``root``: the whole picture."""
    return {"type": "layout", "root": root}


def box(*children: LayoutNode, **props: Unpack[BoxProps]) -> LayoutNode:
    """A box: a container holding ``children``, or empty without them (a spacer, a decoration)."""
    fields = _fields(props)
    if children:
        fields["children"] = list(children)
    return cast("LayoutNode", fields)


def text(content: str | Animated[str], **props: Unpack[ItemProps]) -> LayoutNode:
    """A box holding one paragraph of text, wrapped at its content width and drawn in the style it
    sets or inherits; ``\\n`` starts a line.

    ``content`` may be keyframes of strings (:func:`alpha2.keyframes`): animated text, such as a
    count-up in one clip. Text only holds from one value to the next, so a ``linear`` key (the
    default of :func:`alpha2.key`) is written as ``hold``; any other interpolation raises
    ``ValueError``.
    """
    fields = _fields(props)
    fields["text"] = _held(content)
    return cast("LayoutNode", fields)


def shape(items: Sequence[ShapeItem], **props: Unpack[ItemProps]) -> LayoutNode:
    """A box holding shapes (:mod:`alpha2.shapes`), drawn on a picture the size of its content box:
    positions normalized to it, lengths fractions of its shorter side. It has no size of its own:
    give it ``width`` and ``height``, an ``aspect_ratio``, or ``flex_grow``."""
    if isinstance(items, Mapping):
        items = cast("Sequence[ShapeItem]", items.get("items", []))
    fields = _fields(props)
    fields["shape"] = {"items": list(items)}
    return cast("LayoutNode", fields)


def _held(content: Any) -> Any:
    if not is_animated(content) or content["mode"] == "constant":
        return map_values(content, lambda value: value)
    frames = []
    for frame in content["keyframes"]:
        kind = frame.get("interpolation", {}).get("type")
        if kind not in ("hold", "linear"):
            raise ValueError(f"text only holds between keyframes, not a {kind} interpolation")
        frames.append({**frame, "interpolation": {"type": "hold"}})
    return {**content, "keyframes": frames}


def _sides(value: Any) -> Any:
    if isinstance(value, (tuple, list)):
        if len(value) != 4:
            raise ValueError(f"sides are (top, right, bottom, left), not {value!r}")
        top, right, bottom, left = value
        return {"top": top, "right": right, "bottom": bottom, "left": left}
    return value


def _offset(value: Any) -> Any:
    if isinstance(value, (tuple, list)):
        if len(value) != 2:
            raise ValueError(f"an offset is (x, y), not {value!r}")
        x, y = value
        return {"x": x, "y": y}
    return value


def _scale(value: Any) -> Any:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return {"x": value, "y": value}
    return _offset(value)


def _same(value: Any) -> Any:
    return value


_ANIMATED = frozenset(
    {
        "flex_grow",
        "flex_shrink",
        "opacity",
        "rotation_degrees",
        "gap",
        "row_gap",
        "column_gap",
        "border_radius",
        "font_size",
        "background",
        "width",
        "height",
        "min_width",
        "min_height",
        "max_width",
        "max_height",
        "flex_basis",
    }
)


def _fields(props: Mapping[str, Any]) -> dict[str, Any]:
    """``props`` as the document's fields, each shorthand written out; absent fields stay
    absent."""
    fields: dict[str, Any] = {}
    for name, value in props.items():
        if value is None:
            continue
        if name in _ANIMATED:
            fields[name] = map_values(value, _same)
        elif name in ("padding", "margin"):
            fields[name] = map_values(value, _sides)
        elif name == "border":
            fields[name] = {
                "width": map_values(value["width"], _same),
                "paint": map_values(value["paint"], _same),
            }
        elif name == "inset":
            fields[name] = {
                side: map_values(length, _same)
                for side, length in value.items()
                if length is not None
            }
        elif name == "translate":
            fields[name] = map_values(value, _offset)
        elif name == "scale":
            fields[name] = map_values(value, _scale)
        else:
            # CSS keywords, grid strings, the aspect ratio and the plain style values are written as
            # they are; a field the model does not know it refuses by name.
            fields[name] = value
    return fields
