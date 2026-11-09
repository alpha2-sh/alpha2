"""Needs a host, as ``test_e2e.py`` does."""

from __future__ import annotations

import hashlib
import json
import os
import signal
import struct
import subprocess
import sys
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT, require_or_skip

from alpha2 import Alpha2AbortError, Alpha2HostError, HostClient
from alpha2.client import ExtractPlugin
from alpha2.extract import Emitter, ExtractAudio, ExtractFrame, ExtractProgress

HOST = Path(
    os.environ.get("ALPHA2_HOST_BINARY")
    or ROOT / "target" / "debug" / ("alpha2-host.exe" if sys.platform == "win32" else "alpha2-host")
)
if not HOST.is_file():
    require_or_skip(
        "host", f"no alpha2-host at {HOST}; build it or set ALPHA2_HOST_BINARY", module=True
    )

PROJECT: dict[str, Any] = {
    "schema_version": 1,
    "id": "p",
    "name": "P",
    "settings": {
        "width": 320,
        "height": 180,
        "frame_rate": {"num": 30, "den": 1},
        "sample_rate": 48_000,
    },
    "sources": [{"id": "v", "kind": "video", "locator": "v.mp4"}],
    "timeline": {"tracks": []},
}


@pytest.fixture(scope="module")
def client() -> Iterator[HostClient]:
    with HostClient(host_path=HOST) as client:
        yield client


def fnv1a(data: bytes) -> int:
    """32-bit FNV-1a, as the digest fixture hashes deliveries on every surface."""
    value = 0x811C9DC5
    for byte in data:
        value = ((value ^ byte) * 0x01000193) & 0xFFFFFFFF
    return value


def halves(value: int) -> list[float | None]:
    return [(value >> 16) / 65536, (value & 0xFFFF) / 65536]


class Digest:
    """One ``scalars`` sample per delivery: the hash's halves over 65536."""

    def __init__(self, input_spec: dict[str, Any]) -> None:
        self.input = input_spec

    def describe(self) -> dict[str, Any]:
        return {
            "id": "alpha2.test.digest",
            "version": 1,
            "input": self.input,
            "params_schema": {"type": "object"},
            "outputs": [
                {
                    "name": "digest",
                    "kind": "scalars",
                    "channels": ["hi", "lo"],
                    "interpolation": "hold",
                }
            ],
        }

    def video(self, frame: ExtractFrame, out: Emitter) -> None:
        out.sample("digest", frame.time, halves(fnv1a(frame.data)))

    def audio(self, block: ExtractAudio, out: Emitter) -> None:
        data = struct.pack(f"<{len(block.samples)}f", *block.samples)
        out.sample("digest", block.start, halves(fnv1a(data)))


class Scripted:
    """Emits a given list per delivery, in order, and another at the end."""

    def __init__(
        self,
        outputs: list[dict[str, Any]],
        per_delivery: list[list[dict[str, Any]]],
        at_end: list[dict[str, Any]] | None = None,
    ) -> None:
        self.outputs = outputs
        self.per_delivery = list(per_delivery)
        self.at_end = at_end or []

    def describe(self) -> dict[str, Any]:
        return {
            "id": "alpha2.test.scripted",
            "version": 1,
            "input": {"media": "video", "from": ["source"], "format": "luma8"},
            "params_schema": {
                "type": "object",
                "properties": {"threshold": {"type": "number", "minimum": 0}},
                "additionalProperties": False,
            },
            "outputs": self.outputs,
        }

    @staticmethod
    def _emit(out: Emitter, items: list[dict[str, Any]]) -> None:
        for item in items:
            if "values" in item:
                out.sample(item["output"], item["time"], item["values"])
            elif "text" in item:
                out.word(item["output"], item["start"], item["end"], item["text"])
            else:
                out.interval(item["output"], item["start"], item["end"], item.get("label"))

    def video(self, _frame: ExtractFrame, out: Emitter) -> None:
        self._emit(out, self.per_delivery.pop(0) if self.per_delivery else [])

    def end(self, out: Emitter) -> None:
        self._emit(out, self.at_end)


