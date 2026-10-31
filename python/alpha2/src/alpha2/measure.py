"""Where text and layout boxes lie, measured with the fonts an export draws with, without a GPU::

    measurer = alpha2.TextMeasurer(["fonts/Inter.ttf"])
    bounds = measurer.layout(card, picture=(1920, 1080))
    bounds["nodes"][1]["baselines"]  # {"first": ..., "last": ...}

Measurement uses the renderer's own layout code, so it matches an export with the same fonts, given
in the same order as to :func:`alpha2.export`.

Rectangles are fractions of the canvas (or picture), ``(0, 0)`` top-left to ``(1, 1)``
bottom-right, as a text style's ``position`` is; baselines are y as a fraction of its height.
"""

from __future__ import annotations

import json
import os
from collections.abc import Mapping, Sequence
from typing import TYPE_CHECKING, Any, TypedDict, cast

from . import _native
from .builder import Time, to_ticks

if TYPE_CHECKING:
    from .types import Generator, LayoutNode

__all__ = ["Baselines", "LayoutBounds", "NodeBounds", "Rect", "TextBounds", "TextMeasurer"]


class Rect(TypedDict):
    """A rectangle as fractions of the canvas, y down."""

    left: float
    top: float
    right: float
    bottom: float


class Baselines(TypedDict):
    """The first and last lines' alphabetic baselines, y as a fraction of the height."""

    first: float
    last: float


class TextBounds(TypedDict):
    """Where a text generator lies on its canvas, before its clip's transform."""

    layout: Rect
    """The box its lines are laid out in: as wide as its widest line, as tall as its lines."""
    content: Rect
    """The box it draws in, its outline and background included."""
    ink: Rect | None
    """The pixels it covers, or ``None`` when it draws nothing."""
    lineCount: int
    baselines: Baselines


class NodeBounds(TypedDict):
    """Where one node of a layout lies: its boxes, before its clip's transform and node visuals."""

    at: str
    """Its path from the root, like ``root.children[1]``."""
    border: Rect
    content: Rect
    overflow: bool
    """Text taller than its content box or splitting a word, or an in-flow child past its padding
    box. Rendering draws it anyway."""
    lineCount: int | None
    """A text node's line count; ``None`` for other nodes."""
    baselines: Baselines | None
    """A text node's baselines: its content box's top plus its text's. ``None`` for other nodes
    and empty text."""


class LayoutBounds(TypedDict):
    """Every node of a layout, the root first, in document order."""

    nodes: list[NodeBounds]


def _size(field: str, value: object) -> tuple[int, int]:
    if (
        not isinstance(value, (tuple, list))
        or len(value) != 2
        or not all(isinstance(side, int) and not isinstance(side, bool) for side in value)
    ):
        raise ValueError(f"{field} must be (width, height) in pixels, not {value!r}")
    return (value[0], value[1])


def _json(document: Mapping[str, Any]) -> str:
    return json.dumps(document, ensure_ascii=False, allow_nan=False, separators=(",", ":"))


class TextMeasurer:
    """Measures text and layout generators with the font files at ``fonts``, in fallback order.

    Fonts are parsed once. Measuring releases the GIL; threads may share one measurer.

    Raises ``FileNotFoundError`` naming a missing font file, and ``ValueError`` naming a file that
    holds no font the engine draws.
    """

    def __init__(self, fonts: Sequence[str | os.PathLike[str]]) -> None:
        if isinstance(fonts, (str, bytes, os.PathLike)):
            raise TypeError("fonts must be a list of font file paths")
        self._native = _native.TextMeasurer([os.fspath(font) for font in fonts])

    def text(self, generator: Generator, *, canvas: tuple[int, int]) -> TextBounds:
        """Where the text generator ``generator`` (``{"type": "text", ...}``) lies on a ``canvas``
        (width, height) pixels canvas. Measure at the project's canvas size, since lines break
        differently at other sizes.

        Raises ``ValueError`` with the engine's message for a generator it refuses.
        """
        width, height = _size("canvas", canvas)
        measured = self._native.text(_json(generator), width, height)
        return cast("TextBounds", json.loads(measured))

    def layout(
        self,
        generator: Generator | LayoutNode,
        *,
        picture: tuple[int, int],
        time: Time = 0,
        canvas: tuple[int, int] | None = None,
    ) -> LayoutBounds:
        """Where every node of ``generator`` (a layout generator, or its root box) lies on a
        ``picture`` (width, height) pixels picture at picture time ``time`` (seconds, or exact
        :class:`~alpha2.Ticks`).

        Measure at the drawn picture size. ``"<n>px"`` lengths are pixels of ``canvas`` (default
        ``picture``), so a half-size picture passes the project's size here. Raises ``ValueError``
        with the engine's message for a layout it refuses.
        """
        width, height = _size("picture", picture)
        sized = None if canvas is None else _size("canvas", canvas)
        document: Mapping[str, Any] = (
            generator
            if generator.get("type") == "layout"
            else {"type": "layout", "root": generator}
        )
        measured = self._native.layout(_json(document), width, height, to_ticks(time), sized)
        return cast("LayoutBounds", json.loads(measured))
