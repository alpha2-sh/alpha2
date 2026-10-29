"""The host client against fake hosts: request checks, process lifecycle, errors and pooling."""

from __future__ import annotations

import os
import re
import sys
import threading
import time
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

import pytest
from conftest import corpus, export_args, lines_in, project_document

import alpha2
from alpha2 import (
    Alpha2AbortError,
    Alpha2GpuInitializationError,
    Alpha2HostError,
    Alpha2ProjectValidationError,
    HostClient,
)
from alpha2.client import resolve_host_binary
from alpha2.install import host_executable_name

FakeHost = Callable[[Path, str], str]

posix_only = pytest.mark.skipif(sys.platform == "win32", reason="POSIX signals")


def pid_exited(pid: int) -> bool:
    """Whether process ``pid`` has ended. Windows has no signal 0 (``os.kill`` would end the
    process), so there it is taken on the client's word."""
    if sys.platform == "win32":
        return True
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return False


def started_host(starts: Path) -> str:
    """A host that records each start's decode mode and answers any request with success."""
    return f"""
    record({str(starts)!r}, os.environ.get("ALPHA2_VIDEO_DECODE", "unset"))
    request, _ = read_request()
    if request["operation"] == "capabilities":
        send(request, ok=True, result={{"status": "ready", "capabilities": {{}}, "effectCatalog": {{}}}})
    else:
        send(request, ok=True, result={{"output": request.get("output")}})
    """


def answering_host(answer: str) -> str:
    """A host that answers its request with ``answer`` (Python source of the message's fields)."""
    return f"""
    request, _ = read_request()
    message = {answer}
    sys.stdout.write(json.dumps({{"protocolVersion": 1, "requestId": request["requestId"], **message}}) + "\\n")
    sys.stdout.flush()
    """


def wire_to_python(fields: dict[str, Any]) -> dict[str, Any]:
    """An export request's protocol fields as this client's keyword arguments."""

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
        # The corpus's project is a placeholder; a valid one keeps each case to the field it is
        # about.
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
    # A gap in an image sequence, path objects, and empty proxy lists are what the host reads.
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
        request, line = read_request()
        open({str(recorded)!r}, "w").write(line)
        send(request, ok=True, result={{"output": request["output"]}})
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
    # What is not named is left for the host to resolve from the container.
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
    # A blank variable is an unset one.
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
    # The cache holds the archive of this package's version, bundled before thin.
    version = alpha2.__version__
    target = alpha2.host_target()
    if target is not None:
        cache = tmp_path / "cache"
        # Windows is published as bundled only, so no thin archive is looked for there.
        for kind in ("bundled",) if sys.platform == "win32" else ("thin", "bundled"):
            binary = cache / f"alpha2-{version}-{target}-{kind}" / "bin" / host_executable_name()
            binary.parent.mkdir(parents=True)
            binary.write_text("")
            assert resolve_host_binary(cache_directory=cache) == binary


@posix_only
def test_a_file_that_cannot_be_executed_is_refused_before_it_would_start(tmp_path: Path) -> None:
    plain = tmp_path / "not-executable"
    plain.write_text("#!/bin/sh\n")
    plain.chmod(0o644)
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=plain).export(**export_args(tmp_path / "never.mp4"))
    assert caught.value.code == "HOST_NOT_EXECUTABLE"
    assert caught.value.failure["code"] == "INVALID_INPUT"


