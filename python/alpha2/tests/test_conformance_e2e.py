"""The extract cases of ``tests/cases/surface-conformance.json`` against a real ``alpha2-host`` on
the CPU renderer, mirroring ``@alpha2-sh/node``'s test. The edit cases run in ``test_edit``.

The host is ``ALPHA2_HOST_BINARY``, else ``target/debug/alpha2-host``; without one the module is
skipped.
"""

from __future__ import annotations

import hashlib
import json
import os
import struct
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT, require_or_skip

from alpha2 import HostClient
from alpha2.extract import Emitter, ExtractFrame

HOST = Path(
    os.environ.get("ALPHA2_HOST_BINARY")
    or ROOT / "target" / "debug" / ("alpha2-host.exe" if sys.platform == "win32" else "alpha2-host")
)
if not HOST.is_file():
    require_or_skip(
        "host", f"no alpha2-host at {HOST}; build it or set ALPHA2_HOST_BINARY", module=True
    )

TABLE = json.loads((ROOT / "tests" / "cases" / "surface-conformance.json").read_text("utf-8"))


def bits(value: float | None) -> str:
    """The 8 lowercase hex digits of ``value``'s f32 bits, or ``-`` for null."""
    if value is None:
        return "-"
    return f"{struct.unpack('<I', struct.pack('<f', value))[0]:08x}"


def line(found: dict[str, Any]) -> str:
    kind = found["kind"]
    if kind == "intervals":
        items = [
            f"{item['start']}:{item['end']}:{item.get('label', '')}:"
            + ("" if item.get("score") is None else bits(item["score"]))
            for item in found["intervals"]
        ]
    elif kind == "words":
        items = [
            f"{item['start']}:{item['end']}:{item['text']}:{item.get('speaker', '')}"
            for item in found["words"]
        ]
    else:
        width = len(found["values"]) // max(len(found["times"]), 1)
        items = [
            f"{time}:" + ",".join(bits(v) for v in found["values"][i * width : (i + 1) * width])
            for i, time in enumerate(found["times"])
        ]
    return f"{found['id']} {kind}|{';'.join(items)}"


def data_hash(sets: list[dict[str, Any]]) -> str:
    """The table's ``data_hash`` of ``sets``."""
    return hashlib.sha256("\n".join(line(found) for found in sets).encode()).hexdigest()


def fnv1a(data: bytes) -> int:
    """32-bit FNV-1a, as the digest fixture hashes deliveries on every surface."""
    value = 0x811C9DC5
    for byte in data:
        value = ((value ^ byte) * 0x01000193) & 0xFFFFFFFF
    return value


class Digest:
    """The digest fixture as a client extractor with the table's descriptor."""

    def __init__(self, descriptor: dict[str, Any]) -> None:
        self.descriptor = descriptor

    def describe(self) -> dict[str, Any]:
        return self.descriptor

    def video(self, frame: ExtractFrame, out: Emitter) -> None:
        value = fnv1a(frame.data)
        out.sample("digest", frame.time, [(value >> 16) / 65536, (value & 0xFFFF) / 65536])


@pytest.fixture(scope="module")
def client() -> Iterator[HostClient]:
    with HostClient(host_path=HOST, renderer="cpu") as client:
        yield client


@pytest.fixture(scope="module")
def packed(tmp_path_factory: pytest.TempPathFactory) -> Any:
    """Packs ``plugins/<name>`` once: its manifest's path and SHA-256."""
    done: dict[str, dict[str, str]] = {}
    out = tmp_path_factory.mktemp("plugins")
    env = {key: value for key, value in os.environ.items() if key != "CARGO_TARGET_DIR"}

    def pack(name: str) -> dict[str, str]:
        if name not in done:
            plugins = ROOT / "plugins"
            subprocess.run(
                [
                    "cargo", "run", "--release",
                    "--manifest-path", str(plugins / "Cargo.toml"),
                    "--target-dir", str(plugins / "target"),
                    "-p", "alpha2-plugin-pack", "--",
                    str(plugins / name), "--out", str(out / name),
                ],
                check=True,
                env=env,
            )  # fmt: skip
            manifest = out / name / "manifest.json"
            done[name] = {
                "manifest": str(manifest),
                "sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
            }
        return done[name]

    return pack


@pytest.mark.parametrize("case", TABLE["extract"], ids=lambda case: case["name"])
def test_each_extract_case_hashes_as_pinned(
    client: HostClient, packed: Any, case: dict[str, Any]
) -> None:
    project = TABLE["projects"][case["project"]]
    assets = {key: ROOT / path for key, path in case["assets"].items()}
    plugin = case["extractor"].get("plugin")
    if plugin is None:
        result = client.extract(
            project, case["request"], Digest(case["extractor"]["client"]), assets=assets
        )
    else:
        result = client.extract(project, case["request"], plugin=packed(plugin), assets=assets)
    assert data_hash(result["dataSets"]) == case["sha256"]
