"""Fixtures shared by the tests: the repository's corpora and fake hosts."""

from __future__ import annotations

import json
import os
import sys
import textwrap
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[3]
"""The repository root, where the shared corpora live."""


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
