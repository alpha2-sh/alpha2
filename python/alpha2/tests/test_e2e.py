"""The client against a real ``alpha2-host``: the scenarios ``@alpha2-sh/node``'s e2e test runs.

The host is ``ALPHA2_HOST_BINARY``, else ``target/debug/alpha2-host`` (``cargo build -p
alpha2-host``); without one the module is skipped. The host loads its FFmpeg runtime from the
environment, as it does for the Node e2e test.
"""

from __future__ import annotations

import copy
import json
import os
import re
import signal
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT, corpus

from alpha2 import (
    Alpha2AbortError,
    Alpha2ConformError,
    Alpha2GpuInitializationError,
    Alpha2HostError,
    Alpha2ProjectValidationError,
    HostClient,
)

HOST = Path(
    os.environ.get("ALPHA2_HOST_BINARY")
    or ROOT / "target" / "debug" / ("alpha2-host.exe" if sys.platform == "win32" else "alpha2-host")
)
if not HOST.is_file():
    pytest.skip(
        f"no alpha2-host at {HOST}; build it or set ALPHA2_HOST_BINARY", allow_module_level=True
    )

PROJECT: dict[str, Any] = json.loads((ROOT / "tests/cases/host-parity-project.json").read_text())
ASSET = ROOT / "tests/corpus/cross_topology_parity.png"
REQUIRE_H264 = os.environ.get("ALPHA2_REQUIRE_H264") == "1"


@pytest.fixture(scope="module")
def client() -> HostClient:
    return HostClient(host_path=HOST)


@pytest.fixture(scope="module")
def encode(client: HostClient) -> dict[str, Any]:
    return client.capabilities()["capabilities"]["encode"]  # type: ignore[no-any-return]


def h264_available(encode: dict[str, Any]) -> bool:
    gpu = encode.get("gpuOutputPath") or {}
    return any(codec["id"] == "h264" for codec in encode["video"]) or (
        gpu.get("available") is True and gpu.get("videoCodec") == "h264"
    )


def image_export(output: Path, **options: Any) -> dict[str, Any]:
    """An export of the parity project's one image, without audio, at 24 fps."""
    return {
        "project": PROJECT,
        "output": output,
        "assets": {"image-source": ASSET},
        "start_ticks": 0,
        "end_ticks": 5_000,
        "frame_rate": (24, 1),
        "include_audio": False,
        **options,
    }


def sized(width: int, height: int) -> dict[str, Any]:
    return {**PROJECT, "settings": {**PROJECT["settings"], "width": width, "height": height}}


def partial_of(output: Path) -> Path:
    return output.with_name(f"{output.stem}.alpha2-partial{output.suffix}")


def assert_declared_performance(performance: dict[str, Any], path: str) -> None:
    """``performance`` has a scalar or None per counter, besides the declared non-scalar ones."""
    declared = {"hardwareFallbackReasons", "encodeChannelDepthSamples", "gpuEncodeBudget"}
    for key, value in performance.items():
        if key not in declared:
            assert value is None or isinstance(value, (int, float, bool, str)), key
    for key in declared:
        assert (key in performance) == (path != "packet-copy"), key
    if path == "readback":
        assert performance["gpuEncodeBudget"] is None
    if path == "gpu-resident":
        assert performance["gpuEncodeBudget"] is not None


def test_the_client_reports_the_hosts_encode_capabilities(client: HostClient) -> None:
    capabilities = client.capabilities()
    assert capabilities["status"] == "ready"
    encode = capabilities["capabilities"]["encode"]
    # The containers are the engine's policy, as the shared corpus pins it.
    assert encode["containers"] == [
        {
            key: entry[key]
            for key in (
                "id",
                "extensions",
                "video",
                "audio",
                "defaultVideoCodec",
                "defaultAudioCodec",
            )
        }
        for entry in corpus("export-container-corpus.json")["containers"]
    ]
    assert any(codec["id"] == "mpeg4-part2" for codec in encode["video"])
    h264 = [codec for codec in encode["video"] if codec["id"] == "h264"]
    if REQUIRE_H264:
        assert h264_available(encode)
    assert len([codec for codec in h264 if codec.get("preferred")]) == (1 if h264 else 0)
    for codec in encode["video"]:
        assert codec["encoderClass"] in ("software", "hardware", "hybrid")
    assert capabilities["effectCatalog"]["effects"]
    assert isinstance(capabilities["adapter"], str) and capabilities["adapter"]
    assert client.capabilities() is capabilities


