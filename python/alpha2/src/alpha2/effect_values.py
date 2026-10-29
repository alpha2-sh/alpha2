"""Effect parameter values, imported as ``fx``::

    from alpha2 import effect, fx

    blur = effect("alpha2.effect.gaussian-blur", radius=0.02)
    tint = effect("alpha2.effect.color-overlay", color=fx.color("#ff8800"), mode=fx.enum(1))

An effect parameter is typed in the document (``{"type": "float", "value": 0.2}``), because one
number could be a scalar or an enum index and four could be a vector or a color: the effect's
definition is not at hand to tell. :func:`param` lifts the plain values that are not ambiguous (a
``float``, a ``bool``, an ``(x, y)`` pair, a hex color string) and refuses the ones that are,
naming the constructors to choose from.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, cast

from .values import map_values

if TYPE_CHECKING:
    from .types import (
        Animated,
        EffectValue,
        EffectValueBool,
        EffectValueColor,
        EffectValueEnum,
        EffectValueFloat,
        EffectValueVec2,
        EffectValueVec4,
    )

__all__ = ["color", "enum", "flag", "param", "scalar", "vec2", "vec4"]

_TYPES = frozenset({"float", "vec2", "vec4", "color", "enum", "bool"})


def _number(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TypeError(f"{value!r} is not a number")
    return float(value)


def scalar(value: float) -> EffectValueFloat:
    """A number: a radius, an amount, an angle."""
    return {"type": "float", "value": _number(value)}


def enum(index: int) -> EffectValueEnum:
    """An index into the effect's choices for the parameter, from 0."""
    if isinstance(index, bool) or not isinstance(index, int):
        raise TypeError(f"an enum index is an integer, not {index!r}")
    return {"type": "enum", "value": index}


def flag(value: bool) -> EffectValueBool:
    """A toggle."""
    if not isinstance(value, bool):
        raise TypeError(f"a flag is True or False, not {value!r}")
    return {"type": "bool", "value": value}


def vec2(x: float, y: float) -> EffectValueVec2:
    """Two components, usually a point in normalized coordinates."""
    return {"type": "vec2", "value": [_number(x), _number(y)]}


def vec4(a: float, b: float, c: float, d: float) -> EffectValueVec4:
    """Four components that are not a color, such as per-channel thresholds."""
    return {"type": "vec4", "value": [_number(a), _number(b), _number(c), _number(d)]}


def color(
    value: str | tuple[float, float, float] | tuple[float, float, float, float],
) -> EffectValueColor:
    """A color: CSS hex (``"#rgb"``, ``"#rgba"``, ``"#rrggbb"``, ``"#rrggbbaa"``) or straight
    sRGB components ``(red, green, blue)`` or ``(red, green, blue, alpha)`` in ``0..=1``."""
    if isinstance(value, str):
        digits = value[1:] if value.startswith("#") else ""
        if len(digits) in (3, 4):
            digits = "".join(digit * 2 for digit in digits)
        if len(digits) not in (6, 8) or any(
            digit not in "0123456789abcdefABCDEF" for digit in digits
        ):
            raise ValueError(f"{value!r} is not a hex color such as #ff8800")
        if len(digits) == 6:
            digits += "ff"
        components = [int(digits[i : i + 2], 16) / 255 for i in range(0, 8, 2)]
        return {"type": "color", "value": components}
    if isinstance(value, (tuple, list)) and len(value) in (3, 4):
        components = [_number(component) for component in value]
        return {
            "type": "color",
            "value": components if len(components) == 4 else [*components, 1.0],
        }
    raise TypeError(f"{value!r} is not a color: write a hex string or (red, green, blue[, alpha])")


def _value(value: object) -> EffectValue:
    if isinstance(value, Mapping) and value.get("type") in _TYPES and "value" in value:
        return cast("EffectValue", value)
    # bool before int: a bool is an int.
    if isinstance(value, bool):
        return flag(value)
    if isinstance(value, int):
        raise TypeError(
            f"{value} is ambiguous: write fx.scalar({float(value)!r}) or fx.enum({value})"
        )
    if isinstance(value, float):
        return scalar(value)
    if isinstance(value, str):
        return color(value)
    if isinstance(value, (tuple, list)):
        if len(value) == 2:
            return vec2(value[0], value[1])
        if len(value) == 4:
            raise TypeError(f"{tuple(value)!r} is ambiguous: write fx.vec4(...) or fx.color(...)")
    raise TypeError(
        f"{value!r} is not an effect value: write fx.scalar, fx.enum, fx.flag, fx.vec2, fx.vec4 "
        "or fx.color"
    )


def param(value: Any) -> Animated[EffectValue]:
    """``value`` as an effect parameter: an ``fx`` value, or an ``Animated`` whose values are,
    stays as it is; a plain value is lifted where it is not ambiguous.

    ======================= ===================
    Plain value             Becomes
    ======================= ===================
    ``float``               :func:`scalar`
    ``bool``                :func:`flag`
    ``(x, y)``              :func:`vec2`
    hex ``str``             :func:`color`
    ======================= ===================

    An ``int`` (a scalar or an enum index?) and a 4-tuple (a vector or a color?) raise
    ``TypeError`` naming both constructors, as does any other value. The values of keyframes
    (:func:`alpha2.keyframes`) are lifted the same way, each on its own.
    """
    return cast("Animated[EffectValue]", map_values(value, _value))
