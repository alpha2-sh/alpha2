"""Fixtures shared by the tests: the repository's corpora and fake hosts."""

from __future__ import annotations

import json
import os
import sys
import textwrap
import threading
from collections.abc import Callable
from pathlib import Path
from typing import Any, NoReturn

import pytest

ROOT = Path(__file__).resolve().parents[3]
"""The repository root, where the shared corpora live."""

CAPABILITIES: dict[str, Any] = json.loads(
    (ROOT / "tests" / "capabilities.json").read_text(encoding="utf-8")
)
"""Every capability a test can need, from ``tests/capabilities.json`` (shared with Rust and JS)."""

_SKIP_LOG_LOCK = threading.Lock()


def required_capabilities(environ: dict[str, str] | None = None) -> set[str]:
    """The capabilities ``ALPHA2_REQUIRE`` names: a trimmed comma list; unknown names fail."""
    value = (os.environ if environ is None else environ).get("ALPHA2_REQUIRE", "")
    names = {name.strip() for name in value.split(",") if name.strip()}
    unknown = sorted(names - CAPABILITIES.keys())
    if unknown:
        pytest.fail(
            f"ALPHA2_REQUIRE names unknown capabilities {unknown}; known: {', '.join(CAPABILITIES)}"
        )
    return names


def required(capability: str) -> bool:
    """Whether ``ALPHA2_REQUIRE`` names ``capability``; an unknown name fails the test."""
    if capability not in CAPABILITIES:
        pytest.fail(f"unknown capability {capability!r}")
    return capability in required_capabilities()


def require_or_skip(capability: str, reason: str, *, module: bool = False) -> NoReturn:
    """``capability`` is missing here: a failure when ``ALPHA2_REQUIRE`` names it, else a skip
    printed and appended to ``ALPHA2_SKIP_LOG`` (an absolute path) as one JSON line.

    ``module`` skips the whole module, from its top level.
    """
    if required(capability):
        pytest.fail(f"{capability} is required (ALPHA2_REQUIRE): {reason}")
    path = os.environ.get("ALPHA2_SKIP_LOG")
    if path:
        if not os.path.isabs(path):
            pytest.fail(f"ALPHA2_SKIP_LOG must be absolute ({path})")
        current = os.environ.get("PYTEST_CURRENT_TEST", "")
        test = "*" if module or not current else current.rsplit(" ", 1)[0]
        frame = sys._getframe(1)
        suite = Path(frame.f_code.co_filename).resolve().relative_to(ROOT).as_posix()
        record = {
            "lang": "python",
            "suite": suite,
            "test": test,
            "capability": capability,
            "reason": reason,
            "pid": os.getpid(),
        }
        line = (json.dumps(record) + "\n").encode("utf-8")
        # One write of one whole line on an append-mode file, so concurrent processes never
        # interleave records.
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with _SKIP_LOG_LOCK, open(path, "ab", buffering=0) as log:
            log.write(line)
    sys.stderr.write(f"skipped [{capability}]: {reason}\n")
    pytest.skip(reason, allow_module_level=module)


def corpus(name: str) -> Any:
    return json.loads((ROOT / "tests" / "cases" / name).read_text(encoding="utf-8"))


# What every fake host starts with: it reads its one request line and answers with `send`.
HOST_PRELUDE = """\
import json, os, signal, sys, time
# As the real host reads and writes: UTF-8 and bare newlines, whatever the platform's defaults are.
sys.stdin.reconfigure(encoding="utf-8")
sys.stdout.reconfigure(encoding="utf-8", newline="\\n")
sys.stderr.reconfigure(encoding="utf-8", newline="\\n")
def record(path, text):
    with open(path, "a", encoding="utf-8") as file:
        file.write(text + "\\n")
def read_request():
    line = sys.stdin.readline()
    return json.loads(line), line
def send(request, **message):
    sys.stdout.write(json.dumps({"protocolVersion": 1, "requestId": request["requestId"], **message}) + "\\n")
    sys.stdout.flush()
"""


@pytest.fixture
def fake_host(monkeypatch: pytest.MonkeyPatch) -> Callable[[Path, str], str]:
    """Writes a fake ``alpha2-host`` into a directory and returns the host path that runs it.

    The client starts ``<host_path> serve``. With ``host_path`` this Python, that runs the file
    ``serve`` in the working directory, which this makes the directory: unlike a script with a
    shebang, it starts the same way on every platform.
    """

    def make(directory: Path, source: str) -> str:
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "serve").write_text(HOST_PRELUDE + textwrap.dedent(source), encoding="utf-8")
        monkeypatch.chdir(directory)
        return sys.executable

    return make


