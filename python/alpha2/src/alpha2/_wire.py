"""Framing of host protocol messages on the host's standard streams.

A frame is the header's length and the attachments' length (each a little-endian u32), then the
header (one JSON object) and the attachments. The host writes :data:`MAGIC` before its first frame.
"""

from __future__ import annotations

import json
import os
import struct
from collections.abc import Iterator
from typing import IO, Any, Literal

from . import _native
from .protocol import parse_host_message

MAGIC = b"\x89A2HOST\n"

# Matches the host's limit on a request header.
MAX_REQUEST_BYTES = 2 * _native.MAX_PROJECT_JSON_BYTES

# A host header past this is not one the host would write, so the stream is not trusted further.
_MAX_HEADER_BYTES = 2 * MAX_REQUEST_BYTES

# Bytes a library may print before the magic; past this the stream is not the host's.
MAX_PREAMBLE_BYTES = 1 << 20

_LENGTHS = struct.Struct("<II")

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
    """The message's header, counted against :data:`MAX_REQUEST_BYTES`.

    Raises ``TypeError`` or ``ValueError`` for a message that cannot be written as JSON.
    """
    return json.dumps(
        message,
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
        default=_json_default,
    ).encode("utf-8")


def frame(header: bytes) -> bytes:
    """The frame of a message without attachments."""
    return _LENGTHS.pack(len(header), 0) + header


def write(stream: IO[bytes], framed: bytes) -> None:
    stream.write(framed)
    stream.flush()


def _exactly(stream: IO[bytes], count: int) -> bytes | None:
    """``count`` bytes, or ``None`` when the stream ends first."""
    data = stream.read(count)
    return data if len(data) == count else None


def _skip(stream: IO[bytes], count: int) -> bool:
    while count > 0:
        chunk = stream.read(min(count, 1 << 20))
        if not chunk:
            return False
        count -= len(chunk)
    return True


def read(stream: IO[bytes]) -> Iterator[Received]:
    """What the host writes, until its output ends.

    Bytes before :data:`MAGIC` are a ``diagnostic``. A frame whose header is not a valid message,
    or whose attachments differ from what its header lists, is ``invalid`` and skipped whole. Each
    entry of a message's ``attachments`` gains ``data``, its bytes.
    """
    seen = bytearray()
    while not seen.endswith(MAGIC):
        if len(seen) >= MAX_PREAMBLE_BYTES + len(MAGIC):
            yield "invalid", f"alpha2-host wrote {len(seen)} bytes without its protocol magic"
            return
        byte = stream.read(1)
        if not byte:
            if seen:
                yield "diagnostic", _line(bytes(seen))
            return
        seen += byte
    if len(seen) > len(MAGIC):
        yield "diagnostic", _line(bytes(seen[: -len(MAGIC)]))
    while (lengths := _exactly(stream, _LENGTHS.size)) is not None:
        header_bytes, attachment_bytes = _LENGTHS.unpack(lengths)
        if header_bytes > _MAX_HEADER_BYTES:
            yield "invalid", f"alpha2-host wrote a {header_bytes}-byte header"
            return
        header = _exactly(stream, header_bytes)
        if header is None:
            return
        try:
            message = parse_host_message(header.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as error:
            yield "invalid", str(error)
            if not _skip(stream, attachment_bytes):
                return
            continue
        listed = message.get("attachments", [])
        if sum(entry["bytes"] for entry in listed) != attachment_bytes:
            yield "invalid", "the frame's attachments are not the ones its header lists"
            if not _skip(stream, attachment_bytes):
                return
            continue
        for entry in listed:
            data = _exactly(stream, entry["bytes"])
            if data is None:
                return
            entry["data"] = data
        yield "message", message


def _line(data: bytes) -> bytes:
    return data if data.endswith(b"\n") else data + b"\n"
