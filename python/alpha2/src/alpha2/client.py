"""The ``alpha2-host`` client: capabilities, conform and export on sidecar processes.

Each request runs on its own ``alpha2-host serve`` process, at most ``concurrency`` at a time per
client; further requests wait for a slot. The API is synchronous: progress callbacks run on the
calling thread, a ``cancel`` event or Ctrl-C ends the request's host (which removes its partial
output) before the call returns, and every failure is an :class:`~alpha2.Alpha2HostError`.

Arguments written in Python (option dicts such as ``loudness`` or ``memory``) use snake_case keys;
what the host reports (capabilities, conform reports, export results) keeps the protocol's
camelCase keys, as the host wrote them.
"""

from __future__ import annotations

import collections
import itertools
import json
import os
import queue
import signal as signals
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal, TypedDict

from . import _native
from .errors import (
    Alpha2AbortError,
    Alpha2ConformError,
    Alpha2GpuInitializationError,
    Alpha2HostError,
    Alpha2ProjectValidationError,
    ExportPhase,
)
from .install import cached_host_binary, host_cache_directory, host_executable_name, package_version
from .protocol import UNREAD_REQUEST_ID, create_request, loader_refused, parse_host_message

if TYPE_CHECKING:
    from .compose import Compiled

__all__ = [
    "QUALITY_LEVELS",
    "STDERR_TAIL_BYTES",
    "ExportProgress",
    "HostClient",
    "LoudnessTarget",
    "MemoryBudget",
    "ProxyFile",
    "RenderCacheOptions",
    "VideoDecodeMode",
    "capabilities",
    "conform",
    "export",
    "resolve_host_binary",
]

VideoDecodeMode = Literal["software", "auto", "hardware-required"]
"""The video decode path asked of the host. Unset, the host uses its platform default ("auto" on
Linux, "software" on macOS and Windows)."""

_VIDEO_DECODE_MODES = ("software", "auto", "hardware-required")

QUALITY_LEVELS = ("very-low", "low", "medium", "high", "very-high")
"""The engine's five quality levels, lowest first."""

STDERR_TAIL_BYTES = 16_384
"""Bytes of the host's standard error an error keeps."""

# How long a host asked to end has before it is killed, and a host that stays up without taking its
# request before the write is reported failed.
_HOST_SHUTDOWN_TIMEOUT = 5.0

# The longest request line the host reads (MAX_REQUEST_LINE_BYTES), newline excluded.
_MAX_REQUEST_LINE_BYTES = 2 * _native.MAX_PROJECT_JSON_BYTES

# How often a waiting call looks at its cancel event.
_POLL_SECONDS = 0.05

_HOST_RELEASES_URL = "https://github.com/alpha2-sh/alpha2/releases"

_request_sequence = itertools.count()


class ProxyFile(TypedDict, total=False):
    """A proxy made from a source's original."""

    path: str | os.PathLike[str]
    made_from: dict[str, Any]
    """Evidence of the original it was made from (a ``MediaFingerprint``), when known."""
    color_pipeline: str
    """The color pipeline baked into it, when it was tone-mapped."""


class LoudnessTarget(TypedDict):
    """Normalizes an export's audio to an integrated loudness under a true-peak ceiling."""

    target_lufs: float
    """Integrated loudness, -60..=0 LUFS; streaming platforms use -14 to -16."""
    true_peak_ceiling_dbtp: float
    """True-peak ceiling, -20..=0 dBTP; EBU R128 uses -1."""


class RenderCacheOptions(TypedDict, total=False):
    """Where an export bakes frames, for a later export of the same snapshot to take."""

    directory: str | os.PathLike[str]
    quota_bytes: int
    """The most bytes the directory holds. Default 4 GiB."""
    backlog: Literal["skip", "wait"]
    """A frame rendered while the cache writer is full: skipped (default) or waited for."""


class MemoryBudget(TypedDict, total=False):
    """Byte budgets for one export, replacing the host's environment settings field by field."""

    in_flight_bytes: int
    decode_queue_bytes: int
    reverse_cache_bytes: int
    audio_window_bytes: int
    provider_source_bytes: int
    scale: float
    """A fraction in (0, 1] every budget is scaled by once resolved."""


_MEMORY_FIELDS = {
    "in_flight_bytes": "inFlightBytes",
    "decode_queue_bytes": "decodeQueueBytes",
    "reverse_cache_bytes": "reverseCacheBytes",
    "audio_window_bytes": "audioWindowBytes",
    "provider_source_bytes": "providerSourceBytes",
}


@dataclass(frozen=True)
class ExportProgress:
    """One progress report of an export."""

    phase: Literal["frames", "finalizing"]
    """``frames`` while frames are encoded; ``finalizing`` once every frame is and the output is
    being flushed and closed. The export is complete only when the call returns."""
    completed: int
    """Frames encoded so far; at most ``total``."""
    total: int
    """Frames in the export."""


ProgressCallback = Callable[[ExportProgress], object]

PathLike = str | os.PathLike[str]


def _invalid(message: str) -> Alpha2HostError:
    return Alpha2HostError("INVALID_INPUT", message)


def _check_video_decode(field: str, value: object) -> None:
    """Refuses a set ``field`` that is not a decode mode: the host would refuse it at its start."""
    if value is not None and value not in _VIDEO_DECODE_MODES:
        raise _invalid(f"{field} must be software, auto, or hardware-required")


def _is_file(path: Path) -> bool:
    try:
        return path.is_file()
    except OSError:
        return False


