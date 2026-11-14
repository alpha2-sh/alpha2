"""Client-side extractors over the host protocol's ``extract`` operation.

The host decodes and presents the input and hands it over step by step; the extractor's emits go
back in acknowledgements and the host validates them as the session does.
"""

from __future__ import annotations

import json
import math
import operator
import struct
from dataclasses import dataclass
from typing import Any, Literal, Protocol


@dataclass(frozen=True)
class ExtractFrame:
    """A delivered frame: tightly packed ``rgba8`` (straight alpha, sRGB) or ``luma8``."""

    time: int
    """Presentation time in ticks on the input's clock."""
    width: int
    height: int
    format: Literal["rgba8", "luma8"]
    data: bytes


@dataclass(frozen=True)
class ExtractAudio:
    """A delivered audio block: planar f32, channel after channel."""

    start: int
    """The first sample's time in ticks (the tick at or before it)."""
    start_sample: int
    """The first sample, at ``rate`` from the clock's zero."""
    frames: int
    rate: int
    channels: int
    samples: tuple[float, ...]


@dataclass(frozen=True)
class ExtractProgress:
    """Progress of one pass; ``total`` is ``None`` for source frames, whose count is unknown."""

    pass_index: int
    completed: int
    total: int | None


def _text(value: object, what: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{what} must be a string, not {value!r}")
    return value


def _time(value: object, what: str) -> int:
    try:
        if isinstance(value, bool):
            raise TypeError
        return operator.index(value)  # type: ignore[arg-type]
    except TypeError:
        raise ValueError(f"{what} must be an integer tick count, not {value!r}") from None


def _number(value: object, what: str) -> float:
    """A finite number: ``int`` and ``float`` as they are, anything else with ``__float__``
    (``numpy.float32``) converted."""
    finite = False
    number: Any = value
    if not isinstance(value, (bool, str, bytes)) and hasattr(value, "__float__"):
        if type(value) not in (int, float):
            number = float(value)
        try:
            finite = math.isfinite(number)
        except OverflowError:
            finite = False
    if not finite:
        raise ValueError(f"{what} must be a finite number, not {value!r}")
    return number  # type: ignore[no-any-return]


class Emitter:
    """Where an extractor's values go during one step. An emit the wire cannot carry raises
    ``ValueError`` naming it, which fails the step."""

    def __init__(self) -> None:
        self.emits: list[dict[str, Any]] = []

    def sample(self, output: str, time: int, values: list[float | None]) -> None:
        """A ``points``, ``scalars`` or ``similarity`` sample: an integer tick ``time`` and finite
        values, ``None`` marking a lost channel."""
        what = f"sample {output!r}"
        emit: dict[str, Any] = {
            "output": _text(output, "an emit's output"),
            "time": _time(time, f"{what}: time"),
            "values": [
                None if value is None else _number(value, f"{what}: values[{index}]")
                for index, value in enumerate(values)
            ],
        }
        self.emits.append(emit)

    def word(
        self, output: str, start: int, end: int, text: str, speaker: str | None = None
    ) -> None:
        """A word of a ``words`` output, from ``start`` to ``end`` in ticks."""
        what = f"word {output!r}"
        emit: dict[str, Any] = {
            "output": _text(output, "an emit's output"),
            "start": _time(start, f"{what}: start"),
            "end": _time(end, f"{what}: end"),
            "text": _text(text, f"{what}: text"),
        }
        if speaker is not None:
            emit["speaker"] = _text(speaker, f"{what}: speaker")
        self.emits.append(emit)

    def interval(
        self,
        output: str,
        start: int,
        end: int,
        label: str | None = None,
        score: float | None = None,
    ) -> None:
        """An interval of an ``intervals`` output, with an optional label and finite score."""
        what = f"interval {output!r}"
        emit: dict[str, Any] = {
            "output": _text(output, "an emit's output"),
            "start": _time(start, f"{what}: start"),
            "end": _time(end, f"{what}: end"),
        }
        if label is not None:
            emit["label"] = _text(label, f"{what}: label")
        if score is not None:
            emit["score"] = _number(score, f"{what}: score")
        self.emits.append(emit)


class Extractor(Protocol):
    """A client-side extractor. Only ``describe`` is required; a missing step does nothing.

    The optional steps are ``begin(context)``, ``pass_(index, direction)``,
    ``video(frame, out)``, ``audio(block, out)`` and ``end(out)``. Whole-range work (speech
    recognition) belongs in ``end``, which may emit anywhere in the range.
    """

    def describe(self) -> dict[str, Any]:
        """The descriptor: id, version, input, ``params_schema`` and outputs."""
        ...


def take_step(
    extractor: Any, message: dict[str, Any], attachments: list[dict[str, Any]]
) -> dict[str, Any]:
    """Takes one step with ``extractor``; returns its acknowledgement's payload, never raising."""
    out = Emitter()
    step = message["step"]
    data: bytes = attachments[0]["data"] if attachments else b""
    try:
        if step == "begin" and hasattr(extractor, "begin"):
            extractor.begin(message.get("context", {}))
        elif step == "pass" and hasattr(extractor, "pass_"):
            extractor.pass_(int(message["pass"]), message.get("direction", "forward"))
        elif step == "video" and hasattr(extractor, "video"):
            extractor.video(
                ExtractFrame(
                    int(message["time"]),
                    int(message["width"]),
                    int(message["height"]),
                    "luma8" if message.get("format") == "luma8" else "rgba8",
                    data,
                ),
                out,
            )
        elif step == "audio" and hasattr(extractor, "audio"):
            extractor.audio(
                ExtractAudio(
                    int(message["start"]),
                    int(message["startSample"]),
                    int(message["frames"]),
                    int(message["rate"]),
                    int(message["channels"]),
                    struct.unpack(f"<{len(data) // 4}f", data),
                ),
                out,
            )
        elif step == "end" and hasattr(extractor, "end"):
            extractor.end(out)
    except Exception as cause:  # noqa: BLE001 - reported to the host as the extractor's failure
        return {"error": {"message": str(cause) or type(cause).__name__}}
    return {"emits": out.emits}


def schema_problem(schema: object, value: object, at: str = "params") -> str | None:
    """Where ``value`` breaks ``schema``, or ``None``. Mirrors ``schemaProblem`` of ``alpha2``."""
    if not isinstance(schema, dict):
        return None

    def kind(item: object) -> str:
        if item is None:
            return "null"
        if isinstance(item, bool):
            return "boolean"
        if isinstance(item, int):
            return "integer"
        if isinstance(item, float):
            return "integer" if item.is_integer() else "number"
        if isinstance(item, str):
            return "string"
        if isinstance(item, list):
            return "array"
        return "object" if isinstance(item, dict) else "unknown"

    if "type" in schema:
        types = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        actual = kind(value)
        if not any(t == actual or (t == "number" and actual == "integer") for t in types):
            return f"{at} must be {' or '.join(map(str, types))}"

    def same(a: object, b: object) -> bool:
        return json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)

    if isinstance(schema.get("enum"), list) and not any(same(v, value) for v in schema["enum"]):
        return f"{at} must be one of {json.dumps(schema['enum'])}"
    if "const" in schema and not same(schema["const"], value):
        return f"{at} must be {json.dumps(schema['const'])}"
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if isinstance(schema.get("minimum"), (int, float)) and value < schema["minimum"]:
            return f"{at} must be at least {schema['minimum']}"
        if isinstance(schema.get("maximum"), (int, float)) and value > schema["maximum"]:
            return f"{at} must be at most {schema['maximum']}"
    if isinstance(value, str):
        if isinstance(schema.get("minLength"), int) and len(value) < schema["minLength"]:
            return f"{at} is too short"
        if isinstance(schema.get("maxLength"), int) and len(value) > schema["maxLength"]:
            return f"{at} is too long"
    if isinstance(value, list):
        if isinstance(schema.get("minItems"), int) and len(value) < schema["minItems"]:
            return f"{at} has too few items"
        if isinstance(schema.get("maxItems"), int) and len(value) > schema["maxItems"]:
            return f"{at} has too many items"
        for index, item in enumerate(value):
            problem = schema_problem(schema.get("items"), item, f"{at}[{index}]")
            if problem is not None:
                return problem
    if isinstance(value, dict):
        properties = schema.get("properties") or {}
        for name in schema.get("required") or []:
            if name not in value:
                return f"{at}.{name} is required"
        for name, item in value.items():
            if name in properties:
                problem = schema_problem(properties[name], item, f"{at}.{name}")
            elif schema.get("additionalProperties") is False:
                return f"{at}.{name} is not allowed"
            else:
                problem = schema_problem(schema.get("additionalProperties"), item, f"{at}.{name}")
            if problem is not None:
                return problem
    return None
