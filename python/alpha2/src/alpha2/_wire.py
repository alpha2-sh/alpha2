"""Framing of host protocol messages on the host's standard streams: one JSON object per line."""

from __future__ import annotations

import json
import os
from collections.abc import Iterator
from typing import IO, Any, Literal

from . import _native
from .protocol import parse_host_message

# Matches the host's MAX_REQUEST_LINE_BYTES, newline excluded.
MAX_REQUEST_BYTES = 2 * _native.MAX_PROJECT_JSON_BYTES

Received = (
    tuple[Literal["message"], dict[str, Any]]
    | tuple[Literal["diagnostic"], bytes]
    | tuple[Literal["invalid"], str]
)


def _json_default(value: object) -> object:
    if isinstance(value, os.PathLike):
        return os.fspath(value)
    raise TypeError(f"Object of type {type(value).__name__} is not JSON serializable")


def encode(message: dict[str, Any]) -> bytes:
    """The message's body, counted against :data:`MAX_REQUEST_BYTES`.

    Raises ``TypeError`` or ``ValueError`` for a message that cannot be written as JSON.
    """
    return json.dumps(
        message,
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
        default=_json_default,
    ).encode("utf-8")


def frame(body: bytes) -> bytes:
    """What is written to the host for one message body."""
    return body + b"\n"


def write(stream: IO[bytes], framed: bytes) -> None:
    stream.write(framed)
    stream.flush()


def read(stream: IO[bytes]) -> Iterator[Received]:
    """What the host writes, until its output ends.

    A line that is not JSON comes from a loaded library (a Vulkan validation layer, for example)
    and is a ``diagnostic``; JSON that is not a valid message is ``invalid``.
    """
    for raw in stream:
        text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        if not text.startswith("{"):
            yield "diagnostic", raw if raw.endswith(b"\n") else raw + b"\n"
            continue
        try:
            yield "message", parse_host_message(text)
        except ValueError:
            yield "invalid", text