def request(extractor: Any, end: int, size: int, **extra: Any) -> dict[str, Any]:
    names = [output["name"] for output in extractor.describe()["outputs"]]
    return {
        "extractor": extractor.describe()["id"],
        "version": 1,
        "input": {
            "from": "source",
            "source_id": "v",
            "range": {"start": 0, "end": end},
            "size": size,
        },
        "outputs": {name: name for name in names},
        **extra,
    }


def video(name: str = "h264-bt709-copy.mp4") -> dict[str, Path]:
    return {"v": ROOT / "tests" / "corpus" / name}


def test_the_python_digest_fixture_records_the_bytes_the_rust_reader_pins(
    client: HostClient,
) -> None:
    pinned = json.loads((ROOT / "tests/golden/extract-recipe.json").read_text())
    for name in ("h264-bt709-copy.mp4", "rotation.mp4", "hdr-pq.mp4"):
        progress: list[ExtractProgress] = []
        extractor = Digest({"media": "video", "from": ["source"], "format": "rgba8"})
        result = client.extract(
            PROJECT,
            request(extractor, 40_000, 96),
            extractor,
            assets=video(name),
            on_progress=progress.append,
        )
        (found,) = result["dataSets"]
        hashes = [
            [
                time,
                f"{round(found['values'][i * 2] * 65536) * 65536 + round(found['values'][i * 2 + 1] * 65536):08x}",
            ]
            for i, time in enumerate(found["times"])
        ]
        assert hashes == pinned[name], name
        assert found["made_from"], name
        assert progress


def test_an_invalid_emit_is_refused(client: HostClient) -> None:
    extractor = Scripted(
        [{"name": "track", "kind": "points", "channels": ["a"]}],
        [[{"output": "track", "time": 0, "values": [20, 0]}]],
    )
    with pytest.raises(Alpha2HostError) as raised:
        client.extract(PROJECT, request(extractor, 40_000, 64), extractor, assets=video())
    assert raised.value.code == "INVALID_EMIT"


def test_params_are_checked_against_the_schema(client: HostClient) -> None:
    extractor = Scripted([{"name": "track", "kind": "points", "channels": ["a"]}], [])
    with pytest.raises(Alpha2HostError) as raised:
        client.extract(
            PROJECT,
            request(extractor, 40_000, 64, params={"threshold": -1}),
            extractor,
            assets=video(),
        )
    assert raised.value.code == "INVALID_INPUT"


def test_an_extractor_that_raises_is_extractor_failed(client: HostClient) -> None:
    class Failing(Scripted):
        def video(self, _frame: ExtractFrame, _out: Emitter) -> None:
            raise RuntimeError("model missing")

    extractor = Failing([{"name": "track", "kind": "points", "channels": ["a"]}], [])
    with pytest.raises(Alpha2HostError) as raised:
        client.extract(PROJECT, request(extractor, 40_000, 64), extractor, assets=video())
    assert raised.value.code == "EXTRACTOR_FAILED"
    assert "model missing" in str(raised.value)


def test_cancel_mid_pass_aborts(client: HostClient) -> None:
    cancel = threading.Event()

    class Cancelling(Digest):
        def video(self, frame: ExtractFrame, out: Emitter) -> None:
            if frame.time >= 8_000:
                cancel.set()
            out.sample("digest", frame.time, [0, 0])

    extractor = Cancelling({"media": "video", "from": ["source"], "format": "luma8"})
    with pytest.raises(Alpha2AbortError):
        client.extract(
            PROJECT, request(extractor, 200_000, 32), extractor, assets=video(), cancel=cancel
        )


