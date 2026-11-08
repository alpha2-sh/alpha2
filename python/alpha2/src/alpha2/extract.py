"""Client-side extractors over the host protocol's ``extract`` operation.

The host decodes and presents the input and hands it over step by step; the extractor's emits go
back in acknowledgements and the host validates them as the session does.
"""

from __future__ import annotations

import json
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
    """Progress of one pass; ``total`` is ``None`` for source frames, whose count is not known
    ahead."""

    pass_index: int
    completed: int
    total: int | None


class Emitter:
    """Where an extractor's values go during one step."""

    def __init__(self) -> None:
        self.emits: list[dict[str, Any]] = []

    def sample(self, output: str, time: int, values: list[float | None]) -> None:
        """A sample of a ``points``, ``scalars`` or ``similarity`` output; ``None`` marks a lost
        channel."""
        self.emits.append({"output": output, "time": time, "values": list(values)})

    def word(
        self, output: str, start: int, end: int, text: str, speaker: str | None = None
    ) -> None:
        emit: dict[str, Any] = {"output": output, "start": start, "end": end, "text": text}
        if speaker is not None:
            emit["speaker"] = speaker
        self.emits.append(emit)

    def interval(
        self,
        output: str,
        start: int,
        end: int,
        label: str | None = None,
        score: float | None = None,
    ) -> None:
        emit: dict[str, Any] = {"output": output, "start": start, "end": end}
        if label is not None:
            emit["label"] = label
        if score is not None:
            emit["score"] = score
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
    """Takes one step with ``extractor`` and returns its acknowledgement's payload; never
    raises."""
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
    """Where ``value`` breaks ``schema``, or ``None``. Covers the JSON Schema keywords
    descriptors use: ``type``, ``enum``, ``const``, ``properties``, ``required``,
    ``additionalProperties``, ``items``, ``minItems``, ``maxItems``, ``minimum``, ``maximum``,
    ``minLength`` and ``maxLength``."""
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
