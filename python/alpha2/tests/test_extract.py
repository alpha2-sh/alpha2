"""The emitter a client-side extractor writes to."""

from __future__ import annotations

import json
import math
from typing import Any

import pytest

from alpha2.extract import Emitter


class FloatLike:
    """Stands in for ``numpy.float32``, which converts through ``__float__``."""

    def __float__(self) -> float:
        return 0.5


class IndexLike:
    """Stands in for ``numpy.int64``, which converts through ``__index__``."""

    def __index__(self) -> int:
        return 8_000


def test_emits_the_wire_can_carry_are_kept_as_plain_json() -> None:
    out = Emitter()
    out.sample("level", IndexLike(), [FloatLike(), None, 2])
    out.word("words", 0, IndexLike(), "hello", "a")
    out.interval("scenes", 0, 8_000, "cut", FloatLike())
    assert json.loads(json.dumps(out.emits, allow_nan=False)) == [
        {"output": "level", "time": 8_000, "values": [0.5, None, 2]},
        {"output": "words", "start": 0, "end": 8_000, "text": "hello", "speaker": "a"},
        {"output": "scenes", "start": 0, "end": 8_000, "label": "cut", "score": 0.5},
    ]


@pytest.mark.parametrize(
    ("emit", "message"),
    [
        (lambda out: out.sample("level", 0, [math.nan]), r"values\[0\] must be a finite number"),
        (lambda out: out.sample("level", 0, [1, -math.inf]), r"values\[1\] must be"),
        (lambda out: out.sample("level", 0, [10**400]), r"values\[0\] must be a finite number"),
        (lambda out: out.sample("level", 0, [True]), r"values\[0\] must be a finite number"),
        (lambda out: out.sample("level", 0, ["0.5"]), r"values\[0\] must be a finite number"),
        (lambda out: out.sample("level", 1.5, [1]), "time must be an integer tick count"),
        (lambda out: out.sample("level", True, [1]), "time must be an integer tick count"),
        (lambda out: out.sample(7, 0, [1]), "an emit's output must be a string"),
        (lambda out: out.word("words", 0, 4_000, 5), "text must be a string"),
        (lambda out: out.interval("scenes", 0, 1, "cut", math.nan), "score must be a finite"),
    ],
)
def test_an_emit_the_wire_cannot_carry_raises_naming_it(emit: Any, message: str) -> None:
    out = Emitter()
    with pytest.raises(ValueError, match=message):
        emit(out)
    assert out.emits == []