@pytest.mark.skipif(sys.platform != "linux", reason="finds the host by its parent pid")
def test_a_host_exit_mid_run_fails_and_the_pool_starts_another(client: HostClient) -> None:
    class Killing(Digest):
        def video(self, frame: ExtractFrame, out: Emitter) -> None:
            if frame.time == 8_000:
                pids = subprocess.run(
                    ["pgrep", "-P", str(os.getpid())], capture_output=True, text=True, check=True
                ).stdout.split()
                for pid in pids:
                    os.kill(int(pid), signal.SIGKILL)
            out.sample("digest", frame.time, [0, 0])

    killing = Killing({"media": "video", "from": ["source"], "format": "luma8"})
    with pytest.raises(Alpha2HostError) as raised:
        client.extract(PROJECT, request(killing, 40_000, 32), killing, assets=video())
    assert raised.value.code == "HOST_EXITED"
    digest = Digest({"media": "video", "from": ["source"], "format": "luma8"})
    result = client.extract(PROJECT, request(digest, 4_000, 32), digest, assets=video())
    assert len(result["dataSets"][0]["times"]) == 1


def test_the_python_scripted_fixture_lists_become_data_sets(client: HostClient) -> None:
    extractor = Scripted(
        [
            {"name": "words", "kind": "words"},
            {"name": "quiet", "kind": "intervals", "suggest": "remove_ranges"},
        ],
        # The interval is emitted once the pass has delivered past its end.
        [[], [], [{"output": "quiet", "start": 0, "end": 4_000, "label": "silence"}]],
        [
            {"output": "words", "start": 12_000, "end": 20_000, "text": "late"},
            {"output": "words", "start": 0, "end": 8_000, "text": "early"},
        ],
    )
    result = client.extract(PROJECT, request(extractor, 40_000, 32), extractor, assets=video())
    by_id = {found["id"]: found for found in result["dataSets"]}
    assert by_id["words"]["words"] == [
        {"start": 0, "end": 8_000, "text": "early"},
        {"start": 12_000, "end": 20_000, "text": "late"},
    ]
    assert by_id["quiet"]["intervals"] == [{"start": 0, "end": 4_000, "label": "silence"}]
    for found in result["dataSets"]:
        assert found["anchor"] == {"type": "source", "source_id": "v"}
        assert found["made_by"]["extractor"] == "alpha2.test.scripted"


def host_triple() -> str:
    output = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    return next(line[6:] for line in output.splitlines() if line.startswith("host: "))


def plugin_dir(tmp: Path, name: str, *, wasm: bool = True, native: bool = False) -> tuple[str, str]:
    """Builds fixture ``name`` and writes it as a plugin directory: the manifest's path and hash."""
    plugins = ROOT / "plugins"
    triple = host_triple()
    for target in ("wasm32-unknown-unknown", triple):
        subprocess.run(
            [
                "cargo", "build", "--release", "--target", target,
                "--manifest-path", str(plugins / "Cargo.toml"),
                "--target-dir", str(plugins / "target"),
                "-p", f"alpha2-plugin-fixture-{name}",
            ],
            check=True,
            env={**os.environ, "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS": "-Ctarget-feature=-relaxed-simd"},
        )  # fmt: skip
    library = f"alpha2_plugin_fixture_{name}"
    artifacts = []

    def place(source: Path, url: str, target: str, kind: str) -> None:
        data = source.read_bytes()
        (tmp / url).parent.mkdir(parents=True, exist_ok=True)
        (tmp / url).write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        artifacts.append(
            {"target": target, "type": kind, "url": url, "sha256": digest, "bytes": len(data)}
        )

    release = plugins / "target"
    if wasm:
        place(
            release / "wasm32-unknown-unknown/release" / f"{library}.wasm",
            f"{library}.wasm",
            "wasm32",
            "wasm",
        )
    if native:
        if "windows" in triple:
            file = f"{library}.dll"
        elif "apple" in triple:
            file = f"lib{library}.dylib"
        else:
            file = f"lib{library}.so"
        place(release / triple / "release" / file, f"{triple}/{file}", triple, "cdylib")
    descriptor = json.loads((plugins / "fixtures" / name / "descriptor.json").read_text())
    text = json.dumps(
        {"id": descriptor["id"], "version": "0.1.0", "abi": "alpha2-plugin/1", "kind": "extractor",
         "artifacts": artifacts, "descriptor": descriptor}
    )  # fmt: skip
    (tmp / "manifest.json").write_text(text)
    return str(tmp / "manifest.json"), hashlib.sha256(text.encode()).hexdigest()