def resolve_host_binary(
    host_path: PathLike | None = None,
    *,
    cache_directory: PathLike | None = None,
    cwd: PathLike | None = None,
) -> Path:
    """Locates ``alpha2-host``.

    The binary comes in the platform's native archive, as ``bin/alpha2-host`` beside the ``lib/``
    it loads: the ``alpha2-host`` wheel this package depends on where one is published carries it,
    and :func:`alpha2.install_host` (``python -m alpha2 install``) downloads it into the cache
    shared with ``@alpha2-sh/node`` elsewhere. The order is ``host_path``, ``ALPHA2_HOST_PATH``, the
    ``alpha2-host`` wheel of this package's version, the archive of this package's version in the
    cache (bundled, then thin), then ``PATH``.

    Raises :class:`~alpha2.Alpha2HostError`: ``INVALID_INPUT`` for an empty ``host_path``,
    ``HOST_NOT_FOUND`` when an explicit path is not a file or no candidate exists. An empty
    ``ALPHA2_HOST_PATH`` is unset.
    """
    base = Path(cwd) if cwd is not None else Path.cwd()
    if host_path is not None and str(os.fspath(host_path)).strip() == "":
        raise _invalid("host_path must not be empty")
    # An empty variable is an unset one, as a shell or Compose leaves a variable it was given blank.
    from_environment = os.environ.get("ALPHA2_HOST_PATH", "")
    explicit = (
        host_path
        if host_path is not None
        else (from_environment if from_environment.strip() else None)
    )
    if explicit is not None:
        candidate = (base / os.fspath(explicit)).absolute()
        if _is_file(candidate):
            return candidate
        raise Alpha2HostError("HOST_NOT_FOUND", f"alpha2-host does not exist: {candidate}")

    wheel = _wheel_host_binary()
    if wheel is not None:
        return wheel
    cache = (
        base / os.fspath(cache_directory)
        if cache_directory is not None
        else host_cache_directory(base)
    )
    cached = cached_host_binary(cache)
    if cached is not None:
        return cached
    executable = host_executable_name()
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory:
            continue
        candidate = Path(directory) / executable
        if _is_file(candidate):
            return candidate
    version = package_version()
    raise Alpha2HostError(
        "HOST_NOT_FOUND",
        f"could not locate {executable}; install the alpha2-host package of this version, or run "
        f"`python -m alpha2 install` to download alpha2-host{f' {version}' if version else ''} "
        "for this platform into the cache, or "
        f"download the native archive from {_HOST_RELEASES_URL} and pass host_path or set "
        f"ALPHA2_HOST_PATH to its bin/{executable}, or put that bin/ on PATH",
    )


def _wheel_host_binary() -> Path | None:
    """The ``alpha2-host`` the ``alpha2-host`` wheel installed, when it is this package's version.

    A wheel of another version speaks another protocol, so it is passed over like a cache entry of
    another version.
    """
    try:
        import alpha2_host  # type: ignore[import-not-found,unused-ignore]
    except ImportError:
        return None
    if getattr(alpha2_host, "__version__", None) != package_version():
        return None
    candidate = Path(alpha2_host.host_path())
    return candidate if _is_file(candidate) else None


class _Slots:
    """A first-come-first-served semaphore whose waiters can be cancelled."""

    def __init__(self, limit: int) -> None:
        self._available = limit
        self._waiting: collections.deque[object] = collections.deque()
        self._condition = threading.Condition()

    def acquire(self, cancel: threading.Event | None) -> None:
        ticket = object()
        with self._condition:
            self._waiting.append(ticket)
            try:
                while not (self._available > 0 and self._waiting[0] is ticket):
                    if cancel is not None and cancel.is_set():
                        raise Alpha2AbortError()
                    self._condition.wait(_POLL_SECONDS)
                self._available -= 1
            finally:
                self._waiting.remove(ticket)
                self._condition.notify_all()

    def release(self) -> None:
        with self._condition:
            self._available += 1
            self._condition.notify_all()


class _Probe:
    """One capability probe and the calls waiting for it."""

    def __init__(self) -> None:
        self.cancel = threading.Event()
        self.done = threading.Event()
        self.waiting = 0
        self.result: dict[str, Any] | None = None
        self.error: BaseException | None = None


def _project_problems(project: Mapping[str, Any]) -> list[str]:
    """What the host's engine would refuse in ``project``, found before a host starts."""
    try:
        text = _project_json(project)
    except ValueError as error:
        return [f"project cannot be written as JSON: {error}"]
    return list(_native.validate_project_json(text))


def _project_json(project: Mapping[str, Any]) -> str:
    return json.dumps(project, ensure_ascii=False, allow_nan=False, separators=(",", ":"))


def _path_record(field: str, value: object, what: str) -> dict[str, str]:
    """``value`` as a record of non-empty path strings, or ``INVALID_INPUT``."""
    if value is None:
        return {}
    if not isinstance(value, Mapping):
        raise _invalid(f"{field} must map ids to paths")
    record: dict[str, str] = {}
    for key, path in value.items():
        text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else None
        if not isinstance(key, str) or not isinstance(text, str) or not text:
            raise _invalid(f"{what} for {key} must be a non-empty string")
        record[key] = text
    return record


def _proxies_payload(proxies: object) -> dict[str, list[dict[str, Any]]]:
    if proxies is None:
        return {}
    if not isinstance(proxies, Mapping):
        raise _invalid("proxies must map source ids to lists of proxy files")
    payload: dict[str, list[dict[str, Any]]] = {}
    for source, files in proxies.items():
        message = f"proxies for source {source} must be a list of {{path}} proxy files"
        if isinstance(files, (str, bytes)) or not isinstance(files, Sequence):
            raise _invalid(message)
        entries: list[dict[str, Any]] = []
        for file in files:
            path = file.get("path") if isinstance(file, Mapping) else None
            text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else None
            if not isinstance(text, str) or not text:
                raise _invalid(message)
            entry: dict[str, Any] = {"path": text}
            if file.get("made_from") is not None:
                entry["madeFrom"] = file["made_from"]
            if file.get("color_pipeline") is not None:
                entry["colorPipeline"] = file["color_pipeline"]
            entries.append(entry)
        payload[source] = entries
    return payload


