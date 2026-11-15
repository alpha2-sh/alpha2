"""The host client against fake hosts: request checks, process lifecycle, errors and pooling."""

from __future__ import annotations

import json
import math
import os
import re
import signal
import subprocess
import sys
import textwrap
import threading
import time
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
from pathlib import Path
from typing import Any

import pytest
from conftest import FakeHost, corpus, export_args, lines_in, project_document

import alpha2
from alpha2 import (
    Alpha2AbortError,
    Alpha2GpuInitializationError,
    Alpha2HostError,
    Alpha2ProjectValidationError,
    HostClient,
)
from alpha2.client import _HOST_SHUTDOWN_TIMEOUT, resolve_host_binary
from alpha2.errors import failure_class_of_code
from alpha2.install import host_executable_name
from alpha2.protocol import describe_exit, windows_policy_blocked


def pid_exited(pid: int) -> bool:
    """On Windows ``os.kill`` with signal 0 would end the process, so it is opened and waited on
    for up to a second instead (a job-object child may still be ending)."""
    if sys.platform == "win32":
        import ctypes

        synchronize = 0x0010_0000
        wait_object_0 = 0
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.OpenProcess.argtypes = (ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong)
        kernel32.OpenProcess.restype = ctypes.c_void_p
        kernel32.WaitForSingleObject.argtypes = (ctypes.c_void_p, ctypes.c_ulong)
        kernel32.WaitForSingleObject.restype = ctypes.c_ulong
        kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
        handle = kernel32.OpenProcess(synchronize, False, pid)
        if not handle:
            return True
        try:
            return bool(kernel32.WaitForSingleObject(handle, 1000) == wait_object_0)
        finally:
            kernel32.CloseHandle(handle)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return False


def wait_until(condition: Callable[[], bool], seconds: float = 10) -> None:
    deadline = time.monotonic() + seconds
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.02)


@pytest.fixture
def started_pids(monkeypatch: pytest.MonkeyPatch) -> list[int]:
    """Host pids as the client sees them; on Windows a venv's ``python.exe`` is a launcher, so the
    fake host's own pid differs."""
    pids: list[int] = []

    class Recorded(subprocess.Popen):
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            super().__init__(*args, **kwargs)
            pids.append(self.pid)

    monkeypatch.setattr(subprocess, "Popen", Recorded)
    return pids


def started_host(starts: Path) -> str:
    return f"""
    record({str(starts)!r}, os.environ.get("ALPHA2_VIDEO_DECODE", "unset"))
    request, _ = read_request()
    if request["operation"] == "capabilities":
        send(request, ok=True, result={{"status": "ready", "capabilities": {{}}, "effectCatalog": {{}}}})
    else:
        send(request, ok=True, result={{"output": request.get("output")}})
    """


def answering_host(answer: str) -> str:
    return f"""
    request, _ = read_request()
    message = {answer}
    if "ok" in message:
        send(request, **message)
    else:
        write_message(message)
    """


def wire_to_python(fields: dict[str, Any]) -> dict[str, Any]:
    def snake(name: str) -> str:
        return re.sub(r"(?<!^)([A-Z])", r"_\1", name).lower()

    arguments: dict[str, Any] = {}
    for key, value in fields.items():
        if key in ("loudness", "renderCache", "memory") and isinstance(value, dict):
            value = {snake(inner): field for inner, field in value.items()}
        arguments[snake(key)] = value
    return arguments


