"""The messages of the protocol spoken with ``alpha2-host serve`` over stdio."""

from __future__ import annotations

import json
import re
from typing import Any

__all__ = [
    "HOST_STATES",
    "PROTOCOL_VERSION",
    "UNREAD_REQUEST_ID",
    "create_cancel",
    "create_request",
    "create_status",
    "describe_exit",
    "loader_refused",
    "ntstatus_name",
    "parse_host_message",
    "windows_policy_blocked",
]

PROTOCOL_VERSION = 2

# The requestId the host uses when it could not read the line's own.
UNREAD_REQUEST_ID = "unknown"

HOST_STATES = ("ready", "draining")
"""A host takes requests while ``ready``; ``draining`` takes none and exits."""


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


def create_cancel(target: str) -> dict[str, Any]:
    """A cancel of the request ``target``."""
    return create_request("cancel", f"{target}:cancel", {"target": target})


def create_status(request_id: str) -> dict[str, Any]:
    """A ``status`` request, answered at once with the host's state."""
    return create_request("status", request_id)


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


def _attachments(value: object) -> bool:
    return isinstance(value, list) and all(
        isinstance(entry, dict)
        and isinstance(entry.get("type"), str)
        and len(entry["type"]) > 0
        and _safe_count(entry.get("bytes"))
        for entry in value
    )


def parse_host_message(header: str) -> dict[str, Any]:
    """Parses and checks the header of one frame the host wrote.

    A message is ``state``, ``progress``, ``extract.step``, ``fatal``, or a response (``ok``, with
    ``hostState``). ``attachments``, when present, lists ``{"type", "bytes"}`` entries. Raises
    ``ValueError`` for non-JSON, another protocol version, or a malformed message.
    """
    value = json.loads(header)
    if not isinstance(value, dict):
        raise ValueError("host message must be an object")
    version = value.get("protocolVersion")
    if version != PROTOCOL_VERSION or isinstance(version, bool):
        raise ValueError(f"unsupported host protocol {version}; expected {PROTOCOL_VERSION}")
    if "attachments" in value and not _attachments(value["attachments"]):
        raise ValueError("invalid attachment list")
    kind = value.get("type")
    if kind == "state":
        if (
            value.get("state") not in HOST_STATES
            or not isinstance(value.get("reason"), str)
            or not isinstance(value.get("details"), dict)
        ):
            raise ValueError("invalid state message")
        return value
    if kind == "progress":
        extract = value.get("phase") == "extract"
        total = value.get("total")
        if (
            not isinstance(value.get("requestId"), str)
            or not _safe_count(value.get("completed"))
            or not (_safe_count(total) or (extract and total is None))
            or (total is not None and value["completed"] > total)
            or ("phase" in value and value["phase"] not in ("frames", "finalizing", "extract"))
        ):
            raise ValueError("invalid progress message")
        return value
    if kind == "extract.step":
        if (
            not isinstance(value.get("requestId"), str)
            or not _safe_count(value.get("seq"))
            or value.get("step") not in ("begin", "pass", "video", "audio", "end")
        ):
            raise ValueError("invalid extract step")
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
    if value.get("hostState") not in HOST_STATES:
        raise ValueError("response is missing hostState")
    return value


# DLL load failures, which end the process before `main` with nothing on stderr:
# STATUS_DLL_NOT_FOUND, STATUS_ORDINAL_NOT_FOUND, STATUS_ENTRYPOINT_NOT_FOUND and
# STATUS_INVALID_IMAGE_FORMAT.
_WINDOWS_LOADER_STATUSES = frozenset({0xC0000135, 0xC0000138, 0xC0000139, 0xC000007B})

# Policy refusals: STATUS_INVALID_IMAGE_HASH (Code Integrity) and
# STATUS_ACCESS_DISABLED_BY_POLICY_DEFAULT, _PATH, _PUBLISHER and _OTHER (SRP, AppLocker).
_WINDOWS_POLICY_STATUSES = frozenset({0xC0000428, 0xC0000361, 0xC0000362, 0xC0000363, 0xC0000364})

# STATUS_SYSTEM_INTEGRITY_* (0xC0E9xxxx).
_WINDOWS_SYSTEM_INTEGRITY_FACILITY = 0xC0E90000

_WINDOWS_CRASH_STATUSES = {
    0xC0000005: "STATUS_ACCESS_VIOLATION",
    0xC0000409: "STATUS_STACK_BUFFER_OVERRUN",
    0xC00000FD: "STATUS_STACK_OVERFLOW",
    0xC0000374: "STATUS_HEAP_CORRUPTION",
    0xC000001D: "STATUS_ILLEGAL_INSTRUCTION",
    0xC0000094: "STATUS_INTEGER_DIVIDE_BY_ZERO",
    0xC000013A: "STATUS_CONTROL_C_EXIT",
}

# Messages from glibc's ld.so, musl's loader and macOS dyld when they refuse a program.
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


def windows_policy_blocked(exit_code: int | None) -> bool:
    """Whether a Windows policy (Smart App Control, WDAC, AppLocker, reputation) refused the host
    before it ran."""
    if exit_code is None:
        return False
    status = exit_code % 2**32
    return (
        status in _WINDOWS_POLICY_STATUSES
        or status & 0xFFFF0000 == _WINDOWS_SYSTEM_INTEGRITY_FACILITY
    )


def loader_refused(exit_code: int | None, stderr: str) -> bool:
    """Whether the loader refused the host, typically for a missing or mismatched FFmpeg."""
    if exit_code is not None and (exit_code % 2**32) in _WINDOWS_LOADER_STATUSES:
        return True
    return any(words.search(stderr) for words in _LOADER_WORDS)