def _frames_payload(frames: object) -> dict[str, list[str | None]]:
    if frames is None:
        return {}
    if not isinstance(frames, Mapping):
        raise _invalid("frames must map source ids to lists of frame files")
    payload: dict[str, list[str | None]] = {}
    for source, files in frames.items():
        message = f"frames for source {source} must be a list of frame file paths, None for a gap"
        if isinstance(files, (str, bytes)) or not isinstance(files, Sequence):
            raise _invalid(message)
        entries: list[str | None] = []
        for file in files:
            if file is None:
                entries.append(None)
                continue
            text = os.fspath(file) if isinstance(file, (str, os.PathLike)) else None
            if not isinstance(text, str) or not text:
                raise _invalid(message)
            entries.append(text)
        payload[source] = entries
    return payload


def _safe_integer(value: object) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and abs(value) <= 9_007_199_254_740_991
    )


def _quality(field: str, quality: object, keys: tuple[str, ...]) -> object:
    """Refuses what the host would refuse, naming the field, before a process is started."""
    if quality is None:
        return None
    if isinstance(quality, str):
        if quality not in QUALITY_LEVELS:
            raise _invalid(
                f'{field} must be one of {", ".join(QUALITY_LEVELS)}, received "{quality}"'
            )
        return quality
    if not isinstance(quality, Mapping):
        raise _invalid(f"{field} must be a quality level name or an explicit rate dict")
    unknown = next((key for key in quality if key not in keys), None)
    if unknown is not None:
        raise _invalid(
            f'{field} has an unknown key "{unknown}"; an explicit rate takes {" and ".join(keys)}'
        )
    bitrate = quality.get("bitrate")
    if not _safe_integer(bitrate) or bitrate <= 0:  # type: ignore[operator]
        raise _invalid(f"{field}.bitrate must be a positive integer number of bits per second")
    mode = quality.get("mode")
    if mode is not None and mode not in ("constant", "variable"):
        raise _invalid(f'{field}.mode must be "constant" or "variable"')
    quantizer = quality.get("quantizer")
    if quantizer is not None and (not _safe_integer(quantizer) or not 0 <= quantizer <= 255):
        raise _invalid(f"{field}.quantizer must be an integer from 0 to 255")
    return dict(quality)


def _loudness(loudness: object, include_audio: bool) -> dict[str, float] | None:
    """The host's ``loudness`` checks, with its messages in this API's names."""
    if loudness is None:
        return None
    if not isinstance(loudness, Mapping):
        raise _invalid("loudness must be a dict {target_lufs, true_peak_ceiling_dbtp}")
    unknown = next(
        (key for key in loudness if key not in ("target_lufs", "true_peak_ceiling_dbtp")), None
    )
    if unknown is not None:
        raise _invalid(
            f'loudness has an unknown key "{unknown}"; it takes target_lufs and '
            "true_peak_ceiling_dbtp"
        )
    payload: dict[str, float] = {}
    for key, low, wire in (
        ("target_lufs", -60, "targetLufs"),
        ("true_peak_ceiling_dbtp", -20, "truePeakCeilingDbtp"),
    ):
        value = loudness.get(key)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or not low <= value <= 0:
            raise _invalid(f"loudness.{key} must be within {low}..=0, received {value}")
        payload[wire] = value
    if not include_audio:
        raise _invalid("loudness normalizes the export's audio and needs include_audio=True")
    return payload


def _memory(memory: object) -> dict[str, Any] | None:
    if memory is None:
        return None
    if not isinstance(memory, Mapping):
        raise _invalid("memory must be a dict of byte budgets")
    payload: dict[str, Any] = {}
    for field, value in memory.items():
        if field == "scale":
            if value is not None and (
                not isinstance(value, (int, float)) or isinstance(value, bool) or not 0 < value <= 1
            ):
                raise _invalid("memory.scale must be above 0 and at most 1")
            if value is not None:
                payload["scale"] = value
            continue
        if field not in _MEMORY_FIELDS:
            raise _invalid(f'memory has an unknown field "{field}"')
        if value is not None and (not _safe_integer(value) or value <= 0):
            raise _invalid(f"memory.{field} must be a positive byte count")
        if value is not None:
            payload[_MEMORY_FIELDS[field]] = value
    return payload


def _render_cache(cache: object) -> dict[str, Any] | None:
    if cache is None:
        return None
    message = (
        "render_cache takes a non-empty directory, an optional positive integer quota_bytes, "
        'and an optional backlog "skip" or "wait"'
    )
    if not isinstance(cache, Mapping):
        raise _invalid(message)
    directory = cache.get("directory")
    text = os.fspath(directory) if isinstance(directory, (str, os.PathLike)) else None
    quota = cache.get("quota_bytes")
    backlog = cache.get("backlog")
    if (
        not isinstance(text, str)
        or not text
        or (quota is not None and (not _safe_integer(quota) or quota <= 0))
        or (backlog is not None and backlog not in ("skip", "wait"))
        or any(key not in ("directory", "quota_bytes", "backlog") for key in cache)
    ):
        raise _invalid(message)
    payload: dict[str, Any] = {"directory": text}
    if quota is not None:
        payload["quotaBytes"] = quota
    if backlog is not None:
        payload["backlog"] = backlog
    return payload


def _frame_rate(value: object) -> dict[str, int]:
    """A frame rate given as ``{"num", "den"}``, ``(num, den)`` or a whole number of frames."""
    if _safe_integer(value):
        value = {"num": value, "den": 1}
    elif isinstance(value, tuple) and len(value) == 2:
        value = {"num": value[0], "den": value[1]}
    if (
        not isinstance(value, Mapping)
        or not _safe_integer(value.get("num"))
        or value["num"] <= 0
        or not _safe_integer(value.get("den"))
        or value["den"] <= 0
    ):
        raise _invalid("frame_rate must be a positive rational")
    return {"num": value["num"], "den": value["den"]}