def test_the_plugins_option_becomes_the_serve_flags_and_a_malformed_one_starts_no_host(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    host_path = fake_host(
        tmp_path,
        f"""
    record({str(starts)!r}, json.dumps(sys.argv[1:]))
    request, _ = read_request()
    send(request, ok=True, result={{"status": "ready", "capabilities": {{}}, "effectCatalog": {{}}}})
    """,
    )

    def flags(plugins: Any) -> list[list[str]]:
        starts.unlink(missing_ok=True)
        with HostClient(host_path=host_path, plugins=plugins) as client:
            client.capabilities()
        return [json.loads(line) for line in lines_in(starts)]

    assert flags(None) == [[]]
    assert flags(
        {"mode": "native", "origins": ["https://a.example", "https://b.example:8443"], "root": "p"}
    ) == [
        [
            "--plugins",
            "native",
            "--plugin-origins",
            "https://a.example,https://b.example:8443",
            "--plugin-root",
            str(tmp_path / "p"),
        ]
    ]
    assert flags({"origins": "none", "root": "none"}) == [
        ["--plugin-origins", "none", "--plugin-root", "none"]
    ]
    starts.unlink(missing_ok=True)
    for refused in [
        {"mode": "all"},
        {"origins": []},
        {"origins": ["http://a.example"]},
        {"origins": "https://a.example"},
        {"root": ""},
        {"allow": True},
        "native",
    ]:
        with pytest.raises(Alpha2HostError) as caught:
            HostClient(host_path=host_path, plugins=refused)  # type: ignore[arg-type]
        assert caught.value.code == "INVALID_INPUT"
        assert caught.value.message.startswith("plugins"), caught.value.message
    assert lines_in(starts) == []


def test_the_client_refuses_every_invalid_export_request_of_the_corpus_before_a_host_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    client = HostClient(host_path=fake_host(tmp_path, started_host(starts)))
    cases = [
        fixture
        for fixture in corpus("host-protocol-corpus.json")
        if not fixture["requestValid"] and fixture["request"]["operation"] == "export"
    ]
    assert len(cases) >= 8
    for fixture in cases:
        # Replace the placeholder project so each case tests only its own field.
        fields = {
            key: value
            for key, value in fixture["request"].items()
            if key not in ("protocolVersion", "operation", "requestId", "project")
        }
        arguments = wire_to_python(fields)
        with pytest.raises(Alpha2HostError) as caught:
            client.export(project_document(), **arguments)
        assert fixture["expectedErrorCode"] in ("INVALID_INPUT", "INVALID_REQUEST"), fixture["name"]
        assert caught.value.code == "INVALID_INPUT", (fixture["name"], caught.value.message)
        assert caught.value.failure["code"] == "INVALID_INPUT"
    assert lines_in(starts) == []


def test_media_resource_and_font_fields_not_shaped_as_the_host_reads_them_are_refused(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    client = HostClient(host_path=fake_host(tmp_path, started_host(starts)))
    refusals: list[tuple[dict[str, Any], str]] = [
        ({"fonts": "/staging/fonts/Inter.ttf"}, "fonts"),
        ({"fonts": 5}, "fonts"),
        ({"fonts": [""]}, "fonts"),
        ({"assets": ["/a.mov"]}, "assets"),
        ({"frames": {"seq": "/a.png"}}, "frames for source seq"),
        ({"frames": {"seq": ["/a.png", 3]}}, "frames for source seq"),
        ({"frames": []}, "frames must map"),
        ({"proxies": {"a": "/a.proxy.mov"}}, "proxies for source a"),
        ({"proxies": {"a": [{"path": ""}]}}, "proxies for source a"),
        ({"resources": {"label": "/badge.png"}}, r"resources\.label"),
        ({"effect_assets": {"lut": 7}}, "effect asset path for lut"),
        ({"effects": {}}, "effects"),
        ({"media": "draft"}, "media"),
        ({"assets": {"clip-a": ""}}, "clip-a"),
        ({"start_ticks": 5, "end_ticks": 5}, "range"),
        ({"frame_rate": {"num": 0, "den": 1}}, "frame_rate"),
        ({"output": ""}, "output"),
        ({"video_encoder": ""}, "video_encoder"),
        ({"strategy": "copy"}, "strategy"),
        ({"input_policy": "gpu"}, "input_policy"),
        ({"render_cache": {"directory": ""}}, "render_cache"),
    ]
    for fields, message in refusals:
        with pytest.raises(Alpha2HostError) as caught:
            client.export(**export_args(tmp_path / "never.mp4", **fields))
        assert type(caught.value) is Alpha2HostError, fields
        assert caught.value.code == "INVALID_INPUT", caught.value.message
        assert re.search(message, caught.value.message), (fields, caught.value.message)
    for fields in ({"assets": []}, {"frames": {"seq": "/a.png"}}, {"proxies": {"a": [{}]}}):
        with pytest.raises(Alpha2HostError) as caught:
            client.conform(project_document(), **fields)
        assert caught.value.code == "INVALID_INPUT", fields
    client.export(
        **export_args(
            tmp_path / "gap.mp4",
            frames={"seq": ["/a.png", None, Path("/b.png")]},
            proxies={"a": []},
            assets={"x": Path("/x.mov")},
        )
    )
    assert lines_in(starts) == ["unset"]


def test_an_invalid_quality_is_refused_before_a_host_starts_naming_the_field(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    client = HostClient(host_path=fake_host(tmp_path, started_host(starts)))
    for field, value, needle in [
        ("video_quality", "ultra", "video_quality"),
        ("video_quality", {"bitrate": 0}, "video_quality.bitrate"),
        ("video_quality", {"bitrate": 1.5}, "video_quality.bitrate"),
        ("video_quality", {"bitrate": 8_000_000, "mode": "cbr"}, "video_quality.mode"),
        ("video_quality", {"bitrate": 8_000_000, "crf": 23}, "video_quality"),
        ("video_quality", {"bitrate": 8_000_000, "quantizer": 256}, "video_quality.quantizer"),
        ("audio_quality", "loud", "audio_quality"),
        ("audio_quality", {"bitrate": 128_000, "mode": "constant"}, "audio_quality"),
        ("loudness", {"target": "loud"}, "loudness"),
        ("loudness", {"target_lufs": 1, "true_peak_ceiling_dbtp": -1}, "loudness.target_lufs"),
        ("memory", {"in_flight_bytes": 0}, "memory.in_flight_bytes"),
        ("memory", {"gpu_bytes": 1}, "gpu_bytes"),
        ("memory", {"scale": 2}, "memory.scale"),
    ]:
        with pytest.raises(Alpha2HostError) as caught:
            client.export(**export_args(tmp_path / "never.mp4", **{field: value}))
        assert caught.value.code == "INVALID_INPUT"
        assert needle in caught.value.message, (field, value, caught.value.message)
    assert lines_in(starts) == []


def test_options_written_in_python_reach_the_host_in_the_protocols_names(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    recorded = tmp_path / "request.json"
    host_path = fake_host(
        tmp_path,
        f"""
        def answer(request):
            open({str(recorded)!r}, "w").write(json.dumps(request))
            send(request, ok=True, result={{"output": request["output"]}})
        serve(answer)
        """,
    )
    resources = {"label": {"art/badge.png": "/media/badge.png"}}
    alpha2.export(
        **export_args(
            tmp_path / "with.mp4",
            resources=resources,
            memory={"in_flight_bytes": 1 << 20, "scale": 0.5},
            loudness={"target_lufs": -14, "true_peak_ceiling_dbtp": -1},
            render_cache={"directory": tmp_path / "cache", "quota_bytes": 1 << 30},
            proxies={"a": [{"path": "/a.proxy.mov", "color_pipeline": "tone_map"}]},
            video_quality={"bitrate": 8_000_000, "mode": "constant", "quantizer": 23},
        ),
        host_path=host_path,
    )
    import json

    request = json.loads(recorded.read_text())
    # A path without a drive is resolved on Windows; on POSIX it is already absolute.
    assert request["resources"] == {"label": {"art/badge.png": os.path.abspath("/media/badge.png")}}
    assert request["memory"] == {"inFlightBytes": 1 << 20, "scale": 0.5}
    assert request["loudness"] == {"targetLufs": -14, "truePeakCeilingDbtp": -1}
    assert request["renderCache"] == {"directory": str(tmp_path / "cache"), "quotaBytes": 1 << 30}
    assert request["proxies"] == {
        "a": [{"path": os.path.abspath("/a.proxy.mov"), "colorPipeline": "tone_map"}]
    }
    assert request["videoQuality"] == {"bitrate": 8_000_000, "mode": "constant", "quantizer": 23}
    assert request["frameRate"] == {"num": 30, "den": 1}
    assert request["inputPolicy"] == "auto" and request["strategy"] == "render"
    for absent in ("container", "videoCodec", "audioCodec", "videoEncoder", "audioQuality"):
        assert absent not in request
    alpha2.export(**export_args(tmp_path / "without.mp4"), host_path=host_path)
    assert json.loads(recorded.read_text())["resources"] == {}
    # The host takes -1 dBTP for a loudness without a ceiling.
    alpha2.export(
        **export_args(tmp_path / "loud.mp4", loudness={"target_lufs": -16}), host_path=host_path
    )
    assert json.loads(recorded.read_text())["loudness"] == {"targetLufs": -16}


def test_an_export_defaults_to_the_whole_project_at_its_own_rate(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    recorded = tmp_path / "request.json"
    host_path = fake_host(
        tmp_path,
        f"""
        request, line = read_request()
        open({str(recorded)!r}, "w").write(line)
        send(request, ok=True, result={{}})
        """,
    )
    builder = alpha2.ProjectBuilder("Defaults", width=64, height=64, frame_rate=(30_000, 1_001))
    image = builder.source("image", "a.png")
    builder.video_track().clip(image, at=0.5, duration=2)
    project = builder.build()
    HostClient(host_path=host_path).export(project, tmp_path / "out.mp4", assets={image: "a.png"})
    import json

    request = json.loads(recorded.read_text())
    assert (request["startTicks"], request["endTicks"]) == (0, 300_000)
    assert request["frameRate"] == {"num": 30_000, "den": 1_001}
    with pytest.raises(Alpha2HostError, match="end_ticks is required"):
        HostClient(host_path=host_path).export(project_document(), tmp_path / "empty.mp4")


def test_a_render_kept_after_a_failed_publish_is_named_on_the_error(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    kept = str(tmp_path / "out.a2kept.mp4")
    host_path = fake_host(
        tmp_path,
        f"""
        request, line = read_request()
        send(request, ok=False, error={{
            "code": "OUTPUT_PUBLISH_FAILED",
            "message": "cannot publish the output: denied; the finished render is kept at " + {kept!r},
            "details": {{"keptPath": {kept!r}}},
            "failure": {{"code": "OUTPUT_IO_FAILED", "origin": "native-host", "operation": "export",
                         "backend": "native", "phase": "mux", "resource": "unknown"}},
        }})
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        alpha2.export(**export_args(tmp_path / "out.mp4"), host_path=host_path)
    assert caught.value.code == "OUTPUT_PUBLISH_FAILED"
    assert caught.value.kept_path == kept
    assert caught.value.failure["code"] == "OUTPUT_IO_FAILED"


def test_binary_resolution_order_and_its_not_found_message(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    executable = tmp_path / "bin" / "alpha2-host"
    executable.parent.mkdir()
    executable.write_text("#!/bin/sh\n")
    executable.chmod(0o755)
    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", str(tmp_path / "empty-cache"))
    monkeypatch.setenv("PATH", str(tmp_path / "nowhere"))
    monkeypatch.setenv("ALPHA2_HOST_PATH", str(executable))
    assert resolve_host_binary() == executable
    monkeypatch.setenv("ALPHA2_HOST_PATH", "  ")
    with pytest.raises(Alpha2HostError) as caught:
        resolve_host_binary()
    assert caught.value.code == "HOST_NOT_FOUND"
    assert "python -m alpha2 install" in caught.value.message
    assert "github.com/alpha2-sh/alpha2/releases" in caught.value.message
    assert caught.value.failure["code"] == "INVALID_INPUT"
    monkeypatch.setenv("PATH", str(executable.parent))
    if sys.platform != "win32":
        assert resolve_host_binary() == executable
    with pytest.raises(Alpha2HostError) as caught:
        resolve_host_binary(tmp_path / "missing")
    assert caught.value.code == "HOST_NOT_FOUND"
    with pytest.raises(Alpha2HostError) as caught:
        resolve_host_binary("")
    assert caught.value.code == "INVALID_INPUT"
    version = alpha2.__version__
    target = alpha2.host_target()
    if target is not None:
        cache = tmp_path / "cache"
        binary = cache / f"alpha2-{version}-{target}-bundled" / "bin" / host_executable_name()
        binary.parent.mkdir(parents=True)
        binary.write_text("")
        assert resolve_host_binary(cache_directory=cache) == binary


def test_a_file_that_cannot_be_executed_is_refused_before_it_would_start(tmp_path: Path) -> None:
    if sys.platform == "win32":
        # Windows runs a batch file only through a shell and has no execute bit.
        refused = tmp_path / "alpha2-host.cmd"
        refused.write_text("@echo off\n")
    else:
        refused = tmp_path / "not-executable"
        refused.write_text("#!/bin/sh\n")
        refused.chmod(0o644)
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=refused).export(**export_args(tmp_path / "never.mp4"))
    assert caught.value.code == "HOST_NOT_EXECUTABLE"
    assert caught.value.failure["code"] == "INVALID_INPUT"
    if sys.platform == "win32":
        text = tmp_path / "alpha2-host.exe"
        text.write_text("not a program\n")
        with pytest.raises(Alpha2HostError) as caught:
            HostClient(host_path=text).export(**export_args(tmp_path / "never.mp4"))
        assert caught.value.code == "HOST_START_FAILED"


def test_cancellation_returns_only_after_the_host_process_exits(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    exit_marker = tmp_path / "exited"
    pid_file = tmp_path / "host.pid"
    host_path = fake_host(
        tmp_path,
        f"""
        open({str(pid_file)!r}, "w").write(str(os.getpid()))
        def leave(*_):
            time.sleep(0.1)
            open({str(exit_marker)!r}, "w").write("exited")
            sys.exit(0)
        signal.signal(signal.SIGTERM, leave)
        request, _ = read_request()
        send(request, type="progress", completed=0, total=1)
        while True:
            time.sleep(1)
        """,
    )
    cancel = threading.Event()
    with pytest.raises(Alpha2AbortError) as caught:
        HostClient(host_path=host_path).export(
            **export_args(tmp_path / "out.mp4", cancel=cancel, on_progress=lambda _: cancel.set())
        )
    assert caught.value.code == "ABORTED"
    assert caught.value.failure["code"] == "CANCELLED"
    assert pid_exited(int(pid_file.read_text()))
    assert len(started_pids) == 1 and pid_exited(started_pids[0])
    # Windows ends the host with TerminateProcess, so no handler of its own runs there.
    if sys.platform != "win32":
        assert exit_marker.read_text() == "exited"


def test_ctrl_c_ends_the_host_before_the_interrupt_goes_on(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    pid_file = tmp_path / "host.pid"
    host_path = fake_host(
        tmp_path,
        f"""
        open({str(pid_file)!r}, "w").write(str(os.getpid()))
        request, _ = read_request()
        send(request, type="progress", phase="frames", completed=0, total=9)
        while True:
            time.sleep(1)
        """,
    )

    def interrupt(_: object) -> None:
        raise KeyboardInterrupt

    with pytest.raises(KeyboardInterrupt):
        HostClient(host_path=host_path).export(
            **export_args(tmp_path / "out.mp4", on_progress=interrupt)
        )
    assert pid_exited(int(pid_file.read_text()))
    assert len(started_pids) == 1 and pid_exited(started_pids[0])


def queue_host(log: Path, answer_seconds: float = 0, ignore_term: bool = False) -> str:
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()))
    if {ignore_term}:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    def answer(request):
        record({str(log)!r}, "request " + os.path.basename(request["output"]))
        send(request, type="progress", phase="frames", completed=0, total=1)
        time.sleep({answer_seconds})
        send(request, ok=True, result={{"output": request["output"]}})
    serve(answer)
    """


def test_an_export_cancelled_while_it_waits_for_a_slot_never_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    # The first export outlasts the cancel by seconds, so a slow runner cannot reorder them.
    client = HostClient(host_path=fake_host(tmp_path, queue_host(log, answer_seconds=3)))
    cancel = threading.Event()
    with ThreadPoolExecutor(3) as pool:
        first = pool.submit(client.export, **export_args("a.mp4"))
        time.sleep(0.1)
        second = pool.submit(client.export, **export_args("b.mp4", cancel=cancel))
        time.sleep(0.02)
        third = pool.submit(client.export, **export_args("c.mp4"))
        time.sleep(0.05)
        cancel.set()
        with pytest.raises(Alpha2AbortError):
            second.result()
        assert not first.done(), "the cancel waited for the slot"
        assert first.result() == {"output": str(Path.cwd() / "a.mp4")}
        assert third.result() == {"output": str(Path.cwd() / "c.mp4")}
    requests = [line for line in lines_in(log) if line.startswith("request")]
    assert requests == ["request a.mp4", "request c.mp4"]


def test_a_host_that_stays_up_through_sigterm_is_killed(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    """POSIX waits out the cancel timeout, then SIGTERM, then kills. Windows uses
    TerminateProcess, which cannot be ignored."""
    log = tmp_path / "log"
    client = HostClient(
        host_path=fake_host(tmp_path, queue_host(log, answer_seconds=60, ignore_term=True))
    )
    cancel = threading.Event()
    cancelled: list[float] = []

    def progress(_: object) -> None:
        cancelled.append(time.monotonic())
        cancel.set()

    with pytest.raises(Alpha2AbortError):
        client.export(**export_args("out.mp4", cancel=cancel, on_progress=progress))
    waited = time.monotonic() - cancelled[0]
    if sys.platform == "win32":
        assert 4.9 <= waited < 5 + 5, f"returned {waited:.2f} s after the cancel"
    else:
        assert 9.9 <= waited < 10 + 5, f"returned {waited:.2f} s after the cancel"
    assert pid_exited(int(lines_in(log)[0].split(" ")[1]))
    assert len(started_pids) == 1 and pid_exited(started_pids[0])


def test_a_cancel_while_the_host_starts_ends_it_before_the_request_is_written(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    source = "time.sleep(0.5)\nbegin()\nstate()\n" + textwrap.dedent(queue_host(log))
    host_path = fake_host(tmp_path, source, ready=False)
    cancel = threading.Event()
    threading.Timer(0.2, cancel.set).start()
    with pytest.raises(Alpha2AbortError):
        HostClient(host_path=host_path).export(**export_args("out.mp4", cancel=cancel))
    assert len(started_pids) == 1
    wait_until(lambda: pid_exited(started_pids[0]))
    assert [line for line in lines_in(log) if line.startswith("request")] == []


def test_module_calls_with_the_same_host_and_concurrency_share_one_pool(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    lock = tmp_path / "host.lock"
    overlap = tmp_path / "overlap"
    modes = tmp_path / "modes"
    host_path = fake_host(
        tmp_path,
        f"""
        try:
            descriptor = os.open({str(lock)!r}, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.close(descriptor)
        except FileExistsError:
            open({str(overlap)!r}, "w").write("overlap")
        def answer(request):
            record({str(modes)!r}, os.environ.get("ALPHA2_VIDEO_DECODE", "unset"))
            time.sleep(0.1)
            send(request, ok=True, result={{"output": request.get("output"), "status": "ready"}})
        serve(answer)
        os.remove({str(lock)!r})
        """,
    )
    with ThreadPoolExecutor(5) as pool:
        calls = [
            pool.submit(alpha2.export, **export_args(tmp_path / "1.mp4"), host_path=host_path),
            pool.submit(alpha2.export, **export_args(tmp_path / "2.mp4"), host_path=host_path),
            pool.submit(
                alpha2.export,
                **export_args(tmp_path / "3.mp4", video_decode="software"),
                host_path=host_path,
            ),
            pool.submit(
                alpha2.export,
                **export_args(tmp_path / "4.mp4", video_decode="auto"),
                host_path=host_path,
            ),
            pool.submit(alpha2.capabilities, host_path=host_path, video_decode="hardware-required"),
        ]
        for call in calls:
            call.result()
    assert not overlap.exists()
    assert sorted(lines_in(modes)) == ["auto", "hardware-required", "software", "unset", "unset"]


def test_a_failed_export_keeps_the_hosts_failure_report_and_its_stderr(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        sys.stderr.write("x" * 40_000 + "\\nalpha2-host uncaptured GPU out-of-memory error: staging\\n")
        sys.stderr.flush()
        send(request, type="progress", phase="frames", completed=3, total=9)
        send(request, ok=False, error={
            "code": "ALPHA2_ERROR",
            "message": "GPU texture readback failed: map-failed",
            "failure": {"code": "OUT_OF_MEMORY", "origin": "native-host", "operation": "export",
                        "backend": "native", "phase": "readback", "stage": "map-failed",
                        "frameIndex": 4, "resource": "gpu",
                        "diagnostics": {"videoPipeline": "readback"}},
        })
        """,
    )
    phases: list[str] = []
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(
            **export_args(tmp_path / "out.mp4", on_progress=lambda p: phases.append(p.phase))
        )
    error = caught.value
    assert error.code == "ALPHA2_ERROR"
    assert error.failure["code"] == "OUT_OF_MEMORY"
    assert error.failure["phase"] == "readback"
    assert error.failure["frameIndex"] == 4
    assert error.failure["message"] == "GPU texture readback failed: map-failed"
    assert error.failure["diagnostics"]["videoPipeline"] == "readback"
    assert re.search("out-of-memory error: staging", error.failure["stderr"])
    assert error.stderr is not None and len(error.stderr.encode()) <= 16_384
    assert phases == ["frames"]


def test_the_stderr_an_error_keeps_is_at_most_16_kib_of_utf8_from_a_whole_character(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    line = "오류: /미디어/서울/촬영본.mov 를 읽을 수 없습니다 😀\n"
    host_path = fake_host(
        tmp_path,
        f"""
        request, _ = read_request()
        sys.stderr.write({line!r} * 2000 + "the last line\\n")
        sys.stderr.flush()
        send(request, ok=False, error={{"code": "ALPHA2_ERROR", "message": "failed"}})
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    for stderr in (caught.value.stderr, caught.value.failure["stderr"]):
        assert stderr is not None
        size = len(stderr.encode())
        assert 16_300 < size <= 16_384, size
        assert "�" not in stderr
        assert stderr.endswith("the last line\n")


def test_stderr_written_up_to_the_answer_is_kept_while_the_host_stays_up(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    # More than a pipe buffer, so the client is still reading when the answer arrives.
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        sys.stderr.write("x" * 40_000 + "\\n" + "the last line before the answer\\n")
        sys.stderr.flush()
        send(request, ok=False, error={"code": "ALPHA2_ERROR", "message": "readback failed"})
        read_request()
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "ALPHA2_ERROR"
    for stderr in (caught.value.stderr, caught.value.failure["stderr"]):
        assert stderr is not None and stderr.endswith("the last line before the answer\n")


def test_an_error_keeps_only_the_stderr_its_own_request_wrote(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        def answer(request):
            sys.stderr.write("while " + os.path.basename(request["output"]) + "\\n")
            sys.stderr.flush()
            send(request, ok=False, error={"code": "ALPHA2_ERROR", "message": "failed"})
        serve(answer)
        """,
    )
    client = HostClient(host_path=host_path)
    for name in ("first.mp4", "second.mp4"):
        with pytest.raises(Alpha2HostError) as caught:
            client.export(**export_args(name))
        assert caught.value.stderr == f"while {name}\n"


def test_what_a_host_prints_before_its_magic_is_kept_as_diagnostic_output(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    banner = "Validation Error: [ VUID-StandaloneSpirv-None-10684 ] vkCreateShaderModule"
    host_path = fake_host(
        tmp_path,
        f"""
        sys.stdout.write({banner!r} + "\\n")
        begin()
        state()
        request, _ = read_request()
        send(request, ok=False, error={{"code": "ALPHA2_ERROR", "message": "failed"}})
        """,
        ready=False,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "ALPHA2_ERROR"
    assert caught.value.stderr is not None and banner in caught.value.stderr


def test_a_host_that_never_writes_its_magic_fails_to_start_with_invalid_response(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    # One byte past the client's bound on what may come before the magic.
    host_path = fake_host(
        tmp_path,
        """
        sys.stdout.write("x" * (1 << 20) + "y" * 9)
        sys.stdout.flush()
        read_request()
        """,
        ready=False,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).capabilities()
    assert caught.value.code == "INVALID_RESPONSE"
    assert "without its protocol magic" in caught.value.message


def test_a_frame_the_client_cannot_read_is_invalid_response(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    bodies = {
        "json": 'b"{not json"',
        # The header lists 4 bytes of attachments; the frame carries 3.
        "attachments": 'json.dumps({"protocolVersion": 2, "requestId": request["requestId"], "ok": True, '
        '"result": {}, "hostState": "ready", "attachments": [{"type": "image/png", "bytes": 4}]}).encode()',
        "list": 'json.dumps({"protocolVersion": 2, "requestId": request["requestId"], "ok": True, '
        '"result": {}, "hostState": "ready", "attachments": [{"type": "", "bytes": 3}]}).encode()',
    }
    for name, body in bodies.items():
        host_path = fake_host(
            tmp_path / name,
            f"""
            request, _ = read_request()
            header = {body}
            sys.stdout.buffer.write(struct.pack("<II", len(header), 3) + header + b"PNG")
            sys.stdout.buffer.flush()
            read_request()
            """,
        )
        with pytest.raises(Alpha2HostError) as caught:
            HostClient(host_path=host_path).capabilities()
        assert caught.value.code == "INVALID_RESPONSE", name


def test_a_still_without_an_output_returns_its_png(tmp_path: Path, fake_host: FakeHost) -> None:
    recorded = tmp_path / "request.json"
    host_path = fake_host(
        tmp_path,
        f"""
        request, header = read_request()
        open({str(recorded)!r}, "w").write(header)
        frames, attachments = [], []
        for index, still in enumerate(request["stills"]):
            frame = {{"frameTime": still["time"], "width": 1, "height": 1}}
            if "output" in still:
                frames.append({{**frame, "output": still["output"]}})
            else:
                frames.append({{**frame, "output": None, "attachment": len(attachments)}})
                attachments.append(("image/png", b"PNG-" + str(index).encode()))
        write_message({{"requestId": request["requestId"], "ok": True, "hostState": "ready",
            "result": {{"stills": frames, "renderer": "cpu", "timings": {{}}}}}}, attachments)
        read_request()
        """,
    )
    result = HostClient(host_path=host_path).stills(
        project_document(), [(0, None), (0.5, tmp_path / "b.png"), (1, None)]
    )
    request = json.loads(recorded.read_text())
    assert request["stills"] == [
        {"time": 0},
        {"time": 60_000, "output": str(tmp_path / "b.png")},
        {"time": 120_000},
    ]
    frames = result["stills"]
    assert [frame.get("png") for frame in frames] == [b"PNG-0", None, b"PNG-2"]
    assert [frame["output"] for frame in frames] == [None, str(tmp_path / "b.png"), None]


def test_a_still_naming_an_attachment_the_response_lacks_is_invalid_response(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        frame = {"frameTime": 0, "width": 1, "height": 1, "output": None, "attachment": 1}
        write_message({"requestId": request["requestId"], "ok": True, "hostState": "ready",
            "result": {"stills": [frame], "renderer": "cpu", "timings": {}}}, [("image/png", b"PNG")])
        read_request()
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).still(project_document(), 0)
    assert caught.value.code == "INVALID_RESPONSE"


DEATHS = [pytest.param("os._exit(3)", 3, None, id="exit-code")]
"""How a fake host ends abruptly: with an exit code everywhere, by SIGKILL where there are signals."""
if sys.platform != "win32":
    DEATHS.append(
        pytest.param("os.kill(os.getpid(), signal.SIGKILL)", None, "SIGKILL", id="sigkill")
    )


@pytest.mark.parametrize(("ending", "exit_code", "signal_name"), DEATHS)
def test_a_host_that_dies_mid_export_is_host_exited_with_no_cause_claimed(
    tmp_path: Path,
    fake_host: FakeHost,
    ending: str,
    exit_code: int | None,
    signal_name: str | None,
) -> None:
    host_path = fake_host(
        tmp_path,
        f"""
        request, _ = read_request()
        send(request, type="progress", phase="finalizing", completed=9, total=9)
        sys.stderr.write("last words\\n")
        sys.stderr.flush()
        {ending}
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    error = caught.value
    assert error.code == "HOST_EXITED"
    assert error.signal == signal_name
    assert error.exit_code == exit_code
    assert error.failure["code"] == "HOST_EXITED"
    assert error.failure["resource"] == "unknown"
    assert error.failure["phase"] == "mux"
    assert error.failure["origin"] == "python"
    assert error.failure["signal"] == signal_name
    assert error.failure["exitCode"] == exit_code
    assert "last words" in error.failure["stderr"]


@pytest.mark.skipif(sys.platform == "win32", reason="Windows has no SIGKILL")
def test_oom_killed_host_is_out_of_memory(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    """SIGKILL with a rising ``oom_kill`` count is ``HOST_OOM_KILLED``; without a readable rise it
    is ``HOST_EXITED``."""
    events = tmp_path / "memory.events"
    events.write_text("low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\noom_group_kill 0\n")
    monkeypatch.setenv("ALPHA2_TEST_MEMORY_EVENTS", str(events))
    host_path = fake_host(
        tmp_path,
        """
        import os, pathlib
        request, _ = read_request()
        send(request, type="progress", phase="frames", completed=3, total=9)
        if os.environ.get("RAISE_OOM_KILL") == "1":
            path = pathlib.Path(os.environ["ALPHA2_TEST_MEMORY_EVENTS"])
            path.write_text(path.read_text().replace("oom_kill 1", "oom_kill 2"))
        os.kill(os.getpid(), signal.SIGKILL)
        """,
    )
    monkeypatch.setenv("RAISE_OOM_KILL", "1")
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    error = caught.value
    assert error.code == "HOST_OOM_KILLED"
    assert error.signal == "SIGKILL"
    assert error.failure["code"] == "OUT_OF_MEMORY"
    assert error.failure["resource"] == "cpu"
    assert error.failure["origin"] == "python"
    assert failure_class_of_code("HOST_OOM_KILLED") == ("OUT_OF_MEMORY", "cpu")
    monkeypatch.setenv("RAISE_OOM_KILL", "0")
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "again.mp4"))
    assert caught.value.code == "HOST_EXITED"
    monkeypatch.setenv("ALPHA2_TEST_MEMORY_EVENTS", str(tmp_path / "missing.events"))
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "unread.mp4"))
    assert caught.value.code == "HOST_EXITED"


def test_a_host_the_loader_refuses_to_start_is_host_load_failed(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    words = (
        "/opt/alpha2/bin/alpha2-host: error while loading shared libraries: libavcodec.so.62: "
        "cannot open shared object file: No such file or directory"
    )
    client = HostClient(
        host_path=fake_host(
            tmp_path, f"sys.stderr.write({words + chr(10)!r})\nsys.exit(127)\n", ready=False
        )
    )
    calls: list[Callable[[], object]] = [
        lambda: client.export(**export_args(tmp_path / "out.mp4")),
        client.capabilities,
    ]
    for call in calls:
        with pytest.raises(Alpha2HostError) as caught:
            call()
        assert caught.value.code == "HOST_LOAD_FAILED"
        assert "libavcodec.so.62" in caught.value.message
        assert caught.value.exit_code == 127
        assert caught.value.failure["code"] == "INVALID_INPUT"
        assert caught.value.failure["phase"] == "prepare"
        assert caught.value.failure["exitCode"] == 127


def test_a_windows_crash_status_is_named(tmp_path: Path, fake_host: FakeHost) -> None:
    named = "exit 3221226505 (0xC0000409 STATUS_STACK_BUFFER_OVERRUN)"
    assert describe_exit(0xC0000409, None) == named
    assert (
        describe_exit(-1073740791, None)
        == "exit -1073740791 (0xC0000409 STATUS_STACK_BUFFER_OVERRUN)"
    )
    assert describe_exit(1, None) == "exit 1"
    assert describe_exit(None, "SIGKILL") == "exit SIGKILL"
    if sys.platform != "win32":
        return
    # The signed form of 0xC0000409, which os._exit takes as a C int.
    host_path = fake_host(tmp_path, "read_request()\nos._exit(-1073740791)\n")
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "HOST_EXITED"
    assert caught.value.exit_code == 0xC0000409
    assert named in caught.value.message
    assert caught.value.failure["ntstatus"] == "STATUS_STACK_BUFFER_OVERRUN"


def stalling_host(runs: Path, resource: str = "gpu") -> str:
    return f"""
    request, _ = read_request()
    mode = os.environ.get("ALPHA2_VIDEO_DECODE", "unset")
    record({str(runs)!r}, mode)
    if mode == "software":
        send(request, ok=True, result={{"frames": 9}})
    else:
        send(request, ok=False, error={{
            "code": "DECODE_STALLED",
            "message": "decode shard 0 has been inside one decoder operation for 40001 ms",
            "failure": {{"code": "GPU_TIMEOUT", "origin": "native-host", "operation": "export",
                         "backend": "native", "phase": "decode", "resource": {resource!r}}},
        }})
    """


def test_an_auto_export_whose_hardware_decoder_stalls_runs_again_in_software(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    runs = tmp_path / "runs"
    host_path = fake_host(tmp_path, stalling_host(runs))
    result = HostClient(host_path=host_path, video_decode="auto").export(
        **export_args(tmp_path / "out.mp4")
    )
    assert result == {"frames": 9}
    assert lines_in(runs) == ["auto", "software"]
    # With no mode the host picks its platform's default, so the client cannot tell.
    monkeypatch.delenv("ALPHA2_VIDEO_DECODE", raising=False)
    runs.unlink()
    HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    assert lines_in(runs) == ["unset", "software"]


def test_progress_marks_where_an_export_started_over(tmp_path: Path, fake_host: FakeHost) -> None:
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        if os.environ.get("ALPHA2_VIDEO_DECODE") == "software":
            send(request, type="progress", phase="frames", completed=0, total=9)
            send(request, type="progress", phase="frames", completed=9, total=9)
            send(request, ok=True, result={"frames": 9})
        else:
            send(request, type="progress", phase="frames", completed=4, total=9)
            # The host's own restart on the readback path.
            send(request, type="progress", phase="frames", completed=0, total=9, restarted=True)
            send(request, type="progress", phase="frames", completed=2, total=9)
            send(request, ok=False, error={"code": "DECODE_STALLED",
                "message": "decode shard 0 has been inside one decoder operation for 40001 ms",
                "failure": {"code": "GPU_TIMEOUT", "origin": "native-host", "operation": "export",
                            "backend": "native", "phase": "decode", "resource": "gpu"}})
        """,
    )
    reports: list[tuple[int, bool]] = []
    HostClient(host_path=host_path, video_decode="auto").export(
        **export_args(
            tmp_path / "out.mp4", on_progress=lambda p: reports.append((p.completed, p.restarted))
        )
    )
    # The software rerun's first report is marked by the client.
    assert reports == [(4, False), (0, True), (2, False), (0, True), (9, False)]


def test_a_stall_is_not_retried_with_only_software_sessions_or_hardware_required(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    for name, resource, mode in (
        ("software", "unknown", "auto"),
        ("required", "gpu", "hardware-required"),
    ):
        runs = tmp_path / name / "runs"
        host_path = fake_host(tmp_path / name, stalling_host(runs, resource))
        with pytest.raises(Alpha2HostError) as caught:
            HostClient(host_path=host_path, video_decode=mode).export(  # type: ignore[arg-type]
                **export_args(tmp_path / "out.mp4")
            )
        assert caught.value.code == "DECODE_STALLED"
        assert caught.value.failure["code"] == "GPU_TIMEOUT"
        assert lines_in(runs) == [mode]


def test_a_software_run_after_a_stall_that_fails_too_keeps_the_stall(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        if os.environ.get("ALPHA2_VIDEO_DECODE") == "software":
            sys.stderr.write("second host\\n"); sys.stderr.flush()
            send(request, ok=False, error={"code": "RESOURCE_LIMIT", "message": "decode queue budget too small",
                "failure": {"code": "RESOURCE_LIMIT", "origin": "native-host", "operation": "export",
                            "backend": "native", "phase": "decode", "resource": "cpu",
                            "diagnostics": {"videoPipeline": "readback"}}})
        else:
            sys.stderr.write("first host: decoder stuck\\n"); sys.stderr.flush()
            send(request, ok=False, error={"code": "DECODE_STALLED",
                "message": "decode shard 0 has been inside one decoder operation for 40001 ms",
                "failure": {"code": "GPU_TIMEOUT", "origin": "native-host", "operation": "export",
                            "backend": "native", "phase": "decode", "resource": "gpu"}})
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path, video_decode="auto").export(
            **export_args(tmp_path / "out.mp4")
        )
    error = caught.value
    assert error.code == "RESOURCE_LIMIT"
    assert error.failure["code"] == "RESOURCE_LIMIT"
    assert error.stderr is not None and "second host" in error.stderr
    assert error.failure["diagnostics"]["videoPipeline"] == "readback"
    assert isinstance(error.__cause__, Alpha2HostError)
    assert error.__cause__.code == "DECODE_STALLED"
    stall = error.failure["diagnostics"]["decodeStall"]
    assert stall["hostCode"] == "DECODE_STALLED"
    assert stall["code"] == "GPU_TIMEOUT"
    assert stall["phase"] == "decode"
    assert "40001 ms" in stall["message"]
    assert "decoder stuck" in stall["stderr"]


def test_an_encoder_stall_is_reported_not_run_again_in_software(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    runs = tmp_path / "runs"
    host_path = fake_host(
        tmp_path,
        f"""
        request, _ = read_request()
        record({str(runs)!r}, os.environ.get("ALPHA2_VIDEO_DECODE", "unset"))
        send(request, ok=False, error={{"code": "ENCODE_STALLED", "message": "encoder stuck",
            "failure": {{"code": "GPU_TIMEOUT", "origin": "native-host", "operation": "export",
                         "backend": "native", "phase": "encode", "resource": "gpu"}}}})
        sys.exit(1)
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path, video_decode="auto").export(
            **export_args(tmp_path / "out.mp4")
        )
    assert caught.value.code == "ENCODE_STALLED"
    assert caught.value.failure["resource"] == "gpu"
    assert lines_in(runs) == ["auto"]


def test_a_decode_mode_that_is_not_one_is_refused_before_a_host_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    host_path = fake_host(tmp_path, started_host(starts))
    calls: list[Callable[[], object]] = [
        lambda: HostClient(host_path=host_path, video_decode="hardware"),  # type: ignore[arg-type]
        lambda: alpha2.export(
            **export_args(tmp_path / "m.mp4", video_decode="gpu"), host_path=host_path
        ),
        lambda: alpha2.capabilities(host_path=host_path, video_decode="gpu"),  # type: ignore[arg-type]
    ]
    for call in calls:
        with pytest.raises(Alpha2HostError) as caught:
            call()
        assert caught.value.code == "INVALID_INPUT"
        assert "video_decode must be software, auto, or hardware-required" in caught.value.message
    assert lines_in(starts) == []
    with pytest.raises(ValueError):
        HostClient(host_path=host_path, concurrency=0)


def probed_host(log: Path, export_seconds: float = 0, fail_first: bool = False) -> str:
    return f"""
    def answer(request):
        mode = os.environ.get("ALPHA2_VIDEO_DECODE", "unset")
        record({str(log)!r}, request["operation"] + " " + mode)
        if request["operation"] == "export":
            time.sleep({export_seconds})
            send(request, ok=True, result={{"output": request["output"]}})
            return
        failed = {str(log)!r} + ".failed"
        if {fail_first} and not os.path.exists(failed):
            open(failed, "w").close()
            send(request, ok=False, error={{"code": "ALPHA2_ERROR", "message": "probe failed"}})
        else:
            send(request, ok=True, result={{"status": "ready", "capabilities": {{"decode": {{"mode": mode}}}}, "effectCatalog": {{}}}})
    serve(answer)
    """


def test_a_capability_probe_withdrawn_while_an_export_holds_the_slot_never_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "requests"
    client = HostClient(host_path=fake_host(tmp_path, probed_host(log, export_seconds=0.6)))
    cancel = threading.Event()
    with ThreadPoolExecutor(2) as pool:
        exporting = pool.submit(client.export, **export_args(tmp_path / "out.mp4"))
        time.sleep(0.1)
        probe = pool.submit(client.capabilities, cancel=cancel)
        time.sleep(0.05)
        cancel.set()
        with pytest.raises(Alpha2AbortError):
            probe.result()
        assert not exporting.done(), "the withdrawn call waited for the export"
        exporting.result()
    # Waiting requests are served in order, so a probe still queued would have run before this.
    client.export(**export_args(tmp_path / "after.mp4"))
    assert lines_in(log) == ["export unset", "export unset"]


def test_one_probe_answers_every_capability_call_for_a_mode_and_a_failed_one_is_not_kept(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "requests"
    client = HostClient(
        host_path=fake_host(tmp_path, probed_host(log, fail_first=True)), video_decode="auto"
    )
    with pytest.raises(Alpha2HostError) as caught:
        client.capabilities()
    assert caught.value.code == "ALPHA2_ERROR"
    withdrawn = threading.Event()
    with ThreadPoolExecutor(3) as pool:
        first = pool.submit(client.capabilities)
        second = pool.submit(client.capabilities, cancel=withdrawn)
        third = pool.submit(client.capabilities)
        withdrawn.set()
        report = first.result()
        assert third.result() is report
        try:
            assert second.result() is report  # it may have been answered before the cancel
        except Alpha2AbortError:
            pass
    assert client.capabilities() is report
    assert report["capabilities"]["decode"]["mode"] == "auto"
    software = client.capabilities(video_decode="software")
    assert software["capabilities"]["decode"]["mode"] == "software"
    assert client.capabilities(video_decode="software") is software
    assert lines_in(log) == ["capabilities auto", "capabilities auto", "capabilities software"]


def renderer_host(log: Path) -> str:
    return f"""
    decode = os.environ.get("ALPHA2_VIDEO_DECODE", "unset")
    renderer = os.environ.get("ALPHA2_RENDERER", "unset")
    def answer(request):
        record({str(log)!r}, request["operation"] + " " + decode + " " + renderer)
        if request["operation"] == "capabilities":
            send(request, ok=True, result={{"status": "ready", "renderer": renderer, "rendererReason": None, "capabilities": {{}}, "effectCatalog": {{}}}})
        else:
            send(request, ok=True, result={{"output": request.get("output")}})
    serve(answer)
    """


def test_a_renderer_that_is_not_one_is_refused_before_a_host_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    host_path = fake_host(tmp_path, started_host(starts))
    client = HostClient(host_path=host_path)
    calls: list[Callable[[], object]] = [
        lambda: HostClient(host_path=host_path, renderer="software"),  # type: ignore[arg-type]
        lambda: client.export(**export_args(tmp_path / "c.mp4", renderer="vulkan")),
        lambda: client.capabilities(renderer="GPU"),  # type: ignore[arg-type]
        lambda: alpha2.export(
            **export_args(tmp_path / "m.mp4", renderer="metal"), host_path=host_path
        ),
        lambda: alpha2.capabilities(host_path=host_path, renderer=""),  # type: ignore[arg-type]
    ]
    for call in calls:
        with pytest.raises(Alpha2HostError) as caught:
            call()
        assert caught.value.code == "INVALID_INPUT"
        assert "renderer must be auto, gpu, or cpu" in caught.value.message
        assert caught.value.failure["code"] == "INVALID_INPUT"
    assert lines_in(starts) == []


def test_the_renderer_reaches_the_host_in_its_environment_per_request_over_the_clients(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("ALPHA2_VIDEO_DECODE", raising=False)
    monkeypatch.delenv("ALPHA2_RENDERER", raising=False)
    log = tmp_path / "requests"
    host_path = fake_host(tmp_path, renderer_host(log))
    HostClient(host_path=host_path).export(**export_args(tmp_path / "unset.mp4"))
    client = HostClient(host_path=host_path, renderer="cpu", video_decode="software")
    client.export(**export_args(tmp_path / "client.mp4"))
    client.export(**export_args(tmp_path / "request.mp4", renderer="gpu"))
    client.conform(project_document())
    alpha2.export(**export_args(tmp_path / "module.mp4", renderer="auto"), host_path=host_path)
    assert lines_in(log) == [
        "export unset unset",
        "export software cpu",
        "export software gpu",
        "conform software cpu",
        "export unset auto",
    ]


def test_one_probe_answers_every_capability_call_for_a_renderer_and_the_environments_is_its_own(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("ALPHA2_VIDEO_DECODE", raising=False)
    monkeypatch.delenv("ALPHA2_RENDERER", raising=False)
    log = tmp_path / "requests"
    host_path = fake_host(tmp_path, renderer_host(log))
    client = HostClient(host_path=host_path, renderer="cpu")
    cpu = client.capabilities()
    assert cpu["renderer"] == "cpu"
    assert client.capabilities() is cpu
    assert client.capabilities(renderer="cpu") is cpu
    gpu = client.capabilities(renderer="gpu")
    assert gpu["renderer"] == "gpu"
    assert client.capabilities(renderer="gpu") is gpu
    # The decode mode and the renderer each start their own probe.
    software = client.capabilities(video_decode="software")
    assert software is not cpu
    assert software["renderer"] == "cpu"
    # Unset, the renderer comes from the environment when the probe starts.
    unset = HostClient(host_path=host_path)
    host = unset.capabilities()
    assert host["renderer"] == "unset"
    assert unset.capabilities() is host
    monkeypatch.setenv("ALPHA2_RENDERER", "cpu")
    environment = unset.capabilities()
    assert environment["renderer"] == "cpu"
    assert environment is not host
    assert lines_in(log) == [
        "capabilities unset cpu",
        "capabilities unset gpu",
        "capabilities software cpu",
        "capabilities unset unset",
        "capabilities unset cpu",
    ]


def test_module_calls_that_differ_only_in_renderer_share_one_pool(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("ALPHA2_VIDEO_DECODE", raising=False)
    monkeypatch.delenv("ALPHA2_RENDERER", raising=False)
    log = tmp_path / "requests"
    host_path = fake_host(tmp_path, renderer_host(log))
    with ThreadPoolExecutor(2) as pool:
        cpu_call = pool.submit(alpha2.capabilities, host_path=host_path, renderer="cpu")
        gpu_call = pool.submit(alpha2.capabilities, host_path=host_path, renderer="gpu")
        cpu, gpu = cpu_call.result(), gpu_call.result()
    assert (cpu["renderer"], gpu["renderer"]) == ("cpu", "gpu")
    assert alpha2.capabilities(host_path=host_path, renderer="cpu") is cpu
    assert sorted(lines_in(log)) == ["capabilities unset cpu", "capabilities unset gpu"]


def test_an_export_run_again_in_software_after_a_decode_stall_keeps_its_renderer(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    runs = tmp_path / "runs"
    host_path = fake_host(
        tmp_path,
        f"""
        request, _ = read_request()
        mode = os.environ.get("ALPHA2_VIDEO_DECODE", "unset")
        record({str(runs)!r}, mode + " " + os.environ.get("ALPHA2_RENDERER", "unset"))
        if mode == "software":
            send(request, ok=True, result={{"frames": 9}})
        else:
            send(request, ok=False, error={{
                "code": "DECODE_STALLED",
                "message": "decode shard 0 has been inside one decoder operation for 40001 ms",
                "failure": {{"code": "GPU_TIMEOUT", "origin": "native-host", "operation": "export",
                             "backend": "native", "phase": "decode", "resource": "gpu"}},
            }})
        """,
    )
    HostClient(host_path=host_path, video_decode="auto", renderer="cpu").export(
        **export_args(tmp_path / "out.mp4")
    )
    assert lines_in(runs) == ["auto cpu", "software cpu"]


def test_an_answer_to_a_line_the_host_could_not_read_is_the_requests_answer(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        read_message()
        send({"requestId": "unknown"}, ok=False,
            error={"code": "INVALID_REQUEST", "message": "the request line is 40000000 bytes; the maximum is 134217728"})
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "INVALID_REQUEST"
    assert "maximum is 134217728" in caught.value.message
    assert caught.value.failure["code"] == "INVALID_INPUT"


def test_requests_too_long_or_not_json_are_refused_before_a_host_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    starts = tmp_path / "starts"
    client = HostClient(host_path=fake_host(tmp_path, started_host(starts)))
    assets = {f"asset-{index}": f"/media/{'x' * 1_000}-{index}.mov" for index in range(140_000)}
    with pytest.raises(Alpha2HostError, match="the host reads at most 134217728") as caught:
        client.export(**export_args(tmp_path / "out.mp4", assets=assets))
    assert caught.value.code == "INVALID_INPUT"
    with pytest.raises(Alpha2HostError, match="JSON") as caught:
        client.export(**export_args(tmp_path / "out.mp4", effects=[{"id": object()}]))
    assert caught.value.code == "INVALID_INPUT"
    # A project over the engine's text budget: only its text shows it.
    project = project_document()
    project["name"] = "x" * (64 * 1024 * 1024)
    for call in (
        lambda: client.export(**export_args(tmp_path / "out.mp4", project=project)),
        lambda: client.conform(project),
    ):
        with pytest.raises(Alpha2ProjectValidationError) as refused:
            call()
        assert "67108864" in refused.value.problems[0]
    assert lines_in(starts) == []


def test_a_host_that_exits_before_reading_a_large_request_reports_why(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        write_message({"type": "fatal", "error": {"code": "GPU_INIT_FAILED", "message": "no adapter"}})
        sys.exit(70)
        """,
    )
    assets = {f"asset-{i}": f"/media/{'x' * 48}-{i}.mov" for i in range(120_000)}
    with pytest.raises(Alpha2GpuInitializationError):
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4", assets=assets))


def test_cancelling_while_a_large_request_is_being_written_returns_as_an_abort(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(tmp_path, "while True:\n    time.sleep(1)\n")
    assets = {f"asset-{i}": f"/media/{'x' * 48}-{i}.mov" for i in range(120_000)}
    cancel = threading.Event()
    threading.Timer(0.2, cancel.set).start()
    with pytest.raises(Alpha2AbortError):
        HostClient(host_path=host_path).export(
            **export_args(tmp_path / "out.mp4", assets=assets, cancel=cancel)
        )


def test_a_progress_callback_that_raises_fails_the_export_and_ends_its_host(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    pid_file = tmp_path / "host.pid"
    host_path = fake_host(
        tmp_path,
        f"""
        open({str(pid_file)!r}, "w").write(str(os.getpid()))
        request, _ = read_request()
        send(request, type="progress", phase="frames", completed=1, total=2)
        while True:
            time.sleep(1)
        """,
    )

    def progress(_: object) -> None:
        raise RuntimeError("progress store down")

    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(
            **export_args(tmp_path / "out.mp4", on_progress=progress)
        )
    assert caught.value.code == "PROGRESS_CALLBACK_FAILED"
    assert str(caught.value.__cause__) == "progress store down"
    assert caught.value.failure["phase"] == "render"
    assert pid_exited(int(pid_file.read_text()))
    assert len(started_pids) == 1 and pid_exited(started_pids[0])


def test_a_host_that_cannot_make_its_gpu_device_is_a_gpu_initialization_error(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    cases = [
        (
            "fatal",
            """
            sys.stderr.write("alpha2-host: no adapter\\n"); sys.stderr.flush()
            write_message({"type": "fatal", "error": {"code": "GPU_INIT_FAILED", "message": "could not acquire a GPU adapter"}})
            sys.exit(70)
            """,
            "could not acquire a GPU adapter",
            None,
        ),
        (
            "exit-70",
            'sys.stderr.write("alpha2-host: no adapter\\n")\nsys.exit(70)\n',
            "no adapter",
            70,
        ),
    ]
    for name, source, message, exit_code in cases:
        client = HostClient(host_path=fake_host(tmp_path / name, source))
        calls: list[Callable[[], object]] = [
            client.capabilities,
            lambda client=client: client.export(**export_args(tmp_path / "out.mp4")),  # type: ignore[misc]
        ]
        for call in calls:
            with pytest.raises(Alpha2GpuInitializationError) as caught:
                call()
            assert caught.value.code == "GPU_INIT_FAILED", name
            assert message in caught.value.message, name
            assert (
                caught.value.stderr is not None and "alpha2-host: no adapter" in caught.value.stderr
            )
            if exit_code is not None:
                assert caught.value.exit_code == exit_code
            assert caught.value.failure["hostCode"] == "GPU_INIT_FAILED"
    decode = HostClient(
        host_path=fake_host(
            tmp_path / "decode",
            answering_host(
                '{"type": "fatal", "error": {"code": "HARDWARE_DECODE_UNAVAILABLE", '
                '"message": "a Vulkan adapter without a video-decode queue"}}'
            ),
        )
    )
    with pytest.raises(Alpha2HostError) as caught:
        decode.capabilities()
    assert caught.value.code == "HARDWARE_DECODE_UNAVAILABLE"
    assert not isinstance(caught.value, Alpha2GpuInitializationError)
    assert caught.value.failure["code"] == "INVALID_INPUT"


def test_every_refusal_the_client_makes_itself_carries_a_classified_failure(
    tmp_path: Path,
) -> None:
    client = HostClient(host_path=sys.executable)
    refusals: list[dict[str, Any]] = [
        export_args("/tmp/never.mp4", start_ticks=0, end_ticks=0),
        export_args("/tmp/never.mp4", video_quality={"mode": "bogus"}),
        export_args("/tmp/never.mp4", loudness={"target": "loud"}),
        export_args("/tmp/never.mp4", project={"id": ""}),
    ]
    for arguments in refusals:
        with pytest.raises(Alpha2HostError) as caught:
            client.export(**arguments)
        failure = caught.value.failure
        assert caught.value.code == "INVALID_INPUT"
        assert failure["code"] == "INVALID_INPUT"
        assert failure["origin"] == "python"
        assert failure["operation"] == "export"
        assert failure["phase"] == "prepare"
        assert failure["hostCode"] == "INVALID_INPUT"
        assert failure["message"] == caught.value.message


def test_a_host_error_without_a_failure_is_classified_by_its_code(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    cases = [
        (
            '{"ok": False, "error": {"code": "INVALID_REQUEST", "message": "bad"}}',
            "INVALID_REQUEST",
            "INVALID_INPUT",
        ),
        (
            '{"ok": False, "error": {"code": "UNSUPPORTED_PROTOCOL", "message": "v9"}}',
            "UNSUPPORTED_PROTOCOL",
            "INVALID_INPUT",
        ),
        (
            '{"type": "fatal", "error": {"code": "GPU_INIT_FAILED", "message": "none"}}',
            "GPU_INIT_FAILED",
            "UNKNOWN",
        ),
    ]
    for index, (answer, code, failure_code) in enumerate(cases):
        client = HostClient(host_path=fake_host(tmp_path / f"host-{index}", answering_host(answer)))
        with pytest.raises(Alpha2HostError) as caught:
            client.export(**export_args(tmp_path / f"out-{index}.mp4"))
        assert caught.value.code == code
        assert caught.value.failure["code"] == failure_code
        assert caught.value.failure["hostCode"] == code
        assert caught.value.failure["phase"] == "prepare"


def test_conform_sends_the_purpose_as_the_host_spells_it(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    recorded = tmp_path / "request.json"
    host_path = fake_host(
        tmp_path,
        f"""
        request, line = read_request()
        open({str(recorded)!r}, "w").write(line)
        send(request, ok=True, result={{"catalog": {{"assets": []}}, "report": {{"ready": True}}}})
        """,
    )
    result = HostClient(host_path=host_path).conform(
        project_document(), assets={"a": "/a.mov"}, purpose="preview_proxy"
    )
    assert result["report"]["ready"] is True
    import json

    request = json.loads(recorded.read_text())
    assert request["purpose"] == "preview_proxy"
    assert request["assets"] == {"a": os.path.abspath("/a.mov")}
    with pytest.raises(Alpha2ProjectValidationError):
        HostClient(host_path=host_path).conform({**project_document(), "schema_version": 9})


def test_a_host_that_never_writes_is_host_start_timeout(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    host_path = fake_host(tmp_path, "time.sleep(60)\n", ready=False)
    started = time.monotonic()
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path, host_start_timeout=0.3).export(
            **export_args(tmp_path / "out.mp4")
        )
    assert time.monotonic() - started < 3
    assert caught.value.code == "HOST_START_TIMEOUT", caught.value.message
    assert "did not start within 0.3 s" in caught.value.message
    assert caught.value.failure["code"] == "UNKNOWN"
    assert len(started_pids) == 1 and pid_exited(started_pids[0])


def test_a_host_that_writes_ready_late_but_in_time_is_not_timed_out(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        time.sleep(0.2)
        begin()
        state()
        request, _ = read_request()
        time.sleep(1.5)
        send(request, ok=True, result={"output": request["output"]})
        """,
        ready=False,
    )
    client = HostClient(host_path=host_path, host_start_timeout=0.5)
    assert client.export(**export_args("slow.mp4")) == {"output": str(Path.cwd() / "slow.mp4")}


def test_a_state_message_is_not_an_answer(tmp_path: Path, fake_host: FakeHost) -> None:
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        state("ready", "started", activity="busy")
        send(request, ok=True, result={"output": request["output"]})
        """,
    )
    assert HostClient(host_path=host_path).export(**export_args("ready.mp4")) == {
        "output": str(Path.cwd() / "ready.mp4")
    }


def test_a_start_timeout_that_is_not_above_zero_is_refused(tmp_path: Path) -> None:
    for timeout in (0, -1.0, float("nan"), "30", True):
        with pytest.raises(ValueError, match="host_start_timeout"):
            HostClient(host_path=sys.executable, host_start_timeout=timeout)  # type: ignore[arg-type]
    assert HostClient(host_path=sys.executable, host_start_timeout=None).host_start_timeout is None


def cancellable_host(log: Path, answer_cancel: bool = True) -> str:
    """Answers a cancel as ``serve`` does and serves on; ``answer_cancel=False`` ignores it."""
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()))
    def answer(request):
        record({str(log)!r}, request["operation"])
        if request["operation"] != "export":
            send(request, ok=True, result={{}})
            return
        send(request, type="progress", phase="frames", completed=0, total=100)
        while (received := read_message()) is not None:
            message, _ = received
            record({str(log)!r}, message["operation"] + " " + message["target"])
            if {answer_cancel}:
                send(message, ok=True, result={{"target": message["target"], "state": "running"}})
                time.sleep(0.1)
                record({str(log)!r}, "partial removed")
                send(request, ok=False, error={{"code": "CANCELLED", "message": "cancelled"}})
                return
        record({str(log)!r}, "input ended")
        sys.exit(0)
    serve(answer)
    """


def test_an_abort_writes_a_cancel_line_returns_once_the_host_answers_and_the_host_serves_on(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    host_path = fake_host(tmp_path, cancellable_host(log))
    cancel = threading.Event()
    cancelled: list[float] = []

    def progress(_: object) -> None:
        cancelled.append(time.monotonic())
        cancel.set()

    client = HostClient(host_path=host_path)
    with pytest.raises(Alpha2AbortError):
        client.export(**export_args("out.mp4", cancel=cancel, on_progress=progress))
    assert time.monotonic() - cancelled[0] < 3
    lines = lines_in(log)
    assert lines[1] == "export"
    assert re.fullmatch(r"cancel python-\d+-\d+-\d+", lines[2]), lines
    assert lines[3] == "partial removed"
    client.conform(project_document())
    assert lines_in(log)[4] == "conform"
    assert len(started_pids) == 1 and not pid_exited(started_pids[0])
    client.close()
    assert pid_exited(started_pids[0])


def test_a_host_that_does_not_answer_a_cancel_within_the_timeout_is_ended(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    host_path = fake_host(tmp_path, cancellable_host(log, answer_cancel=False))
    cancel = threading.Event()
    cancelled: list[float] = []

    def progress(_: object) -> None:
        cancelled.append(time.monotonic())
        cancel.set()

    with pytest.raises(Alpha2AbortError):
        HostClient(host_path=host_path).export(
            **export_args("out.mp4", cancel=cancel, on_progress=progress)
        )
    waited = time.monotonic() - cancelled[0]
    assert 4.9 <= waited < 10, f"returned {waited:.2f} s after the cancel"
    assert re.fullmatch(r"cancel python-\d+-\d+-\d+", lines_in(log)[2])
    assert len(started_pids) == 1 and pid_exited(started_pids[0])


def test_a_silent_grandchild_holding_the_pipes_does_not_block_cancel(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    pid_file = tmp_path / "grandchild.pid"
    host_path = fake_host(
        tmp_path,
        f"""
        import subprocess
        request, _ = read_request()
        send(request, type="progress", phase="frames", completed=0, total=9)
        # Inherits this host's stdout and stderr, and outlives it.
        grandchild = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        open({str(pid_file)!r}, "w").write(str(grandchild.pid))
        os._exit(0)
        """,
    )
    cancel = threading.Event()
    started = time.monotonic()
    try:
        with pytest.raises(Alpha2AbortError):
            HostClient(host_path=host_path).export(
                **export_args("out.mp4", cancel=cancel, on_progress=lambda _: cancel.set())
            )
        assert time.monotonic() - started < 15
    finally:
        if pid_file.exists():
            grandchild = int(pid_file.read_text())
            if sys.platform == "win32":
                subprocess.run(["taskkill", "/F", "/PID", str(grandchild)], capture_output=True)
            else:
                os.kill(grandchild, 9)


def test_a_host_windows_blocks_by_policy_is_host_blocked(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    for status in (
        0xC0000428,
        0xC0000361,
        0xC0000362,
        0xC0000363,
        0xC0000364,
        0xC0E90002,
        0xC0E90007,
        0xC0E9000A,
        0xC0E9000B,
    ):
        assert windows_policy_blocked(status), hex(status)
        # The same status as a signed C int.
        assert windows_policy_blocked(status - 2**32)
    for status in (0xC0000135, 0xC0000005, 0, 1, 127, None):
        assert not windows_policy_blocked(status), status
    if sys.platform != "win32":
        return
    # STATUS_INVALID_IMAGE_HASH, as Code Integrity ends a process whose DLL it refused.
    client = HostClient(host_path=fake_host(tmp_path, "os._exit(-1073740760)\n", ready=False))
    with pytest.raises(Alpha2HostError) as caught:
        client.export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "HOST_BLOCKED", caught.value.message
    assert "status 0xC0000428" in caught.value.message
    assert "Smart App Control" in caught.value.message
    assert caught.value.failure["code"] == "INVALID_INPUT"


@pytest.mark.skipif(
    not sys.platform.startswith("linux"),
    reason="Linux only: the loader's missing interpreter is how a glibc build fails on musl",
)
def test_a_spawn_enoent_for_an_existing_file_is_host_load_failed(tmp_path: Path) -> None:
    host_path = tmp_path / "alpha2-host"
    # execve fails ENOENT for a missing interpreter exactly as for a missing ELF loader.
    host_path.write_text("#!/nonexistent/interp\n", encoding="utf-8")
    host_path.chmod(0o755)
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).capabilities()
    assert caught.value.code == "HOST_LOAD_FAILED", caught.value.message
    assert "program interpreter /nonexistent/interp is missing" in caught.value.message
    assert "musl" in caught.value.message
    assert caught.value.failure["code"] == "INVALID_INPUT"


def test_the_glibc_refusal_names_the_floor(tmp_path: Path, fake_host: FakeHost) -> None:
    words = (
        "/opt/alpha2/bin/alpha2-host: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not "
        "found (required by /opt/alpha2/bin/alpha2-host)"
    )
    client = HostClient(
        host_path=fake_host(
            tmp_path, f"sys.stderr.write({words + chr(10)!r})\nsys.exit(1)\n", ready=False
        )
    )
    with pytest.raises(Alpha2HostError) as caught:
        client.capabilities()
    assert caught.value.code == "HOST_LOAD_FAILED", caught.value.message
    assert re.search(
        r"GLIBC_2\.38.*; this alpha2-host needs glibc 2\.\d+ or later \(this system has ",
        caught.value.message,
        re.S,
    ), caught.value.message


def test_a_lone_surrogate_in_a_project_is_an_alpha2_host_error(tmp_path: Path) -> None:
    # A file name decoded with surrogate escapes carries a lone surrogate, which has no UTF-8.
    project = {**project_document(), "name": "take-\udcff"}
    never = tmp_path / "never-started"
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=sys.executable).export(
            project, never / "out.mp4", assets={"a": "/a.mov"}, end_ticks=1
        )
    assert caught.value.code == "INVALID_INPUT"
    assert "project text is not valid Unicode" in str(caught.value)
    assert not never.exists()


def pooled_host(
    log: Path, answer: str = 'send(request, ok=True, result={"pid": os.getpid()})'
) -> str:
    """Records its start and each request, then runs ``answer`` for the request."""
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()) + " " + os.environ.get("ALPHA2_VIDEO_DECODE", "unset"))
    def answer(request):
        record({str(log)!r}, "request " + str(os.getpid()))
        {answer}
    serve(answer)
    """


def starts_in(log: Path) -> list[int]:
    return [int(line.split(" ")[1]) for line in lines_in(log) if line.startswith("start")]


def test_requests_one_after_another_run_on_one_host(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log)))
    first = client.capabilities()
    client.conform(project_document())
    client.export(**export_args("out.mp4"))
    assert len(started_pids) == 1 and len(starts_in(log)) == 1
    assert [line for line in lines_in(log) if line.startswith("request")] == [
        f"request {first['pid']}"
    ] * 3


def test_a_host_that_reports_draining_while_idle_gets_no_more_requests(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    answer = """send(request, ok=True, result={"pid": os.getpid()})
        state("draining", "memory", activity="idle")"""
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)))
    first = client.export(**export_args("first.mp4"))
    # The host exits only once the client, having seen it drain, closes its input.
    wait_until(lambda: pid_exited(started_pids[0]))
    second = client.export(**export_args("second.mp4"))
    assert first["pid"] != second["pid"]
    assert len(started_pids) == 2


def test_a_response_that_says_the_host_is_draining_ends_its_use(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    answer = 'send(request, ok=True, result={"pid": os.getpid()}, hostState="draining")'
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)))
    first = client.export(**export_args("first.mp4"))
    second = client.export(**export_args("second.mp4"))
    assert first["pid"] != second["pid"]
    assert len(started_pids) == 2
    wait_until(lambda: pid_exited(started_pids[0]))


def test_a_host_that_exits_while_idle_is_replaced_by_a_new_one(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    answer = """send(request, ok=True, result={"pid": os.getpid()})
        time.sleep(0.2)
        os._exit(0)"""
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)))
    first = client.export(**export_args("first.mp4"))
    wait_until(lambda: not client._pool._hosts)
    second = client.export(**export_args("second.mp4"))
    assert first["pid"] != second["pid"]
    assert len(started_pids) == 2


def test_a_request_answered_host_retiring_runs_again_on_another_host(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    marker = tmp_path / "first-host"
    answer = f"""if not os.path.exists({str(marker)!r}):
            open({str(marker)!r}, "w").write(str(os.getpid()))
        if open({str(marker)!r}).read() == str(os.getpid()):
            state("draining", "memory")
            send(request, ok=False, error={{"code": "HOST_RETIRING", "message": "draining"}}, hostState="draining")
        else:
            send(request, ok=True, result={{"output": request["output"]}})"""
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)))
    assert client.export(**export_args("out.mp4")) == {"output": str(Path.cwd() / "out.mp4")}
    assert len(starts_in(log)) == 2


def test_a_request_answered_host_retiring_by_every_host_fails_after_a_few_tries(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    answer = """send(request, ok=False, error={"code": "HOST_RETIRING", "message": "draining"}, hostState="draining")"""
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)), concurrency=2)
    with pytest.raises(Alpha2HostError) as caught:
        client.export(**export_args("out.mp4"))
    assert caught.value.code == "HOST_RETIRING"
    assert len(starts_in(log)) == 4


def test_hosts_are_kept_apart_by_their_modes(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("ALPHA2_VIDEO_DECODE", raising=False)
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log)), concurrency=2)
    software = client.export(**export_args("a.mp4", video_decode="software"))
    auto = client.export(**export_args("b.mp4", video_decode="auto"))
    again = client.export(**export_args("c.mp4", video_decode="software"))
    unset = client.export(**export_args("d.mp4"))
    assert software["pid"] == again["pid"] != auto["pid"]
    assert unset["pid"] not in (software["pid"], auto["pid"])
    modes = [line.split(" ")[2] for line in lines_in(log) if line.startswith("start")]
    assert modes == ["software", "auto", "unset"]


def test_at_the_limit_an_idle_host_of_other_modes_is_ended_to_make_room(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log)), concurrency=1)
    software = client.export(**export_args("a.mp4", video_decode="software"))
    assert not pid_exited(started_pids[0])
    auto = client.export(**export_args("b.mp4", video_decode="auto"))
    assert software["pid"] != auto["pid"]
    assert len(started_pids) == 2
    assert pid_exited(started_pids[0])


def test_concurrent_requests_share_at_most_concurrency_hosts(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    # One file per host: appends from two processes to one file can lose a line on Windows.
    running = f"{str(tmp_path / 'running-')!r} + str(os.getpid())"
    answer = f"""record({running}, "1 " + str(time.perf_counter_ns()))
        time.sleep(0.2)
        record({running}, "0 " + str(time.perf_counter_ns()))
        send(request, ok=True, result={{"pid": os.getpid()}})"""
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)), concurrency=2)
    with ThreadPoolExecutor(6) as pool:
        calls = [pool.submit(client.export, **export_args(f"{index}.mp4")) for index in range(6)]
        pids = {call.result()["pid"] for call in calls}
    assert len(started_pids) == 2 and len(pids) == 2
    marks = sorted(
        (int(time), int(starts))
        for path in tmp_path.glob("running-*")
        for starts, time in (line.split() for line in lines_in(path))
    )
    assert len(marks) == 12
    depth = 0
    for _, starts in marks:
        depth += 1 if starts else -1
        assert depth <= 2


EXITING_HOST = """
request, _ = read_request()
sys.exit(3)
"""


def open_descriptors() -> int | None:
    try:
        return len(os.listdir("/proc/self/fd"))
    except OSError:
        return None


def test_hosts_that_exit_mid_request_leave_no_thread_or_pipe_behind(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    client = HostClient(host_path=fake_host(tmp_path, EXITING_HOST))
    with pytest.raises(Alpha2HostError):
        client.export(**export_args("out.mp4"))
    threads, descriptors = threading.active_count(), open_descriptors()
    for _ in range(40):
        with pytest.raises(Alpha2HostError) as caught:
            client.export(**export_args("out.mp4"))
        assert caught.value.code == "HOST_EXITED"
    wait_until(lambda: threading.active_count() <= threads)
    wait_until(lambda: open_descriptors() == descriptors)


EXHAUSTION = r"""
import os, resource, sys, textwrap
sys.path.insert(0, sys.argv[1])
from conftest import HOST_PRELUDE, export_args
from alpha2 import Alpha2HostError, HostClient
with open("serve", "w") as file:
    file.write(HOST_PRELUDE + "begin()\nstate()\n" + textwrap.dedent(sys.argv[2]))
resource.setrlimit(resource.RLIMIT_NOFILE, (64, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))
client = HostClient(host_path=sys.executable)
codes = set()
for _ in range(80):
    try:
        client.export(**export_args("out.mp4"))
    except Alpha2HostError as error:
        codes.add(error.code)
print(" ".join(sorted(codes)))
"""


@pytest.mark.skipif(sys.platform == "win32", reason="RLIMIT_NOFILE is POSIX")
def test_eighty_host_deaths_fit_in_sixty_four_descriptors(tmp_path: Path) -> None:
    script = tmp_path / "exhaust.py"
    script.write_text(EXHAUSTION, encoding="utf-8")
    finished = subprocess.run(
        [sys.executable, str(script), str(Path(__file__).parent), EXITING_HOST],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert finished.returncode == 0, finished.stderr
    assert finished.stdout.split() == ["HOST_EXITED"], finished.stdout


def test_an_interrupt_while_a_new_host_starts_ends_it_and_frees_its_slot(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    from alpha2 import client as client_module

    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log)))
    start, spawn = client_module._Host.start, client_module._Pool._spawn
    armed = {"start": False, "spawn": False}

    def interrupted_start(self: Any) -> None:
        if armed["start"]:
            armed["start"] = False
            raise KeyboardInterrupt
        start(self)

    def interrupted_spawn(self: Any, *args: Any) -> Any:
        host = spawn(self, *args)
        if armed["spawn"]:
            armed["spawn"] = False
            raise KeyboardInterrupt
        return host

    monkeypatch.setattr(client_module._Host, "start", interrupted_start)
    monkeypatch.setattr(client_module._Pool, "_spawn", interrupted_spawn)
    for where in ("start", "spawn"):
        # No idle host, so the request starts one.
        client.close()
        armed[where] = True
        with pytest.raises(KeyboardInterrupt):
            client.export(**export_args("out.mp4"))
        assert client._pool._starting == 0
        client.export(**export_args("out.mp4"))
    assert len(client._pool._hosts) == 1
    started = client._pool._hosts[0].process.pid
    for host_pid in starts_in(log):
        if host_pid != started:
            wait_until(lambda host_pid=host_pid: pid_exited(host_pid))


def test_close_ends_the_hosts_and_a_later_request_starts_a_new_one(
    tmp_path: Path, fake_host: FakeHost, started_pids: list[int]
) -> None:
    log = tmp_path / "log"
    host_path = fake_host(tmp_path, pooled_host(log))
    with HostClient(host_path=host_path, concurrency=2) as client:
        client.export(**export_args("a.mp4", video_decode="software"))
        client.export(**export_args("b.mp4", video_decode="auto"))
        assert len(started_pids) == 2 and not any(map(pid_exited, started_pids))
    assert all(map(pid_exited, started_pids))
    client.export(**export_args("c.mp4"))
    assert len(started_pids) == 3
    client.close()
    assert pid_exited(started_pids[2])


def test_a_change_to_the_environment_starts_a_new_host_and_no_change_reuses_one(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    log = tmp_path / "log"
    answer = """send(request, ok=True, result={"pid": os.getpid(), "flag": os.environ.get("ALPHA2_TEST_FLAG")})"""
    monkeypatch.setenv("ALPHA2_TEST_FLAG", "one")
    client = HostClient(host_path=fake_host(tmp_path, pooled_host(log, answer)), concurrency=2)
    first = client.export(**export_args("a.mp4"))
    same = client.export(**export_args("b.mp4"))
    monkeypatch.setenv("ALPHA2_TEST_FLAG", "two")
    changed = client.export(**export_args("c.mp4"))
    assert first["pid"] == same["pid"] != changed["pid"]
    assert (first["flag"], changed["flag"]) == ("one", "two")
    assert len(starts_in(log)) == 2


def test_a_relative_output_is_written_in_the_directory_the_call_was_made_from(
    tmp_path: Path, fake_host: FakeHost, monkeypatch: pytest.MonkeyPatch
) -> None:
    first, second = tmp_path / "first", tmp_path / "second"
    second.mkdir()
    log = tmp_path / "log"
    host = f"""
    def answer(request):
        record({str(log)!r}, json.dumps({{"cwd": os.getcwd(), "assets": request["assets"]}}))
        with open(request["output"], "a") as file:
            file.write("written for " + request["project"]["name"])
        send(request, ok=True, result={{"output": request["output"]}})
    serve(answer)
    """
    client = HostClient(host_path=fake_host(first, host))
    for name in ("first", "second"):
        monkeypatch.chdir(tmp_path / name)
        project = {**project_document(), "name": name}
        args = export_args("out.mp4", project=project, assets={"a": "clip.mp4"})
        assert client.export(**args) == {"output": str(Path.cwd() / "out.mp4")}
    assert [json.loads(line) for line in lines_in(log)] == [
        {"cwd": str(first), "assets": {"a": str(first / "clip.mp4")}},
        {"cwd": str(first), "assets": {"a": str(second / "clip.mp4")}},
    ]
    assert (first / "out.mp4").read_text() == "written for first"
    assert (second / "out.mp4").read_text() == "written for second"


CLIENT_SCENARIOS = [
    scenario
    for scenario in corpus("client-behavior-corpus.json")
    if "python" in scenario.get("clients", ["python"])
]


def paths_host(log: Path) -> str:
    """Logs its start and every request with its pid and working directory, and answers each."""
    return f"""
    def entry(**fields):
        record({str(log)!r}, json.dumps({{"pid": os.getpid(), "cwd": os.getcwd(), **fields}}))
    entry(start=True)
    results = {{
        "capabilities": {{"status": "ready", "capabilities": {{}}, "effectCatalog": {{}}}},
        "still": {{"stills": []}},
        "conform": {{"catalog": {{}}, "report": {{}}}},
        "extract": {{"dataSets": [], "timings": {{}}}},
    }}
    def answer(request):
        entry(request=request)
        result = results.get(request["operation"], {{"output": request.get("output")}})
        send(request, ok=True, result=result)
    serve(answer)
    """


def with_root(value: Any, root: str) -> Any:
    if isinstance(value, str):
        return value.replace("{root}", root)
    if isinstance(value, list):
        return [with_root(item, root) for item in value]
    if isinstance(value, dict):
        return {key: with_root(item, root) for key, item in value.items()}
    return value


def at_pointer(value: Any, pointer: str) -> Any:
    for key in pointer.split("/")[1:]:
        value = value[int(key)] if isinstance(value, list) else value[key]
    return value


def scenario_call(client: HostClient, operation: str, fields: dict[str, Any]) -> None:
    arguments = wire_to_python(fields)
    if operation == "export":
        client.export(**export_args(arguments.pop("output"), **arguments))
    elif operation == "still":
        stills = [(alpha2.ticks(still["time"]), still.get("output")) for still in fields["stills"]]
        del arguments["stills"]
        client.stills(project_document(), stills, **arguments)
    elif operation == "conform":
        client.conform(project_document(), **arguments)
    elif operation == "extract":
        plugin = {**arguments.pop("plugin"), "sha256": "0" * 64}
        request = {"extractor": "example.levels", "version": 1}
        client.extract(project_document(), request, plugin=plugin, **arguments)
    else:
        pytest.fail(f"no call for operation {operation}")


def run_request_paths(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path / "host", paths_host(log)))
    client.capabilities()
    host_directory = os.getcwd()
    sent: list[tuple[str, Any, dict[str, Any]]] = []
    for directory in scenario["given"]["directories"]:
        (tmp_path / directory).mkdir()
        os.chdir(tmp_path / directory)
        for request, expect in zip(
            scenario["given"]["requests"], scenario["expect"]["requests"], strict=True
        ):
            given = with_root(request["fields"], str(tmp_path))
            sent.append((os.getcwd(), given, expect))
            scenario_call(client, request["operation"], given)
    lines = [json.loads(line) for line in lines_in(log)]
    starts = [line for line in lines if line.get("start")]
    assert len(starts) == scenario["expect"]["hosts"]
    received = [
        line
        for line in lines
        if "request" in line and line["request"]["operation"] != "capabilities"
    ]
    assert len(received) == len(sent)
    for line, (caller, given, expect) in zip(received, sent, strict=True):
        assert (line["pid"], line["cwd"]) == (starts[0]["pid"], host_directory)
        for pointer in expect["resolved"]:
            path = at_pointer(given, pointer)
            absolute = (
                os.path.normpath(os.path.join(caller, path))
                if sys.platform == "win32"
                else caller + "/" + path
            )
            assert at_pointer(line["request"], pointer) == absolute, pointer
        for pointer in expect["unchanged"]:
            assert at_pointer(line["request"], pointer) == at_pointer(given, pointer), pointer


needs_fork = pytest.mark.skipif(not hasattr(os, "fork"), reason="this platform has no os.fork")

# The child runs only the thread that forked, which is what the client prepares it for.
QUIET_FORK = pytest.mark.filterwarnings(
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)


def fork_host(log: Path) -> str:
    """Records its start, each request and the end of its input; output ``slow`` takes 1 s."""
    return f"""
    pid = str(os.getpid())
    record({str(log)!r}, "start " + pid)
    def answer(request):
        output = os.path.basename(request.get("output") or "")
        record({str(log)!r}, " ".join(("request", pid, request["operation"], output)))
        if request["operation"] == "capabilities":
            send(request, ok=True, result={{"status": "ready", "capabilities": {{}}, "effectCatalog": {{}}}})
            return
        if output.startswith("slow"):
            time.sleep(1)
        send(request, ok=True, result={{"output": request["output"], "pid": os.getpid()}})
    serve(answer)
    record({str(log)!r}, "end of input " + pid)
    """


def host_requests(log: Path) -> list[list[str]]:
    """The operations each host served, in the order the hosts started."""
    served: dict[str, list[str]] = {}
    for line in lines_in(log):
        words = line.split(" ")
        if words[0] == "start":
            served[words[1]] = []
        elif words[0] == "request":
            served[words[1]].append(words[2])
    return list(served.values())


def in_forked_child(call: Callable[[], object], seconds: int = 10) -> str:
    """Runs ``call`` in a forked child under an alarm; returns how it ended there."""
    reader, writer = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(reader)
        outcome = "?"
        try:

            def alarm(*_: object) -> None:
                raise TimeoutError

            signal.signal(signal.SIGALRM, alarm)
            signal.alarm(seconds)
            call()
            outcome = "returned"
        except TimeoutError:
            outcome = "timed out"
        except BaseException as error:  # noqa: BLE001 - reported to the parent
            outcome = "raised " + repr(error)
        finally:
            os.write(writer, outcome.encode())
            os._exit(0)
    os.close(writer)
    data = b""
    while chunk := os.read(reader, 4096):
        data += chunk
    os.close(reader)
    os.waitpid(pid, 0)
    return data.decode()


@needs_fork
@QUIET_FORK
@pytest.mark.parametrize("video_decode", [None, "software"], ids=["same-key", "other-key"])
def test_a_forked_child_starts_its_own_host_and_the_parent_keeps_its_own(
    tmp_path: Path, fake_host: FakeHost, video_decode: str | None
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, fork_host(log)))
    client.export(**export_args("parent-1.mp4"))
    outcome = in_forked_child(
        lambda: client.export(**export_args("child.mp4", video_decode=video_decode))
    )
    client.export(**export_args("parent-2.mp4"))
    assert outcome == "returned"
    assert host_requests(log) == [["export", "export"], ["export"]]
    # The child's host ends with the child.
    wait_until(lambda: f"end of input {starts_in(log)[1]}" in lines_in(log))


def report_through(host_path: str, output: str, results: Any) -> None:
    results.put("capabilities " + alpha2.capabilities(host_path=host_path)["status"])
    alpha2.export(**export_args(output), host_path=host_path)
    results.put("export returned")


@needs_fork
@QUIET_FORK
def test_a_fork_context_worker_uses_the_module_level_client(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    import multiprocessing

    log = tmp_path / "log"
    host_path = fake_host(tmp_path, fork_host(log))
    assert alpha2.capabilities(host_path=host_path)["status"] == "ready"
    context = multiprocessing.get_context("fork")
    results: Any = context.Queue()
    worker = context.Process(
        target=report_through, args=(host_path, str(tmp_path / "child.mp4"), results)
    )
    worker.start()
    worker.join(10)
    hung = worker.is_alive()
    if hung:
        worker.kill()
        worker.join()
    assert not hung, "the worker did not finish within 10 s"
    assert [results.get(timeout=5), results.get(timeout=5)] == [
        "capabilities ready",
        "export returned",
    ]


@needs_fork
@QUIET_FORK
def test_a_parent_that_closes_its_client_ends_its_host_while_a_forked_child_lives(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, fork_host(log)))
    client.export(**export_args("parent.mp4"))
    release, hold = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(hold)
        os.read(release, 1)
        os._exit(0)
    os.close(release)
    try:
        started = time.monotonic()
        client.close()
        closed_in = time.monotonic() - started
    finally:
        os.close(hold)
        os.waitpid(pid, 0)
    assert lines_in(log)[-1] == f"end of input {starts_in(log)[0]}"
    assert closed_in < _HOST_SHUTDOWN_TIMEOUT


@needs_fork
@QUIET_FORK
def test_a_fork_while_the_parents_host_is_busy_leaves_both_requests_to_finish(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, fork_host(log)))
    with ThreadPoolExecutor(1) as pool:
        busy = pool.submit(client.export, **export_args("slow.mp4"))
        wait_until(lambda: len(lines_in(log)) == 2)
        outcome = in_forked_child(lambda: client.export(**export_args("child.mp4")))
        assert busy.result()["output"] == str(Path.cwd() / "slow.mp4")
    assert outcome == "returned"
    assert host_requests(log) == [["export"], ["export"]]


def run_fork(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    if not hasattr(os, "fork"):
        pytest.skip("this platform has no os.fork")
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, fork_host(log)))
    calls: dict[str, Callable[[], object]] = {
        "capabilities": client.capabilities,
        "export": lambda: client.export(**export_args("out.mp4")),
    }
    given = scenario["given"]
    for operation in given["parent"]:
        calls[operation]()

    def child() -> None:
        for operation in given["child"]:
            calls[operation]()

    outcome = in_forked_child(child, given["childSeconds"])
    for operation in given["parentAfter"]:
        calls[operation]()
    assert outcome == scenario["expect"]["child"]
    assert host_requests(log) == scenario["expect"]["hosts"]


class Abandoned(BaseException):
    """Stands in for a task queue's time limit, which is not an ``Exception``."""


class SoftTimeLimitExceeded(Exception):
    """What a task queue's soft time limit raises from its signal handler."""


def rendering_host(log: Path, marker: Path, *, answers_cancel: bool = True) -> str:
    """Renders an export for a second and publishes it unless a cancel came first. Without
    ``answers_cancel`` the first export of all never ends and cancels are ignored."""
    return f"""
    import threading
    pid = str(os.getpid())
    record({str(log)!r}, "start " + pid)
    cancelled = threading.Event()
    def render(request):
        send(request, type="progress", phase="frames", completed=0, total=9)
        if not {answers_cancel} and not os.path.exists({str(marker)!r}):
            open({str(marker)!r}, "w").close()
            time.sleep(3600)
        if cancelled.wait(1.0):
            cancelled.clear()
            record({str(log)!r}, "cancelled " + pid)
            send(request, ok=False, error={{"code": "CANCELLED", "message": "cancelled"}})
            return
        open(request["output"], "w").write("rendered")
        record({str(log)!r}, "published " + os.path.basename(request["output"]) + " " + pid)
        send(request, ok=True, result={{"output": request["output"], "pid": os.getpid()}})
    while (received := read_message()) is not None:
        request = received[0]
        record({str(log)!r}, "read " + request["operation"] + " " + pid)
        if request["operation"] == "export":
            threading.Thread(target=render, args=(request,), daemon=True).start()
        elif request["operation"] == "cancel" and {answers_cancel}:
            cancelled.set()
    """


def raise_abandoned(_: object) -> None:
    raise Abandoned


def run_abandon(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    for index, (given, expect) in enumerate(
        zip(scenario["given"]["hosts"], scenario["expect"]["hosts"], strict=True)
    ):
        root = tmp_path / str(index)
        log = root / "log"
        source = rendering_host(log, root / "marker", answers_cancel=given["answersCancel"])
        client = HostClient(host_path=fake_host(root, source))
        with pytest.raises(Abandoned):
            client.export(**export_args("out.mp4", on_progress=raise_abandoned))
        first = starts_in(log)[0]
        # Long enough for a request still running to publish.
        time.sleep(1.5)
        assert (f"read cancel {first}" in lines_in(log)) == expect["cancelRead"]
        assert (root / "out.mp4").exists() == expect["published"]
        served_by = client.export(**export_args("next.mp4"))["pid"]
        assert (served_by == first) == (expect["nextRequest"] == "same host")
        if expect["nextRequest"] == "new host":
            assert pid_exited(first)
        client.close()


@pytest.mark.skipif(not hasattr(signal, "setitimer"), reason="this platform has no interval timer")
@pytest.mark.parametrize("raised", [SoftTimeLimitExceeded, Abandoned, KeyboardInterrupt])
def test_an_exception_from_a_signal_handler_cancels_the_export_before_it_propagates(
    tmp_path: Path, fake_host: FakeHost, raised: type[BaseException]
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, rendering_host(log, tmp_path / "marker")))

    def limit(*_: object) -> None:
        raise raised()

    previous = signal.signal(signal.SIGALRM, limit)
    signal.setitimer(signal.ITIMER_REAL, 0.4)
    try:
        with pytest.raises(raised):
            client.export(**export_args("out.mp4"))
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)
    pid = starts_in(log)[0]
    time.sleep(1.5)
    assert f"read cancel {pid}" in lines_in(log)
    assert not (tmp_path / "out.mp4").exists()
    assert client.export(**export_args("next.mp4"))["pid"] == pid
    assert lines_in(log)[-1] == f"published next.mp4 {pid}"


def emit_host(log: Path) -> str:
    """Asks an extract's extractor for its ``end`` step and answers as the host would."""
    return f"""
    pid = str(os.getpid())
    record({str(log)!r}, "start " + pid)
    pending = {{}}
    def answer(request):
        operation = request["operation"]
        if operation == "extract":
            pending["extract"] = request
            write_message({{"type": "extract.step", "requestId": request["requestId"], "seq": 0, "step": "end"}})
        elif operation == "extract.ack":
            extract = pending.pop("extract")
            if "error" in request:
                record({str(log)!r}, "ack error " + request["error"]["message"])
                error = {{"code": "EXTRACTOR_FAILED", "message": "the extractor failed: " + request["error"]["message"]}}
                send(extract, ok=False, error=error)
            else:
                record({str(log)!r}, "ack emits " + json.dumps(request["emits"]))
                send(extract, ok=True, result={{"dataSets": [], "timings": {{}}}})
        else:
            send(request, ok=True, result={{"pid": os.getpid(), "output": request.get("output")}})
    serve(answer)
    """


class EmitsAtEnd:
    """An extractor whose ``end`` makes one emit."""

    def __init__(self, call: str, *args: Any) -> None:
        self.call = call
        self.args = args

    def describe(self) -> dict[str, Any]:
        return {
            "id": "example.levels",
            "version": 1,
            "input": {"media": "video", "from": ["source"], "format": "luma8"},
            "params_schema": {"type": "object"},
            "outputs": [{"name": "level", "kind": "scalars", "channels": ["a"]}],
        }

    def end(self, out: Any) -> None:
        getattr(out, self.call)(*self.args)


EXTRACT_REQUEST = {"extractor": "example.levels", "version": 1, "params": {}}

_NUMBERS = {"NaN": math.nan, "Infinity": math.inf, "-Infinity": -math.inf}


def corpus_value(value: Any) -> Any:
    if isinstance(value, list):
        return [corpus_value(item) for item in value]
    return _NUMBERS.get(value, value) if isinstance(value, str) else value


def run_unsendable_emit(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, emit_host(log)))
    expect = scenario["expect"]
    served_by = set()
    for emit in scenario["given"]["emits"]:
        extractor = EmitsAtEnd(emit["call"], *corpus_value(emit["args"]))
        with pytest.raises(Alpha2HostError) as caught:
            client.extract(project_document(), EXTRACT_REQUEST, extractor)
        assert caught.value.code == expect["code"]
        assert expect["messageContains"] in str(caught.value)
        served_by.add(client.export(**export_args("next.mp4"))["pid"])
    errors = [line for line in lines_in(log) if line.startswith("ack error")]
    assert len(errors) == len(scenario["given"]["emits"])
    assert len(served_by) == 1 and expect["nextRequest"] == "same host"


def preamble_host(pids: Path, stdout: str = "", stderr: str = "") -> str:
    """Records its pid, writes ``stdout`` and ``stderr`` before any frame, then waits for its
    input to end."""
    return f"""
    record({str(pids)!r}, str(os.getpid()))
    sys.stderr.write({stderr!r})
    sys.stderr.flush()
    sys.stdout.buffer.write({stdout.encode()!r})
    sys.stdout.buffer.flush()
    sys.stdin.buffer.read()
    """


def versioned_host(pids: Path, host_version: str | None) -> str:
    """Records its pid, reports ready naming ``host_version`` (none when ``None``), then serves."""
    details = {} if host_version is None else {"hostVersion": host_version}
    return f"""
    record({str(pids)!r}, str(os.getpid()))
    begin()
    write_message({{"type": "state", "state": "ready", "reason": "started", "details": {details!r}}})
    serve(lambda request: send(request, ok=True, result={{"output": request["output"]}}))
    """


def refused(client: HostClient) -> Alpha2HostError:
    with pytest.raises(Alpha2HostError) as caught:
        client.export(**export_args("out.mp4"))
    return caught.value


def assert_hosts_ended(pids: Path) -> None:
    started = [int(line) for line in lines_in(pids)]
    assert started, "no host started"
    for pid in started:
        wait_until(lambda pid=pid: pid_exited(pid))


def ended_unseen(pid: int) -> None:
    """Waits until ``pid`` has ended; on Linux only until it is a zombie whose threads are gone, so
    the client's reaper may not have seen the exit when the next request is made."""
    if sys.platform != "linux":
        wait_until(lambda: pid_exited(pid))
        return
    deadline = time.monotonic() + 10
    while True:
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
            ended = stat.split(") ")[1][0] == "Z" and len(os.listdir(f"/proc/{pid}/task")) == 1
        except (OSError, IndexError):
            return
        if ended:
            return
        assert time.monotonic() < deadline, f"host {pid} did not end"
        time.sleep(0.002)


def run_dead_idle_host(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    for index, end in enumerate(scenario["given"]["ends"]):
        log = tmp_path / f"log-{index}"
        exits = end == "exits after answering"
        answer = 'send(request, ok=True, result={"pid": os.getpid()})'
        if exits:
            answer += "\n        time.sleep(0.05)\n        os._exit(0)"
        fake_host(tmp_path / str(index), pooled_host(log, answer))
        # A venv interpreter on Windows is a launcher that holds the pipes after the host ends.
        client = HostClient(host_path=sys._base_executable)
        first = client.export(**export_args("out.mp4"))["pid"]
        for _ in range(scenario["given"]["requestsAfterEach"]):
            if not exits:
                os.kill(first, signal.SIGKILL if sys.platform != "win32" else signal.SIGTERM)
            if end != "killed":
                ended_unseen(first)
            try:
                following = client.export(**export_args("out.mp4"))["pid"]
            except Alpha2HostError as error:
                assert end == "killed" and error.code == "HOST_EXITED", error.message
                first = client.export(**export_args("out.mp4"))["pid"]
                continue
            assert following != first
            served = [line for line in lines_in(log) if line == f"request {first}"]
            assert len(served) == scenario["expect"]["firstHostRequests"]
            first = following


def defect_host(log: Path, marker: Path, report: dict[str, Any]) -> str:
    """The first host to start retires as a host does after a caught panic: it says it is draining
    with reason ``defect``, answers ``INTERNAL`` (with ``report`` as an export's failure), and
    refuses later requests until its input ends. Every other host answers with its pid."""
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()))
    if not os.path.exists({str(marker)!r}):
        open({str(marker)!r}, "w").write(str(os.getpid()))
    first = open({str(marker)!r}).read() == str(os.getpid())
    answered = []
    def answer(request):
        record({str(log)!r}, "request " + str(os.getpid()))
        if not first:
            send(request, ok=True, result={{"pid": os.getpid()}})
        elif answered:
            send(request, ok=False, error={{"code": "HOST_RETIRING", "message": "draining"}}, hostState="draining")
        else:
            answered.append(True)
            state("draining", "defect")
            error = {{"code": "INTERNAL", "message": "injected panic"}}
            if request["operation"] == "export":
                error["failure"] = {report!r}
            send(request, ok=False, error=error, hostState="draining")
    serve(answer)
    """


def run_defect(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    for operation in scenario["given"]["operations"]:
        run_defect_operation(scenario, operation, tmp_path / operation, fake_host)


def run_defect_operation(
    scenario: dict[str, Any], operation: str, root: Path, fake_host: FakeHost
) -> None:
    expect = scenario["expect"]
    log = root / "log"
    report = scenario["given"]["failureReport"]["export"]
    client = HostClient(host_path=fake_host(root, defect_host(log, root / "first", report)))

    def run() -> Any:
        if operation == "export":
            return client.export(**export_args("out.mp4"))
        return client.conform(project_document())

    with pytest.raises(Alpha2HostError) as caught:
        run()
    assert caught.value.code == expect["code"], caught.value.message
    assert caught.value.failure["code"] == expect["failureCode"]
    run()
    first, second = starts_in(log)
    assert first != second
    served = [line for line in lines_in(log) if line == f"request {first}"]
    assert len(served) == expect["firstHostRequests"]
    if expect["firstHostEnded"]:
        wait_until(lambda: pid_exited(first))
    client.close()


def reading_host(log: Path) -> str:
    """Reports ready, then exits once the first bytes of a request arrive."""
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()))
    begin()
    state()
    sys.stdin.buffer.read(8)
    os._exit(1)
    """


def run_dies_reading(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, reading_host(log), ready=False))
    project = {**project_document(), "name": "x" * scenario["given"]["projectNameBytes"]}
    with pytest.raises(Alpha2HostError) as caught:
        client.export(**export_args("out.mp4", project=project))
    assert caught.value.code == scenario["expect"]["code"], caught.value.message
    assert len(starts_in(log)) == scenario["expect"]["hosts"]


def abandoning_host(log: Path) -> str:
    """Answers, starts a sleeper that inherits its stdout and stderr, and exits."""
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()))
    import subprocess
    request, _ = read_request()
    send(request, ok=True, result={{"pid": os.getpid()}})
    sleeper = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(60)"], stdin=subprocess.DEVNULL
    )
    record({str(log)!r}, "sleeper " + str(sleeper.pid))
    os._exit(0)
    """


def run_grandchild_holds_the_pipes(
    scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost
) -> None:
    if sys.platform == "win32":
        pytest.skip("a POSIX scenario")
    log = tmp_path / "log"
    client = HostClient(
        host_path=fake_host(tmp_path, abandoning_host(log)),
        concurrency=scenario["given"]["concurrency"],
    )
    try:
        first = client.export(**export_args("out.mp4"))["pid"]
        wait_until(lambda: pid_exited(first))
        started = time.monotonic()
        following = client.export(**export_args("out.mp4"))["pid"]
        assert following != first
        assert (time.monotonic() - started) * 1000 < scenario["expect"]["withinMs"]
        started = time.monotonic()
        client.close()
        assert (time.monotonic() - started) * 1000 < scenario["expect"]["closeWithinMs"]
    finally:
        for line in lines_in(log):
            if line.startswith("sleeper "):
                try:
                    os.kill(int(line.split(" ")[1]), signal.SIGKILL)
                except ProcessLookupError:
                    pass


def run_arrival_order(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    given = scenario["given"]
    keep = given["thenKeepPending"]
    answer = "time.sleep(0.02)\n        send(request, ok=True, result={})"
    client = HostClient(
        host_path=fake_host(tmp_path, pooled_host(tmp_path / "log", answer)),
        concurrency=given["concurrency"],
    )
    completed: list[str] = []
    lock = threading.Lock()
    submitted = 0

    def run(label: str, decode: str) -> None:
        nonlocal submitted
        client.export(**export_args("out.mp4", video_decode=decode))
        with lock:
            completed.append(label)
            more = label == "stream" and submitted < keep["total"]
            if more:
                submitted += 1
        if more:
            pool.submit(run, "stream", keep["videoDecode"])

    with ThreadPoolExecutor(8) as pool:
        # Each request is queued before the next is made, so the order is the order of the calls.
        pool.submit(run, "first", given["first"])
        time.sleep(0.2)
        pool.submit(run, "second", given["second"])
        time.sleep(0.05)
        for _ in range(keep["depth"]):
            with lock:
                submitted += 1
            pool.submit(run, "stream", keep["videoDecode"])
        wait_until(lambda: len(completed) == 2 + keep["total"], 30)
    assert completed.index("second") + 1 == scenario["expect"]["secondCompletes"], completed


CACHE_HOST = """
served = 0
def answer(request):
    global served
    served += 1
    report = {"pid": os.getpid(), "served": served, "cache": os.environ.get("ALPHA2_HOST_CACHE_DIR")}
    if request["operation"] == "capabilities":
        report = {"status": "ready", **report}
    send(request, ok=True, result=report)
serve(answer)
"""


def run_cache_directory(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    given = scenario["given"]
    # The fake host starts from the calling directory, so that is where relative paths point.
    host_path = fake_host(tmp_path, CACHE_HOST)
    named = HostClient(host_path=host_path, cache_directory=given["option"])
    assert named.export(**export_args("out.mp4"))["cache"] == str(tmp_path / given["option"])
    saved = os.environ.get("ALPHA2_HOST_CACHE_DIR")
    os.environ["ALPHA2_HOST_CACHE_DIR"] = given["environment"]
    try:
        inherited = HostClient(host_path=host_path)
        cache = inherited.export(**export_args("out.mp4"))["cache"]
        assert cache == str(tmp_path / given["environment"])
    finally:
        if saved is None:
            del os.environ["ALPHA2_HOST_CACHE_DIR"]
        else:
            os.environ["ALPHA2_HOST_CACHE_DIR"] = saved


def run_install_generation(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    import bz2
    import dataclasses
    import hashlib
    import http.server

    from alpha2 import install as install_module

    client = HostClient(host_path=fake_host(tmp_path, CACHE_HOST))
    before = client.capabilities()
    target = alpha2.host_target()
    assert target is not None
    library = b"abracadabra"
    archive = bz2.compress(library)
    original = install_module._OPENH264_TARGETS[target]
    entry = dataclasses.replace(
        original,
        archive_sha256=hashlib.sha256(archive).hexdigest(),
        library_sha256=hashlib.sha256(library).hexdigest(),
    )

    class Mirror(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            found = self.path == f"/{entry.archive}"
            self.send_response(200 if found else 404)
            self.send_header("Content-Length", str(len(archive) if found else 0))
            self.end_headers()
            if found:
                self.wfile.write(archive)

        def log_message(self, *_: Any) -> None:
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Mirror)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    install_module._OPENH264_TARGETS[target] = entry
    try:
        installed = alpha2.install_openh264(
            cache_directory=tmp_path / "cache",
            download_url=f"http://127.0.0.1:{server.server_address[1]}",
        )
        assert installed.downloaded
    finally:
        install_module._OPENH264_TARGETS[target] = original
        server.shutdown()
    after = client.capabilities()
    exported = client.export(**export_args("out.mp4"))
    assert after["pid"] != before["pid"]
    assert exported["pid"] == after["pid"]
    assert len({before["pid"], after["pid"]}) == scenario["expect"]["hosts"]


def run_foreign_host(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    given, expect = scenario["given"], scenario["expect"]
    pids = tmp_path / "pids"
    host_path = fake_host(tmp_path, preamble_host(pids, given["stdout"]), ready=False)
    client = HostClient(host_path=host_path, host_start_timeout=given["startTimeoutMs"] / 1000)
    started = time.monotonic()
    error = refused(client)
    assert (time.monotonic() - started) * 1000 < expect["withinMs"]
    assert error.code == expect["code"], error.message
    for text in expect["messageContains"]:
        assert text in error.message
    if expect["namesBinary"]:
        assert host_path in error.message
    assert failure_class_of_code(error.code)[0] == "INVALID_INPUT"
    if expect["hostEnded"]:
        assert_hosts_ended(pids)


def run_host_version(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    expect = scenario["expect"]
    for index, host_version in enumerate(scenario["given"]["hostVersions"]):
        pids = tmp_path / f"pids-{index}"
        host_path = fake_host(
            tmp_path / str(index), versioned_host(pids, host_version), ready=False
        )
        error = refused(HostClient(host_path=host_path))
        assert error.code == expect["code"], error.message
        if expect["namesHostVersion"] and host_version is not None:
            assert host_version in error.message
        if expect["namesClientVersion"]:
            assert alpha2.__version__ in error.message
        if expect["namesBinary"]:
            assert host_path in error.message
        if expect["hostEnded"]:
            assert_hosts_ended(pids)


def run_start_timeout(scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost) -> None:
    given = scenario["given"]
    host = preamble_host(tmp_path / "pids", given["stdout"], given["stderr"])
    client = HostClient(
        host_path=fake_host(tmp_path, host, ready=False),
        host_start_timeout=given["startTimeoutMs"] / 1000,
    )
    error = refused(client)
    assert error.code == scenario["expect"]["code"], error.message
    for text in scenario["expect"]["stderrContains"]:
        assert text in (error.stderr or "")


class FloatLike:
    def __float__(self) -> float:
        return 0.25


class IndexLike:
    def __index__(self) -> int:
        return 4_000


@pytest.mark.parametrize(
    ("value", "sent"),
    [
        (float("nan"), None),
        (Decimal("0.5"), 0.5),
        (FloatLike(), 0.25),
    ],
    ids=["nan", "decimal", "float-like"],
)
def test_an_emit_goes_out_as_a_plain_number_or_fails_the_extract(
    tmp_path: Path, fake_host: FakeHost, value: Any, sent: float | None
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, emit_host(log)))
    extractor = EmitsAtEnd("sample", "level", IndexLike(), [value])
    if sent is None:
        with pytest.raises(Alpha2HostError) as caught:
            client.extract(project_document(), EXTRACT_REQUEST, extractor)
        assert caught.value.code == "EXTRACTOR_FAILED"
        assert "values[0] must be a finite number, not nan" in str(caught.value)
    else:
        client.extract(project_document(), EXTRACT_REQUEST, extractor)
        emits = [{"output": "level", "time": 4_000, "values": [sent]}]
        assert lines_in(log)[-1] == "ack emits " + json.dumps(emits)
    assert client.export(**export_args("next.mp4"))["pid"] == starts_in(log)[0]


def test_an_acknowledgement_that_cannot_be_encoded_fails_the_extract(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, emit_host(log)))
    # A lone surrogate is a str but has no UTF-8 form.
    extractor = EmitsAtEnd("word", "words", 0, 4_000, "\udc80")
    with pytest.raises(Alpha2HostError) as caught:
        client.extract(project_document(), EXTRACT_REQUEST, extractor)
    assert caught.value.code == "EXTRACTOR_FAILED"
    assert "the step's emits cannot be sent" in str(caught.value)
    assert client.export(**export_args("next.mp4"))["pid"] == starts_in(log)[0]


SCENARIO_DRIVERS: dict[str, Callable[[dict[str, Any], Path, FakeHost], None]] = {
    "request-paths": run_request_paths,
    "fork": run_fork,
    "abandon": run_abandon,
    "unsendable-emit": run_unsendable_emit,
    "foreign-host": run_foreign_host,
    "dead-idle-host": run_dead_idle_host,
    "dies-reading": run_dies_reading,
    "defect": run_defect,
    "grandchild-holds-the-pipes": run_grandchild_holds_the_pipes,
    "arrival-order": run_arrival_order,
    "cache-directory": run_cache_directory,
    "install-generation": run_install_generation,
    "host-version": run_host_version,
    "start-timeout": run_start_timeout,
}


@QUIET_FORK
@pytest.mark.parametrize(
    "scenario", CLIENT_SCENARIOS, ids=[scenario["id"] for scenario in CLIENT_SCENARIOS]
)
def test_client_behavior_scenario(
    scenario: dict[str, Any], tmp_path: Path, fake_host: FakeHost
) -> None:
    driver = SCENARIO_DRIVERS.get(scenario["kind"])
    assert driver is not None, f"no driver for scenario kind {scenario['kind']}"
    driver(scenario, tmp_path, fake_host)


def test_extract_sends_the_render_inputs_and_only_a_timeline_input_needs_every_effect_asset(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path / "host", paths_host(log)))
    project = corpus("host-parity-effects-project.json")
    descriptor = {
        "id": "example.levels",
        "version": 1,
        "input": {"media": "video", "from": ["source", "timeline"], "format": "rgba8"},
        "params_schema": {"type": "object"},
        "outputs": [],
    }

    class Extractor:
        def describe(self) -> dict[str, Any]:
            return descriptor

    def run(source: str, **fields: Any) -> None:
        read: dict[str, Any] = {"from": source, "range": {"start": 0, "end": 4000}}
        if source == "source":
            read["source_id"] = "video"
        request = {"extractor": "example.levels", "version": 1, "input": read, "outputs": {}}
        assets = {"video": "/media/v.mov", "still": "/media/s.png"}
        client.extract(project, request, Extractor(), assets=assets, **fields)

    run("source")
    run("timeline", effect_assets={"grade": "/media/grade.cube"}, effects=[])
    with pytest.raises(Alpha2HostError) as missing:
        run("timeline")
    assert missing.value.code == "MISSING_INPUT"
    assert "resource grade" in missing.value.message
    requests = [
        line["request"]
        for line in map(json.loads, lines_in(log))
        if line.get("request", {}).get("operation") == "extract"
    ]
    assert len(requests) == 2
    assert requests[0]["effectAssets"] == {}
    assert requests[1]["effectAssets"] == {"grade": os.path.abspath("/media/grade.cube")}
    assert {"frames", "resources", "effects", "fonts"} <= requests[1].keys()