def plugin_request(
    extractor: str, outputs: dict[str, str], params: dict[str, Any]
) -> dict[str, Any]:
    return {
        "extractor": extractor,
        "version": 1,
        "params": params,
        "input": {
            "from": "source",
            "source_id": "v",
            "range": {"start": 0, "end": 40_000},
            "size": 96,
        },
        "outputs": outputs,
    }


def test_a_plugin_the_host_runs_records_the_bytes_the_rust_reader_pins(
    client: HostClient, tmp_path: Path
) -> None:
    pinned = json.loads((ROOT / "tests/golden/extract-recipe.json").read_text())
    manifest, sha256 = plugin_dir(tmp_path, "digest")
    result = client.extract(
        PROJECT,
        plugin_request("alpha2.test.digest", {"digest": "frames"}, {}),
        plugin={"manifest": manifest, "sha256": sha256},
        assets=video(),
    )
    (found,) = result["dataSets"]
    values = found["values"]
    hashes = [
        [time, f"{round(values[i * 2] * 65536) * 65536 + round(values[i * 2 + 1] * 65536):08x}"]
        for i, time in enumerate(found["times"])
    ]
    assert hashes == pinned["h264-bt709-copy.mp4"]
    assert found["made_by"]["plugin"]["version"] == "0.1.0"
    with pytest.raises(Alpha2HostError) as raised:
        client.extract(
            PROJECT,
            plugin_request("alpha2.test.digest", {"digest": "frames"}, {}),
            plugin={"manifest": manifest, "sha256": "0" * 64},
            assets=video(),
        )
    assert raised.value.code == "PLUGIN_HASH_MISMATCH"


@pytest.mark.skipif(
    sys.platform == "win32", reason="the crash is an abort, which Windows reports by dialog"
)
def test_a_native_plugin_that_ends_the_host_fails_and_the_pool_starts_another(
    client: HostClient, tmp_path: Path
) -> None:
    manifest, sha256 = plugin_dir(tmp_path, "scripted", wasm=False, native=True)
    outputs = {"track": "t", "levels": "l", "correction": "c", "words": "w", "quiet": "q"}
    with pytest.raises(Alpha2HostError) as refused:
        client.extract(
            PROJECT,
            plugin_request("alpha2.test.scripted", outputs, {"abort_at": 1}),
            plugin={"manifest": manifest, "sha256": sha256},
            assets=video(),
        )
    assert refused.value.code == "PLUGIN_NATIVE_REFUSED"
    allowed: ExtractPlugin = {"manifest": manifest, "sha256": sha256, "allow_native": True}
    with pytest.raises(Alpha2HostError) as ended:
        client.extract(
            PROJECT,
            plugin_request("alpha2.test.scripted", outputs, {"abort_at": 1}),
            plugin=allowed,
            assets=video(),
        )
    assert ended.value.code == "HOST_EXITED"
    after = client.extract(
        PROJECT,
        plugin_request("alpha2.test.scripted", outputs, {}),
        plugin=allowed,
        assets=video(),
    )
    assert len(after["dataSets"]) == 5


def test_conform_reports_data_made_from_other_media_once_its_source_is_relinked(
    client: HostClient,
) -> None:
    clip = {
        "id": "c",
        "source_id": "v",
        "timeline_range": {"start": 0, "end": 40_000},
        "source_range": {"start": 0, "end": 40_000},
    }
    placed = {
        **PROJECT,
        "timeline": {"tracks": [{"id": "t", "name": "T", "kind": "video", "clips": [clip]}]},
    }
    first = client.conform(placed, assets=video())
    fingerprint = first["catalog"]["assets"][0]["original"]["fingerprint"]
    project = {
        **placed,
        "data": [
            {
                "id": "track",
                "anchor": {"type": "source", "source_id": "v"},
                "kind": "intervals",
                "intervals": [],
                "made_from": fingerprint,
            }
        ],
    }
    assert client.conform(project, assets=video())["report"]["stale_data"] == []
    relinked = client.conform(project, assets=video("rotation.mp4"))
    assert [entry["data"] for entry in relinked["report"]["stale_data"]] == ["track"]