def project_document() -> dict[str, Any]:
    """The smallest valid project: nothing on its timeline."""
    return {
        "schema_version": 1,
        "id": "project",
        "name": "Project",
        "settings": {
            "width": 16,
            "height": 16,
            "frame_rate": {"num": 30, "den": 1},
            "sample_rate": 48_000,
        },
        "sources": [],
        "timeline": {"tracks": []},
    }


def export_args(output: str | os.PathLike[str], /, **overrides: Any) -> dict[str, Any]:
    """Keyword arguments of a minimal export of :func:`project_document`."""
    return {
        "project": project_document(),
        "output": output,
        "assets": {},
        "start_ticks": 0,
        "end_ticks": 1,
        "frame_rate": {"num": 30, "den": 1},
        **overrides,
    }


def lines_in(path: Path) -> list[str]:
    """The non-empty lines a fake host recorded in ``path``, none when it wrote nothing."""
    try:
        return [line for line in path.read_text(encoding="utf-8").splitlines() if line]
    except FileNotFoundError:
        return []


def expand_repeats(value: Any) -> Any:
    """The validation corpus's shorthand, as alpha2-model's shared corpus test expands it.

    ``{"$repeat": N, "item": T}`` in an array is N elements, T at i = 0..N-1, with ``{i}`` in T's
    strings replaced by i and ``{"$index": {"start": a, "step": b}}`` by a + b*i. A ``$repeat``
    inside T counts its own i.
    """

    def is_repeat(node: Any) -> bool:
        return isinstance(node, dict) and "$repeat" in node

    def instantiate(template: Any, index: int) -> Any:
        if isinstance(template, str):
            return template.replace("{i}", str(index))
        if isinstance(template, list):
            return [instantiate(element, index) for element in template]
        if isinstance(template, dict):
            if is_repeat(template):
                return template
            counter = template.get("$index")
            if counter is not None and len(template) == 1:
                return counter["start"] + counter["step"] * index
            return {key: instantiate(field, index) for key, field in template.items()}
        return template

    if isinstance(value, list):
        expanded: list[Any] = []
        for element in value:
            if is_repeat(element):
                expanded.extend(
                    expand_repeats(instantiate(element["item"], index))
                    for index in range(element["$repeat"])
                )
            else:
                expanded.append(expand_repeats(element))
        return expanded
    if isinstance(value, dict):
        return {key: expand_repeats(field) for key, field in value.items()}
    return value


def read_rgba_png(path: Path) -> tuple[int, int, bytes]:
    """An 8-bit RGBA PNG's size and pixels, read with the standard library."""
    import struct
    import zlib

    data = path.read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    position, width, height, compressed = 8, 0, 0, b""
    while position < len(data):
        (length,) = struct.unpack(">I", data[position : position + 4])
        kind = data[position + 4 : position + 8]
        body = data[position + 8 : position + 8 + length]
        if kind == b"IHDR":
            width, height, depth, color = struct.unpack(">IIBB", body[:10])
            assert (depth, color) == (8, 6), "8-bit RGBA"
        elif kind == b"IDAT":
            compressed += body
        position += 12 + length
    raw = zlib.decompress(compressed)
    stride = width * 4
    rows: list[bytearray] = []
    previous = bytearray(stride)
    for row in range(height):
        start = row * (stride + 1)
        kind, line = raw[start], bytearray(raw[start + 1 : start + 1 + stride])
        for i in range(stride):
            left = line[i - 4] if i >= 4 else 0
            up = previous[i]
            corner = previous[i - 4] if i >= 4 else 0
            if kind == 1:
                line[i] = (line[i] + left) & 0xFF
            elif kind == 2:
                line[i] = (line[i] + up) & 0xFF
            elif kind == 3:
                line[i] = (line[i] + (left + up) // 2) & 0xFF
            elif kind == 4:
                estimate = left + up - corner
                near = min(
                    (abs(estimate - left), 0, left),
                    (abs(estimate - up), 1, up),
                    (abs(estimate - corner), 2, corner),
                )[2]
                line[i] = (line[i] + near) & 0xFF
        rows.append(line)
        previous = line
    return width, height, b"".join(rows)