def test_cancellation_returns_only_after_the_host_process_exits(
    tmp_path: Path, fake_host: FakeHost
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
    if sys.platform != "win32":
        assert exit_marker.read_text() == "exited"


def test_ctrl_c_ends_the_host_before_the_interrupt_goes_on(
    tmp_path: Path, fake_host: FakeHost
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


def queue_host(log: Path, answer_seconds: float = 0, ignore_term: bool = False) -> str:
    """Records ``start <pid>`` and ``request <output>``, and answers an export after a while."""
    return f"""
    record({str(log)!r}, "start " + str(os.getpid()))
    if {ignore_term}:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    request, _ = read_request()
    record({str(log)!r}, "request " + request["output"])
    send(request, type="progress", phase="frames", completed=0, total=1)
    time.sleep({answer_seconds})
    send(request, ok=True, result={{"output": request["output"]}})
    """


def test_an_export_cancelled_while_it_waits_for_a_slot_never_starts(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    client = HostClient(host_path=fake_host(tmp_path, queue_host(log, answer_seconds=0.4)))
    cancel = threading.Event()
    with ThreadPoolExecutor(3) as pool:
        first = pool.submit(client.export, **export_args("a.mp4"))
        time.sleep(0.1)
        started = time.monotonic()
        second = pool.submit(client.export, **export_args("b.mp4", cancel=cancel))
        time.sleep(0.02)
        third = pool.submit(client.export, **export_args("c.mp4"))
        time.sleep(0.05)
        cancel.set()
        with pytest.raises(Alpha2AbortError):
            second.result()
        assert time.monotonic() - started < 0.35, "the cancel waited for the slot"
        assert first.result() == {"output": "a.mp4"}
        assert third.result() == {"output": "c.mp4"}
    requests = [line for line in lines_in(log) if line.startswith("request")]
    assert requests == ["request a.mp4", "request c.mp4"]


@posix_only
def test_a_host_that_stays_up_through_sigterm_is_killed(
    tmp_path: Path, fake_host: FakeHost
) -> None:
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
    assert 4.9 <= waited < 8, f"returned {waited:.2f} s after the cancel"
    assert pid_exited(int(lines_in(log)[0].split(" ")[1]))


def test_a_cancel_seen_once_the_host_has_started_ends_it_before_the_request_is_written(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    log = tmp_path / "log"
    host_path = fake_host(tmp_path, queue_host(log))

    class LateEvent(threading.Event):
        """Set from its third look on: after the slot is taken and the host has started."""

        looks = 0

        def is_set(self) -> bool:
            self.looks += 1
            return self.looks >= 3

    cancel = LateEvent()
    with pytest.raises(Alpha2AbortError):
        HostClient(host_path=host_path).export(**export_args("out.mp4", cancel=cancel))
    assert cancel.looks == 3
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
        record({str(modes)!r}, os.environ.get("ALPHA2_VIDEO_DECODE", "unset"))
        try:
            descriptor = os.open({str(lock)!r}, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.close(descriptor)
        except FileExistsError:
            open({str(overlap)!r}, "w").write("overlap")
        request, _ = read_request()
        time.sleep(0.1)
        os.remove({str(lock)!r})
        send(request, ok=True, result={{"output": request.get("output"), "status": "ready"}})
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


@posix_only
def test_stderr_written_after_the_answer_is_kept(tmp_path: Path, fake_host: FakeHost) -> None:
    host_path = fake_host(
        tmp_path,
        """
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        request, _ = read_request()
        sys.stderr.write("x" * 40_000 + "\\n")
        sys.stderr.flush()
        send(request, ok=False, error={"code": "ALPHA2_ERROR", "message": "readback failed"})
        time.sleep(0.1)
        sys.stderr.write("alpha2-host uncaptured GPU out-of-memory error: staging\\n")
        sys.stderr.flush()
        sys.exit(1)
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "ALPHA2_ERROR"
    for stderr in (caught.value.stderr, caught.value.failure["stderr"]):
        assert stderr is not None and stderr.endswith("out-of-memory error: staging\n")


def test_a_line_on_the_hosts_stdout_that_is_not_a_message_is_kept_as_diagnostic_output(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    banner = "Validation Error: [ VUID-StandaloneSpirv-None-10684 ] vkCreateShaderModule"

    def host(answer: str) -> str:
        return f"""
        sys.stdout.write({banner!r} + "\\n\\n")
        sys.stdout.flush()
        request, _ = read_request()
        sys.stdout.write(" " + {banner!r} + "\\n" + {answer!r}.replace("REQUEST", request["requestId"]) + "\\n")
        sys.stdout.flush()
        """

    ready = HostClient(
        host_path=fake_host(
            tmp_path / "ready",
            host(
                '{"protocolVersion":1,"requestId":"REQUEST","ok":true,"result":{"status":"ready"}}'
            ),
        )
    ).capabilities()
    assert ready["status"] == "ready"
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(
            host_path=fake_host(
                tmp_path / "failed",
                host(
                    '{"protocolVersion":1,"requestId":"REQUEST","ok":false,'
                    '"error":{"code":"ALPHA2_ERROR","message":"failed"}}'
                ),
            )
        ).export(**export_args(tmp_path / "out.mp4"))
    assert caught.value.code == "ALPHA2_ERROR"
    assert caught.value.stderr is not None and banner in caught.value.stderr
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=fake_host(tmp_path / "broken", host("{not json"))).capabilities()
    assert caught.value.code == "INVALID_RESPONSE"


@posix_only
def test_a_host_that_dies_mid_export_is_host_exited_with_no_cause_claimed(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host_path = fake_host(
        tmp_path,
        """
        request, _ = read_request()
        send(request, type="progress", phase="finalizing", completed=9, total=9)
        sys.stderr.write("last words\\n")
        sys.stderr.flush()
        os.kill(os.getpid(), signal.SIGKILL)
        """,
    )
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=host_path).export(**export_args(tmp_path / "out.mp4"))
    error = caught.value
    assert error.code == "HOST_EXITED"
    assert error.signal == "SIGKILL"
    assert error.exit_code is None
    assert error.failure["code"] == "HOST_EXITED"
    assert error.failure["resource"] == "unknown"
    assert error.failure["phase"] == "mux"
    assert error.failure["origin"] == "python"
    assert error.failure["signal"] == "SIGKILL"
    assert "last words" in error.failure["stderr"]


def test_a_host_the_loader_refuses_to_start_is_host_load_failed(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    words = (
        "/opt/alpha2/bin/alpha2-host: error while loading shared libraries: libavcodec.so.62: "
        "cannot open shared object file: No such file or directory"
    )
    client = HostClient(
        host_path=fake_host(tmp_path, f"sys.stderr.write({words + chr(10)!r})\nsys.exit(127)\n")
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


def stalling_host(runs: Path, resource: str = "gpu") -> str:
    """Fails with DECODE_STALLED unless it decodes in software; records each decode mode."""
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
    """Records ``<operation> <mode>`` per request; answers capabilities at once."""
    return f"""
    request, _ = read_request()
    mode = os.environ.get("ALPHA2_VIDEO_DECODE", "unset")
    record({str(log)!r}, request["operation"] + " " + mode)
    if request["operation"] == "export":
        time.sleep({export_seconds})
        send(request, ok=True, result={{"output": request["output"]}})
        sys.exit(0)
    failed = {str(log)!r} + ".failed"
    if {fail_first} and not os.path.exists(failed):
        open(failed, "w").close()
        send(request, ok=False, error={{"code": "ALPHA2_ERROR", "message": "probe failed"}})
    else:
        send(request, ok=True, result={{"status": "ready", "capabilities": {{"decode": {{"mode": mode}}}}, "effectCatalog": {{}}}})
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
        started = time.monotonic()
        probe = pool.submit(client.capabilities, cancel=cancel)
        time.sleep(0.05)
        cancel.set()
        with pytest.raises(Alpha2AbortError):
            probe.result()
        assert time.monotonic() - started < 0.5, "the withdrawn call waited for the export"
        exporting.result()
    # Long enough for a probe still queued behind the export to have started.
    time.sleep(0.3)
    assert lines_in(log) == ["export unset"]


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
    """Records ``<operation> <decode mode> <renderer>`` per request, from its environment, and
    answers at once: capabilities naming its renderer, or an export result."""
    return f"""
    request, _ = read_request()
    decode = os.environ.get("ALPHA2_VIDEO_DECODE", "unset")
    renderer = os.environ.get("ALPHA2_RENDERER", "unset")
    record({str(log)!r}, request["operation"] + " " + decode + " " + renderer)
    if request["operation"] == "capabilities":
        send(request, ok=True, result={{"status": "ready", "renderer": renderer, "rendererReason": None, "capabilities": {{}}, "effectCatalog": {{}}}})
    else:
        send(request, ok=True, result={{"output": request.get("output")}})
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
    # Unset, the renderer is the one this process's environment names when the probe starts.
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
        sys.stdin.readline()
        sys.stdout.write(json.dumps({"protocolVersion": 1, "requestId": "unknown", "ok": False,
            "error": {"code": "INVALID_REQUEST", "message": "the request line is 40000000 bytes; the maximum is 134217728"}}) + "\\n")
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
        sys.stdout.write(json.dumps({"protocolVersion": 1, "type": "fatal",
            "error": {"code": "GPU_INIT_FAILED", "message": "no adapter"}}) + "\\n")
        sys.stdout.flush()
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
    tmp_path: Path, fake_host: FakeHost
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


def test_a_host_that_cannot_make_its_gpu_device_is_a_gpu_initialization_error(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    cases = [
        (
            "fatal",
            """
            sys.stderr.write("alpha2-host: no adapter\\n"); sys.stderr.flush()
            sys.stdout.write(json.dumps({"protocolVersion": 1, "type": "fatal", "error": {"code": "GPU_INIT_FAILED", "message": "could not acquire a GPU adapter"}}) + "\\n")
            sys.stdout.flush()
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
