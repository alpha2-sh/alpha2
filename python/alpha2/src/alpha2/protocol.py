"""The line-delimited JSON protocol spoken with ``alpha2-host serve`` over stdio."""

from __future__ import annotations

import json
import re
from typing import Any

__all__ = [
    "PROTOCOL_VERSION",
    "UNREAD_REQUEST_ID",
    "create_request",
    "describe_exit",
    "loader_refused",
    "ntstatus_name",
    "parse_host_message",
]

PROTOCOL_VERSION = 1

# The requestId the host answers a line with when it could not read the line's own.
UNREAD_REQUEST_ID = "unknown"


def create_request(
    operation: str, request_id: str, payload: dict[str, Any] | None = None
) -> dict[str, Any]:
    """A request: ``payload``'s fields beside the protocol version, operation and id."""
    return {
        "protocolVersion": PROTOCOL_VERSION,
        "operation": operation,
        "requestId": request_id,
        **(payload or {}),
    }


def _safe_count(value: object) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 <= value <= 9_007_199_254_740_991
    )


def _protocol_error(value: object) -> bool:
    return (
        isinstance(value, dict)
        and isinstance(value.get("code"), str)
        and len(value["code"]) > 0
        and isinstance(value.get("message"), str)
        and ("failure" not in value or isinstance(value["failure"], dict))
    )


def parse_host_message(line: str) -> dict[str, Any]:
    """Parses and checks one line the host wrote.

    The message is a progress report (``type: "progress"``), a failure of the host itself
    (``type: "fatal"``), or the answer to a request (``ok``). Raises ``ValueError`` when the line is
    not JSON, speaks another protocol version, or is not a well-formed message.
    """
    value = json.loads(line)
    if not isinstance(value, dict):
        raise ValueError("host message must be an object")
    version = value.get("protocolVersion")
    if version != PROTOCOL_VERSION or isinstance(version, bool):
        raise ValueError(f"unsupported host protocol {version}; expected {PROTOCOL_VERSION}")
    kind = value.get("type")
    if kind == "progress":
        if (
            not isinstance(value.get("requestId"), str)
            or not _safe_count(value.get("completed"))
            or not _safe_count(value.get("total"))
            or value["completed"] > value["total"]
            or ("phase" in value and value["phase"] not in ("frames", "finalizing"))
        ):
            raise ValueError("invalid progress message")
        return value
    if kind == "fatal":
        if not _protocol_error(value.get("error")):
            raise ValueError("invalid fatal message")
        return value
    if not isinstance(value.get("requestId"), str) or not isinstance(value.get("ok"), bool):
        raise ValueError("invalid response message")
    if not value["ok"] and not _protocol_error(value.get("error")):
        raise ValueError("invalid error response")
    if value["ok"] and "result" not in value:
        raise ValueError("successful response is missing result")
    return value


# Windows ends a process whose DLLs it cannot load with one of these statuses, before `main` and
# without a word on stderr: STATUS_DLL_NOT_FOUND, STATUS_ORDINAL_NOT_FOUND,
# STATUS_ENTRYPOINT_NOT_FOUND and STATUS_INVALID_IMAGE_FORMAT.
_WINDOWS_LOADER_STATUSES = frozenset({0xC0000135, 0xC0000138, 0xC0000139, 0xC000007B})

# The statuses Windows ends a crashed process with, by name, so a crash reads as one rather than as
# a ten-digit exit code.
_WINDOWS_CRASH_STATUSES = {
    0xC0000005: "STATUS_ACCESS_VIOLATION",
    0xC0000409: "STATUS_STACK_BUFFER_OVERRUN",
    0xC00000FD: "STATUS_STACK_OVERFLOW",
    0xC0000374: "STATUS_HEAP_CORRUPTION",
    0xC000001D: "STATUS_ILLEGAL_INSTRUCTION",
    0xC0000094: "STATUS_INTEGER_DIVIDE_BY_ZERO",
    0xC000013A: "STATUS_CONTROL_C_EXIT",
}

# What the dynamic loaders write when they refuse a program, before it runs a line of its own:
# glibc's ld.so (a missing library, one without a version the program names, a symbol it lacks),
# musl's, and macOS's dyld.
_LOADER_WORDS = (
    re.compile(r": error while loading shared libraries: "),
    re.compile(r": version `[^']+' not found \(required by "),
    re.compile(r": symbol lookup error: "),
    re.compile(r"^Error loading shared library ", re.MULTILINE),
    re.compile(r"^dyld(?:\[\d+\])?: (?:Library not loaded|Symbol not found)", re.MULTILINE),
)


def ntstatus_name(exit_code: int | None) -> str | None:
    """The name of the Windows crash status ``exit_code`` is, or ``None`` for any other code."""
    if exit_code is None:
        return None
    return _WINDOWS_CRASH_STATUSES.get(exit_code % 2**32)


def describe_exit(exit_code: int | None, signal_name: str | None) -> str:
    """A process's end for a message: ``exit SIGKILL``, ``exit 1``, or ``exit 3221226505
    (0xC0000409 STATUS_STACK_BUFFER_OVERRUN)`` for a Windows crash status."""
    if signal_name is not None:
        return f"exit {signal_name}"
    if exit_code is None:
        return "exit unknown"
    name = ntstatus_name(exit_code)
    if name is None:
        return f"exit {exit_code}"
    return f"exit {exit_code} (0x{exit_code % 2**32:08X} {name})"


def loader_refused(exit_code: int | None, stderr: str) -> bool:
    """Whether a host that ended without answering was refused by the operating system's loader.

    A library it links (FFmpeg's above all) is missing or of another version. That is the machine's
    setup, so the same host fails the same way however often it is started.
    """
    if exit_code is not None and (exit_code % 2**32) in _WINDOWS_LOADER_STATUSES:
        return True
    return any(words.search(stderr) for words in _LOADER_WORDS)
