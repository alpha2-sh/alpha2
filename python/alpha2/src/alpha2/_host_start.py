"""What a host that did not start says about the machine: a start that timed out, a policy that
refused it, a loader that cannot run it."""

from __future__ import annotations

import errno
import os
import re
import sys
from pathlib import Path

from .install import LINUX_GLIBC_FLOOR, _linux_libc

DEFAULT_HOST_START_TIMEOUT = 30.0
"""Seconds a started host has to write its first line, by default."""

# The Win32 errors CreateProcess fails with when a policy refuses the executable:
# ERROR_INVALID_IMAGE_HASH, ERROR_ACCESS_DISABLED_BY_POLICY, ERROR_VIRUS_INFECTED,
# ERROR_VIRUS_DELETED, and the ERROR_SYSTEM_INTEGRITY_* range (Smart App Control and App Control
# policy violations and reputation verdicts).
_WINDOWS_POLICY_ERRORS = frozenset({577, 1260, 225, 226})
_WINDOWS_SYSTEM_INTEGRITY_ERRORS = range(4550, 4584)

_GLIBC_REFUSAL = re.compile(r"version `GLIBC_2\.\d+' not found")


def start_timeout_message(binary: Path, seconds: float) -> str:
    """Why a host wrote nothing within ``seconds``, and what may be holding it here."""
    if sys.platform == "darwin":
        hint = "; macOS may be holding it for a Gatekeeper check"
    elif sys.platform == "win32":
        hint = "; an antivirus scan or an App Control policy may be holding it"
    else:
        hint = ""
    return f"alpha2-host did not start within {seconds:g} s: {binary}{hint}"


def blocked_message(what: str) -> str:
    """The message of a ``HOST_BLOCKED``: Windows refused alpha2-host or a library it loads, as
    ``what`` (a status, or the spawn error) says."""
    return (
        f"Windows blocked alpha2-host or a library it loads ({what}): Smart App Control, an "
        "App Control (WDAC) or AppLocker policy, or an antivirus refuses unsigned code. Allow the "
        "files in that policy, or turn Smart App Control off (Windows Security > App & browser "
        "control)."
    )


def windows_status(exit_code: int) -> str:
    """A Windows exit status as a policy refusal names it: ``status 0xC0000428``."""
    return f"status 0x{exit_code % 2**32:08X}"


def spawn_refusal(binary: Path, error: OSError) -> tuple[str, str] | None:
    """What a failed start of ``binary`` says about the machine, as ``(code, message)``, when it
    says something no retry mends: a Windows policy that refused the executable
    (``HOST_BLOCKED``), or a Linux executable whose program interpreter is missing
    (``HOST_LOAD_FAILED``: a glibc build on a musl system fails ``ENOENT`` though the file is
    there)."""
    if not binary.is_file():
        return None
    winerror = getattr(error, "winerror", None)
    if sys.platform == "win32" and isinstance(winerror, int):
        if winerror in _WINDOWS_POLICY_ERRORS or winerror in _WINDOWS_SYSTEM_INTEGRITY_ERRORS:
            return "HOST_BLOCKED", blocked_message(f"error {winerror}")
        return None
    if sys.platform.startswith("linux") and error.errno == errno.ENOENT:
        interpreter = program_interpreter(binary)
        named = f"{interpreter} " if interpreter else ""
        return (
            "HOST_LOAD_FAILED",
            f"alpha2-host could not be loaded: {binary}: its program interpreter {named}is "
            "missing; this alpha2-host is built for glibc, and musl systems such as Alpine are "
            "not supported",
        )
    return None


def program_interpreter(path: Path) -> str | None:
    """The program interpreter ``path`` names: an ELF executable's ``PT_INTERP`` (the dynamic
    loader), or a script's ``#!`` line; ``None`` when it names none or cannot be read."""
    try:
        with open(path, "rb") as file:
            head = file.read(4096)
    except OSError:
        return None
    if head.startswith(b"#!"):
        words = head[2:].split(b"\n", 1)[0].split()
        return words[0].decode("latin-1") if words else None
    # A 64-bit little-endian ELF: the class and data bytes after the magic number.
    if len(head) < 64 or head[:4] != b"\x7fELF" or head[4] != 2 or head[5] != 1:
        return None
    table = int.from_bytes(head[0x20:0x28], "little")
    entry_size = int.from_bytes(head[0x36:0x38], "little")
    entries = int.from_bytes(head[0x38:0x3A], "little")
    pt_interp = 3
    for index in range(entries):
        entry = table + index * entry_size
        if entry + 40 > len(head):
            return None
        if int.from_bytes(head[entry : entry + 4], "little") != pt_interp:
            continue
        offset = int.from_bytes(head[entry + 8 : entry + 16], "little")
        size = int.from_bytes(head[entry + 32 : entry + 40], "little")
        if offset + size > len(head):
            return None
        return head[offset : offset + size].split(b"\0", 1)[0].decode("latin-1") or None
    return None


def glibc_hint(stderr: str) -> str:
    """What a loader refusal adds when glibc refused the host for being too old: the floor the
    host needs and the version this system has. Empty for any other refusal."""
    if not _GLIBC_REFUSAL.search(stderr):
        return ""
    kind, version = _linux_libc() if sys.platform.startswith("linux") else ("unknown", None)
    has = f"glibc {version}" if kind == "glibc" and version else kind
    return f"; this alpha2-host needs glibc {LINUX_GLIBC_FLOOR} or later (this system has {has})"


def oom_kill_count() -> int | None:
    """How many processes the kernel's out-of-memory killer has ended in this process's cgroup (the
    ``oom_kill`` line of cgroup v2's ``memory.events``), or ``None`` where there is none to read:
    off Linux, outside cgroup v2, or without the file. A host started by this process runs in the
    same cgroup, so a count that rose while a SIGKILLed host ran says the kernel killed it.
    ``ALPHA2_TEST_MEMORY_EVENTS`` names another file to read, for tests only."""
    path = os.environ.get("ALPHA2_TEST_MEMORY_EVENTS")
    if path is None:
        if not sys.platform.startswith("linux"):
            return None
        try:
            with open("/proc/self/cgroup", encoding="utf-8") as membership:
                relative = next(
                    (line[3:].strip() for line in membership if line.startswith("0::")), None
                )
        except OSError:
            return None
        if relative is None:
            return None
        path = f"/sys/fs/cgroup{relative.rstrip('/')}/memory.events"
    try:
        with open(path, encoding="utf-8") as events:
            for line in events:
                name, _, value = line.partition(" ")
                if name == "oom_kill":
                    return int(value)
    except (OSError, ValueError):
        return None
    return None
