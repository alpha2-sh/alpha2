"""macOS quarantine: files a browser downloaded carry an attribute, and macOS holds a program that
needs such a file on an approval prompt when Gatekeeper does not accept it."""

from __future__ import annotations

import ctypes
import ctypes.util
import os
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any, Literal, Protocol

__all__ = ["DEFAULT_PROBE", "QuarantineProbe", "quarantine_refusal"]

Kind = Literal["execute", "library"]

# How long Gatekeeper has to assess one file; it may ask Apple online.
_ASSESSMENT_TIMEOUT = 10.0


class QuarantineProbe(Protocol):
    """Whether macOS quarantined a file, and whether Gatekeeper accepts it as ``kind`` (``None``
    when Gatekeeper did not answer)."""

    def quarantined(self, path: Path) -> bool: ...

    def accepted(self, path: Path, kind: Kind) -> bool | None: ...


class _SystemProbe:
    """``getxattr`` from libSystem and ``spctl``, as macOS ships them."""

    def __init__(self) -> None:
        self._getxattr: Any = None

    def quarantined(self, path: Path) -> bool:
        if self._getxattr is None:
            library = ctypes.CDLL(
                ctypes.util.find_library("System") or "libSystem.dylib", use_errno=True
            )
            function = library.getxattr
            function.argtypes = [
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.c_void_p,
                ctypes.c_size_t,
                ctypes.c_uint32,
                ctypes.c_int,
            ]
            function.restype = ctypes.c_ssize_t
            self._getxattr = function
        size: int = self._getxattr(os.fsencode(path), b"com.apple.quarantine", None, 0, 0, 0)
        # -1 with ENOATTR is "not quarantined"; any other failure is not evidence of quarantine.
        return size >= 0

    def accepted(self, path: Path, kind: Kind) -> bool | None:
        arguments = (
            ["--assess", "--type", "execute", str(path)]
            if kind == "execute"
            else ["--assess", "--type", "open", "--context", "context:primary-signature", str(path)]
        )
        try:
            completed = subprocess.run(
                ["/usr/sbin/spctl", *arguments],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=_ASSESSMENT_TIMEOUT,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        return completed.returncode == 0


DEFAULT_PROBE: QuarantineProbe = _SystemProbe()


def _files_of(binary: Path) -> list[tuple[Path, Kind]]:
    """The files ``binary`` needs that macOS checks when they are loaded: the executable, every
    library in the archive's ``lib/`` beside its ``bin/`` (its files, not the links that name them
    again), and the OpenH264 library ``ALPHA2_OPENH264_LIBRARY`` names."""
    files: list[tuple[Path, Kind]] = [(binary, "execute")]
    lib = binary.parent.parent / "lib"
    try:
        names = sorted(os.listdir(lib))
    except OSError:
        names = []
    files.extend(
        (lib / name, "library")
        for name in names
        if name.endswith(".dylib") and (lib / name).is_file() and not (lib / name).is_symlink()
    )
    openh264 = os.environ.get("ALPHA2_OPENH264_LIBRARY")
    if openh264 and Path(openh264).is_file():
        files.append((Path(openh264), "library"))
    return files


def quarantine_refusal(binary: Path, probe: QuarantineProbe = DEFAULT_PROBE) -> str | None:
    """Why macOS would hold ``binary`` on an approval prompt rather than run it: files it needs
    carry the quarantine attribute and Gatekeeper does not accept them. ``None`` when nothing is
    quarantined, Gatekeeper accepts what is, or Gatekeeper did not answer (the start timeout then
    reports a held start)."""
    quarantined = [(path, kind) for path, kind in _files_of(binary) if probe.quarantined(path)]
    # Assessed at once: each may take a round trip to Apple.
    with ThreadPoolExecutor(max_workers=8) as pool:
        verdicts = list(pool.map(lambda file: probe.accepted(*file), quarantined))
    rejected = [
        path for (path, _), verdict in zip(quarantined, verdicts, strict=True) if verdict is False
    ]
    if not rejected:
        return None
    root = binary.parent.parent if binary.parent.name == "bin" else binary.parent
    named = ", ".join(str(path) for path in rejected[:3])
    more = f" and {len(rejected) - 3} more" if len(rejected) > 3 else ""
    return (
        f"macOS quarantined {len(rejected)} file(s) alpha2-host needs and Gatekeeper does not "
        f"accept them, so it would hold alpha2-host on an approval prompt: {named}{more}. Remove "
        f"the attribute from the unpacked archive: xattr -dr com.apple.quarantine '{root}' (files "
        "downloaded with a browser carry it), or install with `python -m alpha2 install` (or "
        "`pip install alpha2`), which downloads without it."
    )