def _optional_name(field: str, value: object) -> str | None:
    if value is not None and (not isinstance(value, str) or not value):
        raise _invalid(f"{field} must be a non-empty string")
    return value


class HostClient:
    """Runs requests on ``alpha2-host`` sidecar processes.

    One process per request, at most ``concurrency`` at a time; further requests wait for a slot.
    The binary is resolved now (see :func:`resolve_host_binary`). ``video_decode`` is the decode
    path for every request this client makes, unless a request names its own. A client is safe to
    share between threads.

    Raises ``ValueError`` for a ``concurrency`` that is not a positive integer, and
    :class:`~alpha2.Alpha2HostError` when ``video_decode`` is not a mode (``INVALID_INPUT``) or the
    binary cannot be found.
    """

    def __init__(
        self,
        *,
        host_path: PathLike | None = None,
        cache_directory: PathLike | None = None,
        concurrency: int = 1,
        video_decode: VideoDecodeMode | None = None,
    ) -> None:
        if not _safe_integer(concurrency) or concurrency < 1:
            raise ValueError("concurrency must be a positive integer")
        _check_video_decode("video_decode", video_decode)
        self.host_path: Path = resolve_host_binary(host_path, cache_directory=cache_directory)
        self.concurrency = concurrency
        self.video_decode: VideoDecodeMode | None = video_decode
        self._slots = _Slots(concurrency)
        self._probes: dict[str, _Probe] = {}
        self._probes_lock = threading.Lock()

    def capabilities(
        self,
        *,
        video_decode: VideoDecodeMode | None = None,
        cancel: threading.Event | None = None,
    ) -> dict[str, Any]:
        """What the host can decode, render and encode, for the client's decode mode or this one.

        The first call for a mode starts a host, in a slot like any request; calls made while it
        runs share it, and its report answers every later call for that mode on this client,
        since the same binary and mode report the same. A probe that fails is not kept: the next
        call starts another. ``cancel`` withdraws this call (:class:`~alpha2.Alpha2AbortError`); the
        probe
        goes on while another call still waits for it.
        """
        _check_video_decode("video_decode", video_decode)
        if cancel is not None and cancel.is_set():
            raise Alpha2AbortError()
        mode = video_decode if video_decode is not None else self.video_decode
        # Unset, the mode is the host's default or the one this process's environment names.
        key = (
            mode if mode is not None else f"environment:{os.environ.get('ALPHA2_VIDEO_DECODE', '')}"
        )
        with self._probes_lock:
            probe = self._probes.get(key)
            if probe is None:
                probe = _Probe()
                self._probes[key] = probe
                threading.Thread(
                    target=self._run_probe, args=(key, probe, mode), daemon=True
                ).start()
            if probe.done.is_set() and probe.error is None:
                assert probe.result is not None
                return probe.result
            probe.waiting += 1
        try:
            while not probe.done.wait(_POLL_SECONDS):
                if cancel is not None and cancel.is_set():
                    raise Alpha2AbortError()
        except BaseException:
            # A withdrawn call (or Ctrl-C) leaves the probe; the last one to leave withdraws it.
            with self._probes_lock:
                probe.waiting -= 1
                if probe.waiting == 0 and not probe.done.is_set():
                    if self._probes.get(key) is probe:
                        del self._probes[key]
                    probe.cancel.set()
            raise
        with self._probes_lock:
            probe.waiting -= 1
        if probe.error is not None:
            raise probe.error
        assert probe.result is not None
        return probe.result

    def _run_probe(self, key: str, probe: _Probe, mode: VideoDecodeMode | None) -> None:
        try:
            self._slots.acquire(probe.cancel)
            try:
                probe.result = _request_host(
                    self.host_path, "capabilities", {}, mode, probe.cancel, None
                )
            finally:
                self._slots.release()
        except BaseException as error:  # noqa: BLE001 - handed to every waiting call
            probe.error = error
            with self._probes_lock:
                if self._probes.get(key) is probe:
                    del self._probes[key]
        finally:
            probe.done.set()

    def conform(
        self,
        project: Mapping[str, Any],
        *,
        assets: Mapping[str, PathLike] | None = None,
        proxies: Mapping[str, Sequence[ProxyFile]] | None = None,
        frames: Mapping[str, Sequence[PathLike | None]] | None = None,
        purpose: str = "export_original",
        cancel: threading.Event | None = None,
    ) -> dict[str, Any]:
        """Which media a purpose would read for each source, and what blocks it, without rendering.

        Returns ``{"catalog", "report"}``. The host hashes an original only when the project
        recorded a hash to compare. Raises :class:`~alpha2.Alpha2ProjectValidationError` for an
        invalid project. ``purpose`` is ``preview``, ``preview_proxy``, ``thumbnail``,
        ``remote_edit``, ``export_original`` (the default) or ``export_proxy``.
        """
        problems = _project_problems(project)
        if problems:
            raise Alpha2ProjectValidationError(problems)
        payload = {
            "project": project,
            "assets": _path_record("assets", assets, "asset path"),
            "proxies": _proxies_payload(proxies),
            "frames": _frames_payload(frames),
            "purpose": purpose.replace("_", "-"),
        }
        self._slots.acquire(cancel)
        try:
            return _request_host(  # type: ignore[no-any-return]
                self.host_path, "conform", payload, self.video_decode, cancel, None
            )
        finally:
            self._slots.release()

    def export(
        self,
        project: Mapping[str, Any] | Compiled,
        output: PathLike,
        *,
        assets: Mapping[str, PathLike] | None = None,
        proxies: Mapping[str, Sequence[ProxyFile]] | None = None,
        frames: Mapping[str, Sequence[PathLike | None]] | None = None,
        resources: Mapping[str, Mapping[str, PathLike]] | None = None,
        media: Literal["original", "proxies"] = "original",
        effect_assets: Mapping[str, PathLike] | None = None,
        fonts: Sequence[PathLike] | None = None,
        effects: Sequence[Mapping[str, Any]] | None = None,
        start_ticks: int = 0,
        end_ticks: int | None = None,
        frame_rate: Mapping[str, int] | tuple[int, int] | int | None = None,
        include_audio: bool = True,
        container: Literal["mp4", "mov", "mkv", "webm"] | None = None,
        video_codec: str | None = None,
        video_encoder: str | None = None,
        input_policy: Literal["auto", "gpu-required", "cpu-readback"] = "auto",
        strategy: Literal["render", "prefer-copy", "require-copy"] = "render",
        video_quality: str | Mapping[str, Any] | None = None,
        audio_codec: Literal["aac", "opus"] | None = None,
        audio_quality: str | Mapping[str, Any] | None = None,
        loudness: LoudnessTarget | None = None,
        render_cache: RenderCacheOptions | None = None,
        memory: MemoryBudget | None = None,
        video_decode: VideoDecodeMode | None = None,
        on_progress: ProgressCallback | None = None,
        cancel: threading.Event | None = None,
    ) -> dict[str, Any]:
        """Renders and encodes ``project`` to ``output`` and returns the host's export result.

        ``assets`` maps source ids to the files a render reads; the host checks them before it
        renders and refuses with :class:`~alpha2.Alpha2ConformError` listing what is missing,
        offline, or not the media the project recorded. The range is ``start_ticks`` to
        ``end_ticks`` (default: where the project ends), widened to the grid of ``frame_rate``
        (default: the project's). ``output``'s extension (``.mp4``, ``.m4v``, ``.mov``, ``.mkv``
        or ``.webm``) names the container, which must agree with ``container`` when both are given;
        absent codecs are the container's defaults.

        ``on_progress`` receives each :class:`ExportProgress` on the calling thread; an exception
        it raises fails the export with ``PROGRESS_CALLBACK_FAILED`` and ends its host. Setting
        ``cancel``, or Ctrl-C, ends the host, which removes its partial file: the call then raises
        :class:`~alpha2.Alpha2AbortError`, or re-raises the ``KeyboardInterrupt``.

        Raises :class:`~alpha2.Alpha2ProjectValidationError` or an ``INVALID_INPUT``
        :class:`~alpha2.Alpha2HostError` before a host starts when the project or request is
        invalid, and :class:`~alpha2.Alpha2HostError` otherwise. Under the ``auto`` decode mode, an
        export whose hardware decoder stalls (``DECODE_STALLED``) runs once more decoding in
        software; its progress starts over.

        ``project`` may be a :class:`~alpha2.compose.Compiled` spec: its ``assets``, ``fonts`` and
        ``loudness`` are the defaults of those options.

        The result's keys are the host protocol's (``videoOutputPath``, ``effectiveRange``,
        ``performance``, ...).
        """
        from .compose import Compiled  # here: compose imports this module

        if isinstance(project, Compiled):
            compiled = project
            project = compiled.project
            assets = compiled.assets if assets is None else assets
            fonts = compiled.fonts if fonts is None else fonts
            if loudness is None and include_audio:
                loudness = compiled.loudness
        problems = _project_problems(project)
        if problems:
            raise Alpha2ProjectValidationError(problems)
        output_text = os.fspath(output) if isinstance(output, (str, os.PathLike)) else None
        if not isinstance(output_text, str) or not output_text:
            raise _invalid("output must not be empty")
        if end_ticks is None:
            end_ticks = _native.project_end_ticks(_project_json(project))
            if end_ticks is None:
                raise _invalid("end_ticks is required for a project that shows nothing")
        if (
            not _safe_integer(start_ticks)
            or not _safe_integer(end_ticks)
            or end_ticks <= start_ticks
        ):
            raise _invalid("export range must be a non-empty integer tick range")
        rate = _frame_rate(
            frame_rate if frame_rate is not None else project["settings"]["frame_rate"]
        )
        memory_payload = _memory(memory)
        effect_payload = _path_record("effect_assets", effect_assets, "effect asset path")
        resources_payload: dict[str, dict[str, str]] = {}
        if resources is not None:
            if not isinstance(resources, Mapping):
                raise _invalid("resources must map source ids to their files")
            for source, files in resources.items():
                resources_payload[source] = _path_record(
                    f"resources.{source}", files, "resource path"
                )
        if effects is not None and (
            isinstance(effects, (str, bytes, Mapping)) or not isinstance(effects, Sequence)
        ):
            raise _invalid("effects must be a list of effect definitions")
        if media not in ("original", "proxies"):
            raise _invalid('media must be "original" or "proxies"')
        font_paths: list[str] = []
        if fonts is not None:
            if isinstance(fonts, (str, bytes, os.PathLike)) or not isinstance(fonts, Sequence):
                raise _invalid("fonts must be a list of font file paths")
            for font in fonts:
                text = os.fspath(font) if isinstance(font, (str, os.PathLike)) else None
                if not isinstance(text, str) or not text:
                    raise _invalid("fonts must be a list of font file paths")
                font_paths.append(text)
        for resource in project.get("effect_resources") or []:
            if not effect_payload.get(resource["id"]):
                raise Alpha2HostError(
                    "MISSING_INPUT", f"missing effect asset path for resource {resource['id']}"
                )
        _check_video_decode("video_decode", video_decode)
        # The engine's container policy, which the host applies to the same request: a container
        # the output's extension contradicts, or a codec the container does not offer, never starts
        # a host.
        for field, value in (
            ("container", container),
            ("video_codec", video_codec),
            ("audio_codec", audio_codec),
        ):
            _optional_name(field, value)
        try:
            _native.resolve_export_format(
                container, os.path.splitext(output_text)[1][1:], video_codec, audio_codec
            )
        except ValueError as error:
            raise _invalid(str(error)) from None
        if video_encoder is not None and (not isinstance(video_encoder, str) or not video_encoder):
            raise _invalid("video_encoder must be a non-empty FFmpeg encoder name")
        if strategy not in ("render", "prefer-copy", "require-copy"):
            raise _invalid("strategy must be render, prefer-copy, or require-copy")
        if input_policy not in ("auto", "gpu-required", "cpu-readback"):
            raise _invalid("input_policy must be auto, gpu-required, or cpu-readback")
        video_rate = _quality("video_quality", video_quality, ("bitrate", "mode", "quantizer"))
        audio_rate = _quality("audio_quality", audio_quality, ("bitrate",))
        loudness_payload = _loudness(loudness, include_audio)
        cache_payload = _render_cache(render_cache)

        payload: dict[str, Any] = {
            "project": project,
            "assets": _path_record("assets", assets, "asset path"),
            "proxies": _proxies_payload(proxies),
            "frames": _frames_payload(frames),
            "resources": resources_payload,
            "media": media,
            "effectAssets": effect_payload,
            "fonts": font_paths,
            "effects": list(effects or []),
            "startTicks": start_ticks,
            "endTicks": end_ticks,
            "frameRate": rate,
            "includeAudio": include_audio,
            "inputPolicy": input_policy,
            "strategy": strategy,
            "output": output_text,
        }
        # The host resolves what is not named from the container, so there is one rule.
        named: tuple[tuple[str, object], ...] = (
            ("container", container),
            ("videoCodec", video_codec),
            ("videoEncoder", video_encoder),
            ("videoQuality", video_rate),
            ("audioCodec", audio_codec),
            ("audioQuality", audio_rate),
            ("loudness", loudness_payload),
            ("renderCache", cache_payload),
            ("memory", memory_payload),
        )
        for wire, named_value in named:
            if named_value is not None:
                payload[wire] = named_value

        mode = video_decode if video_decode is not None else self.video_decode
        self._slots.acquire(cancel)
        try:
            try:
                return _request_host(  # type: ignore[no-any-return]
                    self.host_path, "export", payload, mode, cancel, on_progress
                )
            except Alpha2HostError as stall:
                # A hardware decoder stuck inside the driver cannot be recovered by its host, so the
                # export reruns in a new host decoding on the CPU. Only a stall with a hardware
                # session (resource "gpu") is retried: a software stall would recur, and
                # hardware-required rules it out.
                if (
                    stall.code != "DECODE_STALLED"
                    or stall.failure.get("resource") != "gpu"
                    or (mode or os.environ.get("ALPHA2_VIDEO_DECODE")) == "hardware-required"
                ):
                    raise
                try:
                    return _request_host(  # type: ignore[no-any-return]
                        self.host_path, "export", payload, "software", cancel, on_progress
                    )
                except Alpha2HostError as retry_error:
                    raise _record_decode_stall(retry_error, stall) from stall
        finally:
            self._slots.release()