def test_the_client_exports_a_project_through_the_host_binary(
    client: HostClient, tmp_path: Path
) -> None:
    output = tmp_path / "export.mp4"
    progress: list[Any] = []
    result = client.export(
        **image_export(output, video_codec="mpeg4-part2", on_progress=progress.append)
    )
    assert output.read_bytes()[4:8] == b"ftyp"
    assert progress, "the host must report export progress"
    for report in progress:
        assert 0 <= report.completed <= report.total
    assert progress[-1].completed == progress[-1].total
    assert progress[-1].phase == "finalizing"
    assert result["frames"] == 1
    assert result["effectiveRange"] == {"startTicks": 0, "endTicks": 5_000}
    assert result["color"]["workingSpace"] == "linear_bt709_sdr"
    assert result["color"]["output"] == "bt709_sdr"
    assert result["color"]["intermediateFormat"] in ("rgba16float", "rgba8unorm")
    assert isinstance(result["performance"]["frameSubmitMs"], (int, float))
    assert_declared_performance(result["performance"], result["videoOutputPath"])
    assert result["decodeDiagnostics"] == []


def test_a_failed_readback_reaches_the_client_classified_with_the_hosts_evidence(
    client: HostClient, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME", "1")
    with pytest.raises(Alpha2HostError) as caught:
        client.export(
            **image_export(
                tmp_path / "failed.mp4",
                end_ticks=15_000,
                video_codec="mpeg4-part2",
                input_policy="cpu-readback",
            )
        )
    error = caught.value
    assert error.code == "ALPHA2_ERROR"
    assert "GPU texture readback failed: map-failed" in error.message
    failure = error.failure
    assert (failure["code"], failure["phase"], failure["stage"]) == (
        "READBACK_FAILED",
        "readback",
        "map-failed",
    )
    assert failure["frameIndex"] == 1
    assert failure["origin"] == "native-host"
    assert failure["diagnostics"]["videoOutputPath"] == "readback"
    assert failure["diagnostics"]["request"]["startTicks"] == "0"
    assert failure["diagnostics"]["gpu"]["deviceLost"] is False
    assert isinstance(error.stderr, str)


def test_the_smallest_budget_that_holds_one_frame_writes_the_same_file_as_the_default(
    client: HostClient, tmp_path: Path
) -> None:
    def run(name: str, memory: dict[str, int] | None = None) -> tuple[dict[str, Any], bytes]:
        output = tmp_path / name
        result = client.export(
            **image_export(
                output,
                end_ticks=40_000,
                video_codec="mpeg4-part2",
                input_policy="cpu-readback",
                memory=memory,
            )
        )
        return result, output.read_bytes()

    roomy, roomy_bytes = run("roomy.mp4")
    assert roomy["memoryPolicy"]["inFlightBytes"]["source"] == "default"
    performance = roomy["performance"]
    # One readback, the frame being packed, and the frame being encoded: a rendezvous channel.
    least = performance["stagingBytesPerFrame"] + 2 * performance["packedBytesPerFrame"]
    tight, tight_bytes = run(
        "tight.mp4", {"in_flight_bytes": least, "decode_queue_bytes": 64 * 1024 * 1024}
    )
    assert tight["memoryPolicy"]["inFlightBytes"] == {
        **tight["memoryPolicy"]["inFlightBytes"],
        "bytes": least,
        "source": "request",
    }
    assert tight["performance"]["inFlightDepth"] == 1
    assert tight["performance"]["peakReadbackBytes"] <= least
    assert tight_bytes == roomy_bytes
    with pytest.raises(Alpha2HostError) as caught:
        run("starved.mp4", {"in_flight_bytes": least - 1})
    assert caught.value.code == "RESOURCE_LIMIT"
    assert caught.value.failure["code"] == "RESOURCE_LIMIT"


def test_a_retry_after_a_failure_takes_the_frames_already_rendered(
    client: HostClient, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    frames = 12

    def export_to(
        name: str, render_cache: dict[str, Any] | None = None
    ) -> tuple[dict[str, Any], bytes]:
        output = tmp_path / name
        result = client.export(
            **image_export(
                output,
                end_ticks=frames * 120_000 // 24,
                video_codec="mpeg4-part2",
                input_policy="cpu-readback",
                render_cache=render_cache,
            )
        )
        return result, output.read_bytes()

    clean, clean_bytes = export_to("clean.mp4")
    for fail_at in (1, frames - 1):
        cache = {
            "directory": tmp_path / f"cache-{fail_at}",
            "quota_bytes": 64 << 20,
            "backlog": "wait",
        }
        monkeypatch.setenv("ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME", str(fail_at))
        monkeypatch.setenv("ALPHA2_EXPORT_FAIL_ONCE_MARKER", str(tmp_path / f"failed-{fail_at}"))
        with pytest.raises(Alpha2HostError) as caught:
            export_to(f"interrupted-{fail_at}.mp4", cache)
        assert caught.value.failure["code"] == "READBACK_FAILED"
        assert caught.value.failure["frameIndex"] == fail_at
        resumed, resumed_bytes = export_to(f"resumed-{fail_at}.mp4", cache)
        assert resumed["renderCache"]["hits"] == fail_at
        assert resumed["renderCache"]["writes"] == frames - fail_at
        assert resumed["frames"] == clean["frames"]
        assert resumed_bytes == clean_bytes, f"resumed at {fail_at}"
        monkeypatch.delenv("ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME")
        monkeypatch.delenv("ALPHA2_EXPORT_FAIL_ONCE_MARKER")


def test_a_second_export_of_the_same_snapshot_takes_its_frames_from_the_render_cache(
    client: HostClient, tmp_path: Path
) -> None:
    def exported(name: str) -> dict[str, Any]:
        result = client.export(
            **image_export(
                tmp_path / name,
                end_ticks=20_000,
                video_codec="mpeg4-part2",
                render_cache={"directory": tmp_path / "cache", "backlog": "wait"},
            )
        )
        return result["renderCache"]  # type: ignore[no-any-return]

    first = exported("first.mp4")
    assert first["status"] == "used"
    assert first["writes"] == 4
    second = exported("second.mp4")
    assert (second["hits"], second["writes"]) == (4, 0)
    assert (tmp_path / "first.mp4").read_bytes() == (tmp_path / "second.mp4").read_bytes()


@pytest.mark.skipif(sys.platform != "linux", reason="hiding every GPU this way is Linux's")
def test_without_a_gpu_the_host_still_conforms_and_a_probe_is_a_gpu_initialization_error(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("VK_ICD_FILENAMES", "/nonexistent")
    monkeypatch.setenv("WGPU_BACKEND", "vulkan")
    # A client of its own: this one's capability report is not the shared client's.
    gpuless = HostClient(host_path=HOST)
    conformed = gpuless.conform(PROJECT, assets={"image-source": ASSET})
    assert conformed["report"]["ready"] is True
    with pytest.raises(Alpha2GpuInitializationError) as caught:
        gpuless.capabilities()
    assert caught.value.code == "GPU_INIT_FAILED"
    assert isinstance(caught.value.stderr, str)


def test_the_host_conforms_media_before_it_renders_and_says_what_an_export_read(
    client: HostClient, tmp_path: Path
) -> None:
    ready = client.conform(PROJECT, assets={"image-source": ASSET})
    assert ready["report"]["ready"] is True
    assert ready["catalog"]["assets"][0]["original"]["state"] == "online"
    assert ready["report"]["sources"][0]["selection"] == {"media": "original"}

    refused = tmp_path / "refused.mp4"
    with pytest.raises(Alpha2ConformError) as caught:
        client.export(
            **image_export(
                refused,
                assets={"image-source": tmp_path / "moved-away.png"},
                video_codec="mpeg4-part2",
            )
        )
    assert caught.value.report["ready"] is False
    assert caught.value.report["sources"][0]["issues"] == [{"kind": "offline", "blocking": True}]
    assert not refused.exists()

    result = client.export(
        **image_export(
            tmp_path / "proxied.mp4",
            assets={"image-source": tmp_path / "moved-away.png"},
            proxies={"image-source": [{"path": ASSET}]},
            media="proxies",
            video_codec="mpeg4-part2",
        )
    )
    assert result["media"]["image-source"] == {"media": "proxy", "id": "image-source#proxy0"}


def matroska_doc_type(data: bytes) -> str | None:
    if data[:4] != bytes([0x1A, 0x45, 0xDF, 0xA3]):
        return None
    header = data[:64].decode("latin-1")
    if "webm" in header:
        return "webm"
    return "matroska" if "matroska" in header else None


def test_an_export_writes_the_container_its_output_names(
    client: HostClient, encode: dict[str, Any], tmp_path: Path
) -> None:
    vp9 = any(codec["id"] == "vp9" for codec in encode["video"])
    for name in ("export.mov", "export.mkv", "export.webm"):
        output = tmp_path / name
        webm = name.endswith(".webm")
        if webm and not vp9:
            # A runtime without a VP9 encoder refuses WebM's default before anything opens.
            with pytest.raises(Alpha2HostError) as caught:
                client.export(**image_export(output))
            assert caught.value.code == "INVALID_INPUT"
            continue
        # MOV and Matroska take the reference encoder every runtime has; WebM its own default.
        result = client.export(
            **image_export(output, **({} if webm else {"video_codec": "mpeg4-part2"}))
        )
        data = output.read_bytes()
        if name.endswith(".mov"):
            assert data[4:12] == b"ftypqt  "
            assert result["container"] == "mov"
        elif webm:
            assert matroska_doc_type(data) == "webm"
            assert (result["container"], result["videoCodec"]) == ("webm", "vp9")
        else:
            assert matroska_doc_type(data) == "matroska"
            assert result["container"] == "mkv"
        assert not partial_of(output).exists()
    mismatched = tmp_path / "mismatched.mp4"
    with pytest.raises(Alpha2HostError, match=r"a mov output must end in \.mov") as caught:
        client.export(**image_export(mismatched, container="mov"))
    assert caught.value.code == "INVALID_INPUT"
    assert not mismatched.exists()


def test_an_export_that_names_no_codec_gets_h264_or_is_refused_without_it(
    client: HostClient, encode: dict[str, Any], tmp_path: Path
) -> None:
    output = tmp_path / "default-codec.mp4"
    # The 64x48 parity fixture is below every NVENC encoder's minimum frame size.
    arguments = image_export(output, project=sized(256, 192))
    if not h264_available(encode) and not REQUIRE_H264:
        # No silent downgrade to MPEG-4 Part 2: the export fails and writes nothing.
        with pytest.raises(Alpha2HostError, match="no usable h264 encoder") as caught:
            client.export(**arguments)
        assert caught.value.code == "INVALID_INPUT"
        assert caught.value.failure["code"] == "INVALID_INPUT"
        assert not output.exists()
        return
    result = client.export(**arguments)
    assert result["videoCodec"] == "h264"
    assert isinstance(result["videoEncoder"], str)
    assert result["videoEncoderClass"] in ("software", "hardware", "hybrid")
    assert isinstance(result["videoEncoderFallbacks"], list)


def test_an_invalid_project_or_range_is_rejected_before_a_host_process_is_started(
    client: HostClient, tmp_path: Path
) -> None:
    with pytest.raises(Alpha2ProjectValidationError):
        client.export(
            **image_export(tmp_path / "never.mp4", project={**PROJECT, "schema_version": 999})
        )
    with pytest.raises(Alpha2HostError, match="range"):
        client.export(**image_export(tmp_path / "never.mp4", start_ticks=5_000, end_ticks=5_000))


def test_video_quality_reaches_the_encoder(
    client: HostClient, encode: dict[str, Any], tmp_path: Path
) -> None:
    if not h264_available(encode) and not REQUIRE_H264:
        pytest.skip("MPEG-4 Part 2 at 256x192 is at its quality ceiling: this needs H.264")
    project = json.loads((ROOT / "tests/cases/host-video-export-project.json").read_text())
    project["settings"] = {
        **project["settings"],
        "width": 256,
        "height": 192,
        "frame_rate": {"num": 30, "den": 1},
    }
    exported = {}
    for level in ("low", "very-high"):
        output = tmp_path / f"{level}.mp4"
        result = client.export(
            project,
            output,
            assets={"video-source": ROOT / "tests/corpus/long-gop.mp4"},
            end_ticks=120_000,
            frame_rate=30,
            include_audio=False,
            video_quality=level,
        )
        assert result["videoRateControl"]["level"] == level
        assert result["videoRateControl"]["source"] == "request"
        exported[level] = (result["videoRateControl"]["bitrate"], output.stat().st_size)
    assert exported["low"][0] < exported["very-high"][0]
    assert exported["low"][1] < exported["very-high"][1], exported


def copy_project(source_start: int, duration: int, has_audio: bool = False) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "id": "copy",
        "name": "Copy",
        "settings": {
            "width": 128,
            "height": 96,
            "frame_rate": {"num": 30, "den": 1},
            "sample_rate": 48_000,
        },
        "sources": [
            {"id": "video", "locator": "copy.mp4", "kind": "video", "has_audio": has_audio}
        ],
        "timeline": {
            "tracks": [
                {
                    "id": "v",
                    "name": "V",
                    "kind": "video",
                    "clips": [
                        {
                            "id": "clip",
                            "source_id": "video",
                            "timeline_range": {"start": 0, "end": duration},
                            "source_range": {"start": source_start, "end": source_start + duration},
                        }
                    ],
                }
            ]
        },
    }


def test_a_range_that_is_its_source_untouched_is_copied_and_one_that_cannot_be_is_refused(
    client: HostClient, tmp_path: Path
) -> None:
    second = 120_000
    assets = {"video": ROOT / "tests/corpus/h264-bt709-copy.mp4"}
    # No encoder takes part, so this holds on a host that cannot encode H.264 at all.
    result = client.export(
        copy_project(second, second),
        tmp_path / "gop.mp4",
        assets=assets,
        end_ticks=second,
        include_audio=False,
        strategy="require-copy",
    )
    assert result["videoOutputPath"] == "packet-copy"
    assert_declared_performance(result["performance"], "packet-copy")
    assert (result["strategy"], result["requestedStrategy"]) == ("copy", "require-copy")
    assert result["frames"] == 30
    assert result["videoRateControl"] is None
    assert result["performance"]["copiedVideoPackets"] == 30

    result = client.export(
        copy_project(second, second, has_audio=True),
        tmp_path / "gop-audio.mp4",
        assets=assets,
        end_ticks=second,
        strategy="require-copy",
    )
    assert result["videoOutputPath"] == "packet-copy"
    assert result["audio"] is True
    assert result["audioStrategy"] == "encode"

    with pytest.raises(Alpha2HostError, match="IDR") as caught:
        client.export(
            copy_project(second // 2, second),
            tmp_path / "mid-gop.mp4",
            assets=assets,
            end_ticks=second,
            include_audio=False,
            strategy="require-copy",
        )
    assert caught.value.code == "COPY_INELIGIBLE"
    assert not (tmp_path / "mid-gop.mp4").exists()


def held_image_export(output: Path, frames: int, **options: Any) -> dict[str, Any]:
    held = copy.deepcopy(PROJECT)
    clip = held["timeline"]["tracks"][0]["clips"][0]
    clip["timeline_range"]["end"] = frames * 5_000
    clip["source_range"]["end"] = frames * 5_000
    return image_export(
        output, project=held, end_ticks=frames * 5_000, video_codec="mpeg4-part2", **options
    )


# Long enough that nothing below waits for it to finish: it is stopped first.
LONG_FRAMES = 200_000


def test_an_export_cancelled_mid_frame_leaves_neither_output_nor_partial_and_exports_again(
    client: HostClient, tmp_path: Path
) -> None:
    output = tmp_path / "aborted.mp4"
    partial = partial_of(output)
    cancel = threading.Event()
    writing: list[bool] = []

    def progress(report: Any) -> None:
        if report.phase == "frames" and report.completed > 0 and not cancel.is_set():
            writing.append(partial.exists())
            cancel.set()

    with pytest.raises(Alpha2AbortError):
        client.export(**held_image_export(output, LONG_FRAMES, cancel=cancel, on_progress=progress))
    assert writing == [True], "the export was writing its partial file when cancelled"
    assert not output.exists()
    # On Windows terminate() is TerminateProcess, which leaves the partial file for the next export.
    assert partial.exists() == (sys.platform == "win32")
    client.export(**held_image_export(output, 24))
    assert output.exists() and not partial.exists()


@pytest.mark.skipif(sys.platform == "win32", reason="SIGKILL")
def test_a_host_killed_mid_export_leaves_a_partial_the_next_export_takes_over(
    client: HostClient, tmp_path: Path
) -> None:
    output = tmp_path / "killed.mp4"
    partial = partial_of(output)
    project_file = tmp_path / "held.json"
    project_file.write_text(json.dumps(held_image_export(output, LONG_FRAMES)["project"]))
    child = subprocess.Popen(
        [
            str(HOST),
            "export",
            "--project",
            str(project_file),
            "--asset",
            f"image-source={ASSET}",
            "--out",
            str(output),
            "--end-ticks",
            str(LONG_FRAMES * 5_000),
            "--frame-rate",
            "24/1",
            "--no-audio",
            "--video-codec",
            "mpeg4-part2",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    assert child.stderr is not None
    seen = b""
    while not re.search(rb"alpha2-host export: frames [1-9]", seen):
        chunk = os.read(child.stderr.fileno(), 65_536)
        if not chunk:
            pytest.fail(f"the host ended first: {seen[-500:]!r}")
        seen += chunk
    child.send_signal(signal.SIGKILL)
    child.wait()
    assert partial.exists(), "SIGKILL leaves the partial file"
    client.export(**held_image_export(output, 24))
    assert not partial.exists()
    assert output.read_bytes()[4:8] == b"ftyp"


def test_a_second_export_to_an_output_a_running_export_is_writing_is_refused(
    tmp_path: Path,
) -> None:
    output = tmp_path / "shared.mp4"
    partial = partial_of(output)
    both = HostClient(host_path=HOST, concurrency=2)
    cancel = threading.Event()
    running = threading.Event()
    outcome: list[BaseException] = []

    def first() -> None:
        try:
            both.export(
                **held_image_export(
                    output,
                    LONG_FRAMES,
                    cancel=cancel,
                    on_progress=lambda p: (
                        running.set() if p.phase == "frames" and p.completed else None
                    ),
                )
            )
        except BaseException as error:  # noqa: BLE001 - examined below
            outcome.append(error)

    thread = threading.Thread(target=first)
    thread.start()
    try:
        assert running.wait(60)
        with pytest.raises(Alpha2HostError) as caught:
            both.export(**held_image_export(output, 24))
        assert caught.value.code == "OUTPUT_IN_USE", caught.value.message
        assert str(partial) in caught.value.message
        # The same request meets the same running export: a retry policy spends no attempts on it.
        assert caught.value.failure["code"] == "INVALID_INPUT"
        assert partial.exists(), "the refusal left the running export's file"
    finally:
        cancel.set()
        thread.join()
    assert isinstance(outcome[0], Alpha2AbortError)
    assert partial.exists() == (sys.platform == "win32")
    assert not output.exists()


def even(value: int) -> int:
    return value if value % 2 == 0 else value + 1


def export_size(
    client: HostClient, tmp_path: Path, width: int, height: int, name: str, **options: Any
) -> dict[str, Any]:
    return client.export(**image_export(tmp_path / name, project=sized(width, height), **options))


def test_the_client_can_tell_in_advance_whether_an_export_will_be_gpu_resident(
    client: HostClient, encode: dict[str, Any], tmp_path: Path
) -> None:
    gpu = encode["gpuOutputPath"]
    assert isinstance(gpu["available"], bool)
    assert gpu["requiresEvenDimensions"] is True
    if not gpu["available"]:
        assert isinstance(gpu["reason"], str) and gpu["reason"], "an unavailable path says why"
        return
    assert gpu["reason"] is None
    assert gpu["videoCodec"] == "h264"
    for key in (
        "minimumWidth",
        "minimumHeight",
        "maximumWidth",
        "maximumHeight",
        "widthAlignment",
        "heightAlignment",
    ):
        assert isinstance(gpu[key], int) and gpu[key] > 0, key
    # Below the minimum a runtime with a software H.264 encoder falls back to readback and one
    # without fails naming the encoder; neither may claim the GPU path.
    below = (
        even(max(2, gpu["minimumWidth"] - gpu["widthAlignment"])),
        even(max(2, gpu["minimumHeight"] - gpu["heightAlignment"])),
    )
    has_below = below[0] < gpu["minimumWidth"] or below[1] < gpu["minimumHeight"]
    if has_below:
        try:
            result = export_size(client, tmp_path, *below, "below-minimum.mp4")
            assert result["videoOutputPath"] == "readback"
        except Alpha2HostError as error:
            assert "encoder" in error.message.lower(), error.message
    inside = (even(max(gpu["minimumWidth"], 256)), even(max(gpu["minimumHeight"], 192)))
    result = export_size(client, tmp_path, *inside, "inside-extent.mp4")
    assert result["videoOutputPath"] == "gpu-resident"
    assert result["videoEncoder"] == gpu["encoder"]
    assert result["inputPolicy"] == "auto"
    assert result["performance"]["readbackCopyBytes"] == 0
    for options in ({"video_encoder": gpu["encoder"]}, {"input_policy": "gpu-required"}):
        pinned = export_size(client, tmp_path, *inside, "required.mp4", **options)
        assert pinned["videoOutputPath"] == "gpu-resident", options
        (tmp_path / "required.mp4").unlink()
    # Required outside the extent is an error naming the path, never a fallback.
    outside = [(gpu["maximumWidth"] + even(gpu["widthAlignment"]), inside[1])]
    if has_below:
        outside.append(below)
    for size in outside:
        with pytest.raises(Alpha2HostError) as caught:
            export_size(
                client,
                tmp_path,
                *size,
                f"outside-{size[0]}x{size[1]}.mp4",
                input_policy="gpu-required",
            )
        assert caught.value.code == "GPU_OUTPUT_UNAVAILABLE", size


def test_a_clip_shown_as_it_is_reaches_the_gpu_encoder_as_its_decoded_planes(
    client: HostClient, encode: dict[str, Any], tmp_path: Path
) -> None:
    gpu = encode["gpuOutputPath"]
    width, height = 256, 144
    if not gpu["available"] or width < gpu["minimumWidth"] or height < gpu["minimumHeight"]:
        pytest.skip(
            f"no GPU-resident path for {width}x{height}: {gpu['reason'] or 'below its extent'}"
        )
    half = 60_000

    def export_at(output_width: int, output_height: int, name: str) -> dict[str, Any]:
        project = copy_project(0, half)
        project["settings"] = {
            **project["settings"],
            "width": output_width,
            "height": output_height,
        }
        return client.export(
            project,
            tmp_path / name,
            assets={"video": ROOT / "tests/corpus/h264-bt709-transcode.mp4"},
            end_ticks=half,
            include_audio=False,
        )

    transcoded = export_at(width, height, "transcoded.mp4")
    assert transcoded["videoOutputPath"] == "gpu-resident"
    assert transcoded["strategy"] == "render"
    assert transcoded["videoTranscode"] == "yuv-repack"
    assert transcoded["yuvTranscodeIneligible"] is None
    assert transcoded["frames"] == 15
    assert transcoded["performance"]["yuvRepacks"] == 15
    assert transcoded["performance"]["graphPasses"] == 0
    assert_declared_performance(transcoded["performance"], "gpu-resident")
    rendered = export_at(width * 2, height * 2, "rendered.mp4")
    assert rendered["videoTranscode"] is None
    assert "not the 512x288 output" in rendered["yuvTranscodeIneligible"]
    assert rendered["performance"]["graphPasses"] > 0
