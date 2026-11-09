"""The host client against fake hosts: request checks, process lifecycle, errors and pooling."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import textwrap
import threading
import time
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
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
from alpha2.client import resolve_host_binary
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
    assert request["resources"] == resources
    assert request["memory"] == {"inFlightBytes": 1 << 20, "scale": 0.5}
    assert request["loudness"] == {"targetLufs": -14, "truePeakCeilingDbtp": -1}
    assert request["renderCache"] == {"directory": str(tmp_path / "cache"), "quotaBytes": 1 << 30}
    assert request["proxies"] == {"a": [{"path": "/a.proxy.mov", "colorPipeline": "tone_map"}]}
    assert request["videoQuality"] == {"bitrate": 8_000_000, "mode": "constant", "quantizer": 23}
    assert request["frameRate"] == {"num": 30, "den": 1}
    assert request["inputPolicy"] == "auto" and request["strategy"] == "render"
    for absent in ("container", "videoCodec", "audioCodec", "videoEncoder", "audioQuality"):
        assert absent not in request
    alpha2.export(**export_args(tmp_path / "without.mp4"), host_path=host_path)
    assert json.loads(recorded.read_text())["resources"] == {}


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
        record({str(log)!r}, "request " + request["output"])
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
        assert first.result() == {"output": "a.mp4"}
        assert third.result() == {"output": "c.mp4"}
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
            sys.stderr.write("while " + request["output"] + "\\n")
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
    assert request["purpose"] == "preview-proxy"
    assert request["assets"] == {"a": "/a.mov"}
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
    assert client.export(**export_args("slow.mp4")) == {"output": "slow.mp4"}


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
        "output": "ready.mp4"
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
    assert client.export(**export_args("out.mp4")) == {"output": "out.mp4"}
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