def _record_decode_stall(retry_error: Alpha2HostError, stall: Alpha2HostError) -> Alpha2HostError:
    """``retry_error`` with the stall that led to it in its failure's diagnostics."""
    note: dict[str, Any] = {
        "hostCode": stall.code,
        "code": stall.failure.get("code"),
        "phase": stall.failure.get("phase"),
        "message": stall.message,
    }
    if stall.stderr is not None:
        note["stderr"] = stall.stderr
    retry_error.failure = {
        **retry_error.failure,
        "diagnostics": {**(retry_error.failure.get("diagnostics") or {}), "decodeStall": note},
    }
    return retry_error


def _utf8_tail(data: bytes, limit: int) -> str:
    """The last at most ``limit`` bytes of ``data`` as text, from the first whole character."""
    start = max(0, len(data) - limit)
    # A continuation byte (10xxxxxx) is the middle of a character.
    while start < len(data) and data[start] & 0xC0 == 0x80:
        start += 1
    text = data[start:].decode("utf-8", errors="replace")
    # A byte that is not UTF-8 becomes U+FFFD, three bytes: drop characters from the front to fit.
    excess = len(text.encode("utf-8")) - limit
    index = 0
    while excess > 0 and index < len(text):
        excess -= len(text[index].encode("utf-8"))
        index += 1
    return text[index:]


def _ended(returncode: int | None) -> tuple[int | None, str | None]:
    """A process's end as an exit code and a signal name, one of them ``None``."""
    if returncode is not None and returncode < 0 and sys.platform != "win32":
        try:
            return None, signals.Signals(-returncode).name
        except ValueError:
            return None, str(-returncode)
    return returncode, None


class _Host:
    """One ``alpha2-host serve`` process answering one request."""

    def __init__(self, binary: Path, video_decode: str | None) -> None:
        environment = None
        if video_decode is not None:
            # The host chooses its decode path while it creates the renderer device, before any
            # request arrives, so the mode travels in the environment rather than in the request.
            environment = {**os.environ, "ALPHA2_VIDEO_DECODE": video_decode}
        self.process = subprocess.Popen(
            [str(binary), "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        self.events: queue.Queue[tuple[str, Any]] = queue.Queue()
        self._stderr = bytearray()
        self._stderr_lock = threading.Lock()
        self._threads = [
            threading.Thread(target=self._read_stdout, daemon=True),
            threading.Thread(target=self._read_stderr, daemon=True),
        ]
        for thread in self._threads:
            thread.start()

    def keep_stderr(self, data: bytes) -> None:
        with self._stderr_lock:
            self._stderr += data
            if len(self._stderr) > STDERR_TAIL_BYTES:
                del self._stderr[: len(self._stderr) - STDERR_TAIL_BYTES]

    def stderr_tail(self) -> str:
        with self._stderr_lock:
            return _utf8_tail(bytes(self._stderr), STDERR_TAIL_BYTES)

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        try:
            for raw in self.process.stdout:
                self.events.put(("line", raw))
        except (OSError, ValueError):
            pass
        self.events.put(("eof", None))

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        try:
            while chunk := self.process.stderr.read1(65_536):  # type: ignore[attr-defined]
                self.keep_stderr(chunk)
        except (OSError, ValueError):
            pass

    def write(self, line: bytes) -> None:
        """Writes the request on a thread of its own: a host that does not read never blocks us."""

        def send() -> None:
            assert self.process.stdin is not None
            try:
                self.process.stdin.write(line)
                self.process.stdin.close()
            except (OSError, ValueError) as error:
                self.events.put(("write-failed", error))

        threading.Thread(target=send, daemon=True).start()

    def stop(self) -> None:
        """Asks the host to end, and kills it when it has not within the shutdown timeout."""
        if self.process.poll() is None:
            try:
                self.process.terminate()
            except OSError:
                pass

    def wait(self) -> tuple[int | None, str | None]:
        try:
            self.process.wait(_HOST_SHUTDOWN_TIMEOUT)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        # The readers end with the pipes; a grandchild holding them open is not waited for.
        for thread in self._threads:
            thread.join(_HOST_SHUTDOWN_TIMEOUT)
        for stream in (self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()
        return _ended(self.process.returncode)


def _error_from_host(error: dict[str, Any], stderr: str, phase: ExportPhase) -> Alpha2HostError:
    """The error the host reported, with its failure report when it sent one."""
    failure = None
    if isinstance(error.get("failure"), dict):
        failure = {**error["failure"], "message": error["message"], "stderr": stderr}
    options: dict[str, Any] = {"failure": failure, "stderr": stderr, "phase": phase}
    if error["code"] == "GPU_INIT_FAILED":
        return Alpha2GpuInitializationError(error["message"], **options)
    if error["code"] == "CONFORM_FAILED" and error.get("details") is not None:
        return Alpha2ConformError(error["message"], error["details"], **options)
    return Alpha2HostError(error["code"], error["message"], **options)


def _request_host(
    binary: Path,
    operation: str,
    payload: dict[str, Any],
    video_decode: str | None,
    cancel: threading.Event | None,
    on_progress: ProgressCallback | None,
) -> Any:
    if cancel is not None and cancel.is_set():
        raise Alpha2AbortError()
    # Windows starts a batch file only through a shell, which this never asks for.
    if sys.platform == "win32" and binary.suffix.lower() in (".bat", ".cmd"):
        raise Alpha2HostError(
            "HOST_NOT_EXECUTABLE", f"alpha2-host must be the executable, not a batch file: {binary}"
        )
    if not os.access(binary, os.F_OK if sys.platform == "win32" else os.X_OK):
        raise Alpha2HostError("HOST_NOT_EXECUTABLE", f"cannot execute alpha2-host: {binary}")
    request_id = f"python-{os.getpid()}-{int(time.time() * 1000)}-{next(_request_sequence)}"
    # The request becomes its line before a host starts: a value JSON cannot carry refuses the
    # request and starts nothing.
    try:
        line = json.dumps(
            create_request(operation, request_id, payload),
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
            default=_json_default,
        ).encode("utf-8")
    except (TypeError, ValueError) as cause:
        raise Alpha2HostError(
            "INVALID_INPUT", f"the {operation} request cannot be written as JSON: {cause}"
        ) from cause
    # The host would make its GPU device before refusing a line this long, unread.
    if len(line) > _MAX_REQUEST_LINE_BYTES:
        raise _invalid(
            f"the {operation} request is {len(line)} bytes of JSON; the host reads at most "
            f"{_MAX_REQUEST_LINE_BYTES}"
        )
    try:
        host = _Host(binary, video_decode)
    except OSError as cause:
        raise Alpha2HostError(
            "HOST_START_FAILED", f"could not start alpha2-host: {cause}"
        ) from cause
    # A cancel seen once the host has started ends it before the request is written.
    if cancel is not None and cancel.is_set():
        host.stop()
        host.wait()
        raise Alpha2AbortError()
    host.write(line + b"\n")

    last_phase: str | None = None

    def phase_now() -> ExportPhase:
        if last_phase == "finalizing":
            return "mux"
        return "render" if last_phase == "frames" else "prepare"

    # How the request ended: its result, or what makes its error once the host has ended (stderr
    # written before the answer may be read after it).
    outcome: tuple[bool, Any] | None = None
    write_failed_at: float | None = None
    write_error: BaseException | None = None
    eof = False
    try:
        while outcome is None and not eof:
            if cancel is not None and cancel.is_set():
                outcome = (False, Alpha2AbortError())
                break
            if (
                write_failed_at is not None
                and time.monotonic() - write_failed_at > _HOST_SHUTDOWN_TIMEOUT
            ):
                outcome = (False, Alpha2HostError("HOST_WRITE_FAILED", str(write_error)))
                break
            try:
                kind, value = host.events.get(timeout=_POLL_SECONDS)
            except queue.Empty:
                continue
            if kind == "eof":
                eof = True
                continue
            if kind == "write-failed":
                # The host's own report (a fatal line) and its exit arrive after this: they name
                # the cause. Only a host that stays up without taking its request fails as
                # unwritten.
                write_error = value
                write_failed_at = time.monotonic()
                continue
            raw: bytes = value
            text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
            # Every message is a JSON object. A line that does not start as one is a library's the
            # host loaded (a Vulkan validation layer prints to stdout): kept with stderr.
            if not text.startswith("{"):
                host.keep_stderr(raw if raw.endswith(b"\n") else raw + b"\n")
                continue
            try:
                message = parse_host_message(text)
            except ValueError:
                outcome = (
                    False,
                    Alpha2HostError(
                        "INVALID_RESPONSE", "alpha2-host returned invalid JSON", phase=phase_now()
                    ),
                )
                break
            if message.get("type") == "fatal":
                outcome = (False, (message["error"], phase_now()))
                break
            if message.get("type") == "progress":
                if message["requestId"] != request_id:
                    continue
                last_phase = message.get("phase", "frames")
                if on_progress is not None:
                    try:
                        on_progress(
                            ExportProgress(
                                message.get("phase", "frames"),
                                message["completed"],
                                message["total"],
                            )
                        )
                    except Exception as cause:  # noqa: BLE001 - reported as the export's failure
                        error = Alpha2HostError(
                            "PROGRESS_CALLBACK_FAILED",
                            "on_progress callback raised",
                            phase=phase_now(),
                        )
                        error.__cause__ = cause
                        outcome = (False, error)
                        break
                continue
            # A process serves this one request, so an answer to a line the host could not read an
            # id from is its answer.
            if message["requestId"] not in (request_id, UNREAD_REQUEST_ID):
                continue
            outcome = (
                (True, message["result"])
                if message["ok"]
                else (False, (message["error"], phase_now()))
            )
    except KeyboardInterrupt:
        # Ctrl-C: the host ends as an abort would end it, removing its partial file, before the
        # interrupt goes on.
        host.stop()
        host.wait()
        raise
    finally:
        host.stop()
    code, signal_name = host.wait()

    if outcome is not None and outcome[0]:
        return outcome[1]
    if outcome is not None:
        error = outcome[1]
        if isinstance(error, tuple):
            protocol_error, phase = error
            raise _error_from_host(protocol_error, host.stderr_tail(), phase)
        raise error
    tail = host.stderr_tail()
    if signal_name is not None:
        ended = f"exit {signal_name}"
    else:
        ended = f"exit {code if code is not None else 'unknown'}"
    detail = tail.strip() or ended
    exit_options: dict[str, Any] = {
        "stderr": tail,
        "exit_code": code,
        "signal": signal_name,
        "has_exit": True,
    }
    if code == 70 or "GPU_INIT_FAILED" in detail:
        raise Alpha2GpuInitializationError(detail, **exit_options)
    if loader_refused(code, tail):
        # The loader refused the executable before it ran (a missing or mismatched FFmpeg): the
        # same host on the same machine fails the same way, so no retry is asked for.
        raise Alpha2HostError(
            "HOST_LOAD_FAILED", f"alpha2-host could not be loaded: {detail}", **exit_options
        )
    # Nothing the host said names a cause, so none is claimed: a SIGKILL is as consistent with an
    # external kill as with the OOM killer.
    failure = {
        "code": "HOST_EXITED",
        "message": f"alpha2-host exited before responding ({ended})",
        "origin": "python",
        "operation": "export",
        "backend": "native",
        "phase": phase_now(),
        "resource": "unknown",
        "stderr": tail,
        "exitCode": code,
        "signal": signal_name,
    }
    raise Alpha2HostError(
        "HOST_EXITED",
        f"alpha2-host exited before responding: {detail}",
        failure=failure,
        stderr=tail,
        exit_code=code,
        signal=signal_name,
    )


def _json_default(value: object) -> object:
    """Paths are written as their text, which is how the host reads every file it is given."""
    if isinstance(value, os.PathLike):
        return os.fspath(value)
    raise TypeError(f"Object of type {type(value).__name__} is not JSON serializable")


_shared_clients: dict[tuple[int, str], HostClient] = {}
_shared_lock = threading.Lock()


def _shared_client(
    host_path: PathLike | None, cache_directory: PathLike | None, concurrency: int
) -> HostClient:
    """Clients by binary and ``concurrency``: the decode mode travels with each request, so calls
    that differ only in it still share the bound."""
    if not _safe_integer(concurrency) or concurrency < 1:
        raise ValueError("concurrency must be a positive integer")
    binary = resolve_host_binary(host_path, cache_directory=cache_directory)
    key = (concurrency, str(binary))
    with _shared_lock:
        client = _shared_clients.get(key)
        if client is None:
            client = HostClient(host_path=binary, concurrency=concurrency)
            _shared_clients[key] = client
        return client


def capabilities(
    *,
    host_path: PathLike | None = None,
    cache_directory: PathLike | None = None,
    concurrency: int = 1,
    video_decode: VideoDecodeMode | None = None,
    cancel: threading.Event | None = None,
) -> dict[str, Any]:
    """:meth:`HostClient.capabilities` on a client shared by every module-level call with the same
    resolved binary and ``concurrency``."""
    return _shared_client(host_path, cache_directory, concurrency).capabilities(
        video_decode=video_decode, cancel=cancel
    )


def conform(
    project: Mapping[str, Any],
    *,
    host_path: PathLike | None = None,
    cache_directory: PathLike | None = None,
    concurrency: int = 1,
    **options: Any,
) -> dict[str, Any]:
    """:meth:`HostClient.conform` on a shared client (see :func:`capabilities`)."""
    return _shared_client(host_path, cache_directory, concurrency).conform(project, **options)


def export(
    project: Mapping[str, Any] | Compiled,
    output: PathLike,
    *,
    host_path: PathLike | None = None,
    cache_directory: PathLike | None = None,
    concurrency: int = 1,
    **options: Any,
) -> dict[str, Any]:
    """:meth:`HostClient.export` on a shared client (see :func:`capabilities`), so ``concurrency``
    holds across calls whatever decode mode each asks for."""
    return _shared_client(host_path, cache_directory, concurrency).export(
        project, output, **options
    )
