"""The ``alpha2-host`` client: capabilities, conform and export on sidecar processes.

Each client keeps up to ``concurrency`` ``alpha2-host serve`` processes and sends each one request
at a time. A host serves request after request until it reports that it is draining or exits.
Calls are synchronous: progress callbacks run on the calling thread, a ``cancel`` event or Ctrl-C
cancels the request (removing partial output) before the call returns, and every failure is an
:class:`~alpha2.Alpha2HostError`.

Option dicts passed in use snake_case keys; host reports keep the protocol's camelCase keys.
"""

from __future__ import annotations

import collections
import hashlib
import itertools
import json
import os
import queue
import re
import signal as signals
import subprocess
import sys
import threading
import time
import unicodedata
import weakref
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal, TypedDict

from . import _host_start, _native, _wire
from ._quarantine import quarantine_refusal
from .builder import Time, to_ticks
from .errors import (
    Alpha2AbortError,
    Alpha2ConformError,
    Alpha2GpuInitializationError,
    Alpha2HostError,
    Alpha2ProjectValidationError,
    ExportPhase,
)
from .extract import ExtractProgress, schema_problem, take_step
from .install import cached_host_binary, host_cache_directory, host_executable_name, package_version
from .protocol import (
    UNREAD_REQUEST_ID,
    create_cancel,
    create_request,
    describe_exit,
    loader_refused,
    ntstatus_name,
    windows_policy_blocked,
)
from .results import ExportResult, StillResult

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
    "RendererMode",
    "VideoDecodeMode",
    "capabilities",
    "conform",
    "export",
    "resolve_host_binary",
]

VideoDecodeMode = Literal["software", "auto", "hardware-required"]
"""The video decode path asked of the host. Unset, the host uses its platform default ("auto" on
Linux and macOS, "software" on Windows)."""

_VIDEO_DECODE_MODES = ("software", "auto", "hardware-required")

RendererMode = Literal["auto", "gpu", "cpu"]
"""The renderer asked of the host. Unset, the host uses ``ALPHA2_RENDERER``, else "auto": a
hardware GPU if present, otherwise the CPU renderer (never a software GPU adapter). "gpu" uses any
adapter, software included, and fails with ``GPU_INIT_FAILED`` without one; "cpu" opens no graphics
driver."""

_RENDERER_MODES = ("auto", "gpu", "cpu")

QUALITY_LEVELS = ("very-low", "low", "medium", "high", "very-high")
"""The engine's five quality levels, lowest first."""

STDERR_TAIL_BYTES = 16_384
"""Bytes of the host's standard error an error keeps."""

_HOST_SHUTDOWN_TIMEOUT = 5.0

_CANCEL_TIMEOUT = 5.0

# A host draining at each try would otherwise take the request from host to host forever.
_MAX_RESENDS = 3

_POLL_SECONDS = 0.05

_STDERR_QUIET_SECONDS = 0.05

_STDERR_SETTLE_SECONDS = 1.0

_HOST_RELEASES_URL = "https://github.com/alpha2-sh/alpha2/releases"

_request_sequence = itertools.count()


class ProxyFile(TypedDict, total=False):
    """A proxy made from a source's original."""

    path: str | os.PathLike[str]
    made_from: dict[str, Any]
    """Evidence of the original it was made from (a ``MediaFingerprint``), when known."""
    color_pipeline: str
    """The color pipeline baked into it, when it was tone-mapped."""


class ExtractPlugin(TypedDict, total=False):
    """A plugin for :meth:`HostClient.extract`, by manifest."""

    manifest: str
    """An ``https:`` URL, a ``file:`` URL or a path on the host's file system."""
    sha256: str
    """SHA-256 of the manifest's bytes."""
    allow_native: bool
    """Lets a native artifact run, with the host process's rights."""
    force_wasm: bool
    """The wasm artifact even where a native one is allowed."""


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
    decoder_session_bytes: int
    """Bounds how many video decoder sessions an export keeps open, by the host's estimate."""
    scale: float
    """A fraction in (0, 1] every budget is scaled by once resolved."""


_MEMORY_FIELDS = {
    "in_flight_bytes": "inFlightBytes",
    "decode_queue_bytes": "decodeQueueBytes",
    "reverse_cache_bytes": "reverseCacheBytes",
    "audio_window_bytes": "audioWindowBytes",
    "provider_source_bytes": "providerSourceBytes",
    "decoder_session_bytes": "decoderSessionBytes",
}


@dataclass(frozen=True)
class ExportProgress:
    """One progress report of an export."""

    phase: Literal["frames", "finalizing"]
    """``frames`` while encoding, ``finalizing`` while the output is flushed. The export is complete
    only when the call returns."""
    completed: int
    """Frames encoded so far; at most ``total``."""
    total: int
    """Frames in the export."""


ProgressCallback = Callable[[ExportProgress], object]

PathLike = str | os.PathLike[str]


def _invalid(message: str) -> Alpha2HostError:
    return Alpha2HostError("INVALID_INPUT", message)


def _check_video_decode(field: str, value: object) -> None:
    if value is not None and value not in _VIDEO_DECODE_MODES:
        raise _invalid(f"{field} must be software, auto, or hardware-required")


def _check_renderer(field: str, value: object) -> None:
    if value is not None and value not in _RENDERER_MODES:
        raise _invalid(f"{field} must be auto, gpu, or cpu")


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

    The order is ``host_path``, ``ALPHA2_HOST_PATH``, the ``alpha2-host`` wheel of this package's
    version, the archive :func:`alpha2.install_host` put in the shared cache, then ``PATH``. An
    empty ``ALPHA2_HOST_PATH`` counts as unset.

    Raises :class:`~alpha2.Alpha2HostError`: ``INVALID_INPUT`` for an empty ``host_path``,
    ``HOST_NOT_FOUND`` when an explicit path is not a file or no candidate exists.
    """
    base = Path(cwd) if cwd is not None else Path.cwd()
    if host_path is not None and str(os.fspath(host_path)).strip() == "":
        raise _invalid("host_path must not be empty")
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
    """The wheel-installed ``alpha2-host``, only when it is this package's version (protocols
    differ between versions)."""
    try:
        import alpha2_host  # type: ignore[import-not-found,unused-ignore]
    except ImportError:
        return None
    if getattr(alpha2_host, "__version__", None) != package_version():
        return None
    candidate = Path(alpha2_host.host_path())
    return candidate if _is_file(candidate) else None


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
    except UnicodeError as error:
        return [f"project text is not valid Unicode: {error}"]
    except ValueError as error:
        return [f"project cannot be written as JSON: {error}"]
    try:
        return list(_native.validate_project_json(text))
    except (ValueError, UnicodeError) as error:
        # A lone surrogate (e.g. from surrogate-escaped file names) has no UTF-8 encoding.
        return [f"project text is not valid Unicode: {error}"]


def _project_json(project: Mapping[str, Any]) -> str:
    return json.dumps(project, ensure_ascii=False, allow_nan=False, separators=(",", ":"))


_WINDOWS_ABSOLUTE = re.compile(r"[A-Za-z]:[\\/]|[\\/]{2}")

# A scheme of two or more letters, as the host tells a plugin manifest URL from a path.
_URL_SCHEME = re.compile(r"[A-Za-z][A-Za-z0-9+.-]+:")


def _absolute(text: str) -> str:
    """``text`` with a relative path resolved against the working directory now, since a host
    resolves one against the directory it started in. POSIX keeps ``..`` and links as they are."""
    if sys.platform == "win32":
        return text if _WINDOWS_ABSOLUTE.match(text) else os.path.abspath(text)
    return text if os.path.isabs(text) else os.path.join(os.getcwd(), text)


def _path_record(field: str, value: object, what: str) -> dict[str, str]:
    if value is None:
        return {}
    if not isinstance(value, Mapping):
        raise _invalid(f"{field} must map ids to paths")
    record: dict[str, str] = {}
    for key, path in value.items():
        text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else None
        if not isinstance(key, str) or not isinstance(text, str) or not text:
            raise _invalid(f"{what} for {key} must be a non-empty string")
        record[key] = _absolute(text)
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
            entry: dict[str, Any] = {"path": _absolute(text)}
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
            entries.append(_absolute(text))
        payload[source] = entries
    return payload


def _request_inputs(
    project: Mapping[str, Any],
    assets: object,
    proxies: object,
    frames: object,
    resources: object,
    media: object,
    effect_assets: object,
    fonts: object,
    effects: object,
) -> dict[str, Any]:
    effect_payload = _path_record("effect_assets", effect_assets, "effect asset path")
    resources_payload: dict[str, dict[str, str]] = {}
    if resources is not None:
        if not isinstance(resources, Mapping):
            raise _invalid("resources must map source ids to their files")
        for source, files in resources.items():
            resources_payload[source] = _path_record(f"resources.{source}", files, "resource path")
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
            font_paths.append(_absolute(text))
    for resource in project.get("effect_resources") or []:
        if not effect_payload.get(resource["id"]):
            raise Alpha2HostError(
                "MISSING_INPUT", f"missing effect asset path for resource {resource['id']}"
            )
    return {
        "project": project,
        "assets": _path_record("assets", assets, "asset path"),
        "proxies": _proxies_payload(proxies),
        "frames": _frames_payload(frames),
        "resources": resources_payload,
        "media": media,
        "effectAssets": effect_payload,
        "fonts": font_paths,
        "effects": list(effects or []),
    }


def _flag(field: str, value: object) -> bool:
    if not isinstance(value, bool):
        raise _invalid(f"{field} must be true or false")
    return value


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
    payload: dict[str, Any] = {"directory": _absolute(text)}
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

    The client keeps up to ``concurrency`` hosts and reuses them, one request at a time each. A
    host takes only requests that would start a host with its environment, modes included, so a
    change to ``os.environ`` starts new hosts. :meth:`close`, or leaving a ``with`` block, ends
    them; idle hosts never keep the interpreter from exiting. The binary is resolved now (see
    :func:`resolve_host_binary`). ``video_decode`` and ``renderer`` (see :data:`RendererMode`) apply
    to every request unless it names its own. A client is safe to share between threads.

    ``host_start_timeout`` is how many seconds a started host has to report that it is ready
    (``None`` waits forever). A host held before it runs (Gatekeeper, antivirus, App Control) is
    killed and the request fails with ``HOST_START_TIMEOUT``.

    Raises ``ValueError`` for a ``concurrency`` that is not a positive integer or a
    ``host_start_timeout`` that is not above zero, and :class:`~alpha2.Alpha2HostError` when
    ``video_decode`` or ``renderer`` is not a mode (``INVALID_INPUT``) or the binary cannot be
    found.
    """

    def __init__(
        self,
        *,
        host_path: PathLike | None = None,
        cache_directory: PathLike | None = None,
        concurrency: int = 1,
        video_decode: VideoDecodeMode | None = None,
        renderer: RendererMode | None = None,
        host_start_timeout: float | None = _host_start.DEFAULT_HOST_START_TIMEOUT,
    ) -> None:
        if not _safe_integer(concurrency) or concurrency < 1:
            raise ValueError("concurrency must be a positive integer")
        if host_start_timeout is not None and (
            isinstance(host_start_timeout, bool)
            or not isinstance(host_start_timeout, (int, float))
            or not host_start_timeout > 0
        ):
            raise ValueError("host_start_timeout must be a number of seconds above zero, or None")
        _check_video_decode("video_decode", video_decode)
        _check_renderer("renderer", renderer)
        self.host_path: Path = resolve_host_binary(host_path, cache_directory=cache_directory)
        self.concurrency = concurrency
        self.video_decode: VideoDecodeMode | None = video_decode
        self.renderer: RendererMode | None = renderer
        self.host_start_timeout: float | None = host_start_timeout
        self._pool = _Pool(self.host_path, concurrency)
        self._probes: dict[str, _Probe] = {}
        self._probes_lock = threading.Lock()
        _clients.add(self)

    def capabilities(
        self,
        *,
        video_decode: VideoDecodeMode | None = None,
        renderer: RendererMode | None = None,
        cancel: threading.Event | None = None,
    ) -> dict[str, Any]:
        """What the host can decode, render and encode, for the client's modes or these.

        Concurrent calls for a pair of modes share one probe, and a successful report is cached on
        this client for those modes and the environment it ran in. ``cancel`` withdraws this call
        (:class:`~alpha2.Alpha2AbortError`); the probe continues while another call waits for it.
        """
        _check_video_decode("video_decode", video_decode)
        _check_renderer("renderer", renderer)
        if cancel is not None and cancel.is_set():
            raise Alpha2AbortError()
        mode = video_decode if video_decode is not None else self.video_decode
        drawn_by = renderer if renderer is not None else self.renderer
        key = _HostModes(mode, drawn_by).key()
        with self._probes_lock:
            probe = self._probes.get(key)
            if probe is None:
                probe = _Probe()
                self._probes[key] = probe
                threading.Thread(
                    target=self._run_probe, args=(key, probe, mode, drawn_by), daemon=True
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
            # The last withdrawn call (or Ctrl-C) cancels the probe.
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

    def _run_probe(
        self,
        key: str,
        probe: _Probe,
        mode: VideoDecodeMode | None,
        renderer: RendererMode | None,
    ) -> None:
        try:
            probe.result = self._pool.request(
                "capabilities",
                {},
                _HostModes(mode, renderer, self.host_start_timeout),
                probe.cancel,
                None,
            )
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
        return self._pool.request(  # type: ignore[no-any-return]
            "conform",
            payload,
            _HostModes(self.video_decode, self.renderer, self.host_start_timeout),
            cancel,
            None,
        )

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
        alpha: bool = False,
        bit_depth: Literal[8, 10] | None = None,
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
        renderer: RendererMode | None = None,
        overwrite: bool = False,
        on_progress: ProgressCallback | None = None,
        cancel: threading.Event | None = None,
    ) -> ExportResult:
        """Renders and encodes ``project`` to ``output`` and returns the host's export result.

        ``assets`` maps source ids to files; missing, offline or mismatched media fails with
        :class:`~alpha2.Alpha2ConformError`. The range is ``start_ticks`` to ``end_ticks`` (default:
        the project's end), widened to the ``frame_rate`` grid (default: the project's). The
        ``output`` extension (``.mp4``, ``.m4v``, ``.mov``, ``.mkv`` or ``.webm``) selects the
        container and must agree with ``container`` if given; absent codecs use the container's
        defaults. ``alpha`` keeps transparency (VP9 in WebM or Matroska, ``prores`` 4444 in MOV);
        ``bit_depth`` 10 writes 10-bit video (HEVC, AV1, VP9, ``prores``).

        An existing ``output`` is refused unless ``overwrite`` is true, in which case it is replaced
        only when the export completes. An ``output`` that is also an input is always refused.

        ``on_progress`` receives each :class:`ExportProgress` on the calling thread; an exception
        it raises fails the export with ``PROGRESS_CALLBACK_FAILED``. Setting ``cancel`` or Ctrl-C
        cancels the export and removes the partial file; the call then raises
        :class:`~alpha2.Alpha2AbortError` or re-raises the ``KeyboardInterrupt``.

        Raises :class:`~alpha2.Alpha2ProjectValidationError` or an ``INVALID_INPUT``
        :class:`~alpha2.Alpha2HostError` before a host starts when the project or request is
        invalid, and :class:`~alpha2.Alpha2HostError` otherwise. Under the ``auto`` decode mode, a
        hardware decoder stall (``DECODE_STALLED``) retries once with software decoding, restarting
        progress. ``video_decode`` and ``renderer`` override the client's; a runtime WGSL effect
        fails on the CPU renderer with ``EFFECT_ERROR``.

        ``project`` may be a :class:`~alpha2.compose.Compiled` spec, whose ``assets``, ``fonts`` and
        ``loudness`` become defaults.

        Returns an :class:`~alpha2.results.ExportResult` with the host protocol's keys.
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
        inputs = _request_inputs(
            project, assets, proxies, frames, resources, media, effect_assets, fonts, effects
        )
        _check_video_decode("video_decode", video_decode)
        _check_renderer("renderer", renderer)
        # Apply the engine's container policy before starting a host.
        for field, value in (
            ("container", container),
            ("video_codec", video_codec),
            ("audio_codec", audio_codec),
        ):
            _optional_name(field, value)
        if not isinstance(alpha, bool):
            raise _invalid("alpha must be true or false")
        if bit_depth is not None and bit_depth not in (8, 10):
            raise _invalid(f"bit_depth: {bit_depth!r} is not 8 or 10")
        try:
            resolved_container, resolved_codec, _ = _native.resolve_export_format(
                container, os.path.splitext(output_text)[1][1:], video_codec, audio_codec
            )
            _native.resolve_pixels(resolved_codec, resolved_container, bit_depth, alpha)
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
            **inputs,
            "startTicks": start_ticks,
            "endTicks": end_ticks,
            "frameRate": rate,
            "includeAudio": include_audio,
            "inputPolicy": input_policy,
            "strategy": strategy,
            "output": _absolute(output_text),
            "overwrite": _flag("overwrite", overwrite),
        }
        named: tuple[tuple[str, object], ...] = (
            ("container", container),
            ("videoCodec", video_codec),
            ("alpha", alpha or None),
            ("bitDepth", bit_depth),
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
        drawn_by = renderer if renderer is not None else self.renderer
        try:
            return self._pool.request(  # type: ignore[no-any-return]
                "export",
                payload,
                _HostModes(mode, drawn_by, self.host_start_timeout),
                cancel,
                on_progress,
            )
        except Alpha2HostError as stall:
            # A hardware decoder stuck in the driver cannot be recovered in-process, so rerun with
            # software decode, on a host of that mode. Software stalls would recur and are not
            # retried.
            if (
                stall.code != "DECODE_STALLED"
                or stall.failure.get("resource") != "gpu"
                or (mode or os.environ.get("ALPHA2_VIDEO_DECODE")) == "hardware-required"
            ):
                raise
            try:
                return self._pool.request(  # type: ignore[no-any-return]
                    "export",
                    payload,
                    _HostModes("software", drawn_by, self.host_start_timeout),
                    cancel,
                    on_progress,
                )
            except Alpha2HostError as retry_error:
                raise _record_decode_stall(retry_error, stall) from stall

    def stills(
        self,
        project: Mapping[str, Any] | Compiled,
        stills: Sequence[tuple[Time, PathLike | None]],
        *,
        assets: Mapping[str, PathLike] | None = None,
        proxies: Mapping[str, Sequence[ProxyFile]] | None = None,
        frames: Mapping[str, Sequence[PathLike | None]] | None = None,
        resources: Mapping[str, Mapping[str, PathLike]] | None = None,
        media: Literal["original", "proxies"] = "original",
        effect_assets: Mapping[str, PathLike] | None = None,
        fonts: Sequence[PathLike] | None = None,
        effects: Sequence[Mapping[str, Any]] | None = None,
        size: tuple[int, int] | None = None,
        alpha: bool = False,
        overwrite: bool = False,
        video_decode: VideoDecodeMode | None = None,
        renderer: RendererMode | None = None,
        cancel: threading.Event | None = None,
    ) -> StillResult:
        """Renders frames of ``project`` to PNG images, each as an export renders that frame.

        ``stills`` pairs 1 to 256 times (seconds, or exact :class:`~alpha2.Ticks`) with PNG paths,
        or with ``None`` to get the PNG back as bytes. Each still is the frame-grid frame holding
        its time (``frameTime`` in ticks), as 8-bit sRGB RGBA before video encoding. ``size``
        (width, height, 1 to 16384 each) overrides the canvas size. ``alpha`` omits the project
        background. ``overwrite`` works as in :meth:`export`.

        Other options are :meth:`export`'s. Returns ``stills`` (``output``, ``frameTime``,
        ``width``, ``height``, in request order; ``output`` is ``None`` and ``png`` holds the
        image for a still without a path), ``renderer`` and ``timings``.
        """
        from .compose import Compiled  # here: compose imports this module

        if isinstance(project, Compiled):
            compiled = project
            project = compiled.project
            assets = compiled.assets if assets is None else assets
            fonts = compiled.fonts if fonts is None else fonts
        problems = _project_problems(project)
        if problems:
            raise Alpha2ProjectValidationError(problems)
        if isinstance(stills, (str, bytes, Mapping)) or not isinstance(stills, Sequence):
            raise _invalid("stills must be a list of (time, output) pairs")
        if not 1 <= len(stills) <= _MAX_STILLS:
            raise _invalid(f"a still request renders 1 to {_MAX_STILLS} stills, not {len(stills)}")
        entries: list[dict[str, Any]] = []
        outputs: dict[str, str] = {}
        for entry in stills:
            if not isinstance(entry, (tuple, list)) or len(entry) != 2:
                raise _invalid("stills must be a list of (time, output) pairs")
            time_value, output = entry
            try:
                at = to_ticks(time_value)
            except ValueError as error:
                raise _invalid(f"a still's time: {error}") from None
            if at < 0:
                raise _invalid(f"a still's time is from 0, not {time_value!r}")
            if output is None:
                entries.append({"time": at})
                continue
            output_text = os.fspath(output) if isinstance(output, (str, os.PathLike)) else None
            if not isinstance(output_text, str) or not output_text:
                raise _invalid("a still's output must not be empty")
            absolute = _absolute(output_text)
            # Windows and macOS file systems are case- and normalization-insensitive.
            key = unicodedata.normalize("NFC", os.path.normpath(absolute)).lower()
            if key in outputs:
                raise _invalid(f"two stills write one file: {outputs[key]} and {output_text}")
            outputs[key] = output_text
            entries.append({"time": at, "output": absolute})
        payload: dict[str, Any] = {
            **_request_inputs(
                project, assets, proxies, frames, resources, media, effect_assets, fonts, effects
            ),
            "stills": entries,
            "alpha": _flag("alpha", alpha),
            "overwrite": _flag("overwrite", overwrite),
        }
        if size is not None:
            if (
                not isinstance(size, (tuple, list))
                or len(size) != 2
                or not all(
                    _safe_integer(side) and 1 <= side <= _MAX_STILL_DIMENSION for side in size
                )
            ):
                raise _invalid(
                    f"size must be (width, height), each 1 to {_MAX_STILL_DIMENSION} pixels"
                )
            payload["size"] = {"width": size[0], "height": size[1]}
        _check_video_decode("video_decode", video_decode)
        _check_renderer("renderer", renderer)
        mode = video_decode if video_decode is not None else self.video_decode
        drawn_by = renderer if renderer is not None else self.renderer
        result, attachments = self._pool.request_with_attachments(
            "still",
            payload,
            _HostModes(mode, drawn_by, self.host_start_timeout),
            cancel,
            None,
        )
        for frame in result["stills"]:
            index = frame.get("attachment")
            if index is None:
                continue
            if (
                not _safe_integer(index)
                or not 0 <= index < len(attachments)
                or attachments[index]["type"] != "image/png"
            ):
                raise Alpha2HostError(
                    "INVALID_RESPONSE",
                    f"a still names attachment {index!r}, which the response has no PNG for",
                )
            frame["png"] = attachments[index]["data"]
        return result  # type: ignore[no-any-return]

    def still(
        self,
        project: Mapping[str, Any] | Compiled,
        time: Time,
        output: PathLike | None = None,
        **options: Any,
    ) -> StillResult:
        """One frame of ``project`` at ``time`` to the PNG ``output``, or to ``png`` bytes in the
        result without one: :meth:`stills` of one."""
        return self.stills(project, [(time, output)], **options)

    def extract(
        self,
        project: Mapping[str, Any],
        request: Mapping[str, Any],
        extractor: Any = None,
        *,
        plugin: ExtractPlugin | None = None,
        assets: Mapping[str, PathLike] | None = None,
        proxies: Mapping[str, Sequence[ProxyFile]] | None = None,
        media: Literal["original", "proxies"] = "original",
        fonts: Sequence[PathLike] | None = None,
        on_progress: Callable[[ExtractProgress], object] | None = None,
        cancel: threading.Event | None = None,
    ) -> dict[str, Any]:
        """Runs ``extractor`` (see :class:`alpha2.extract.Extractor`), or the plugin ``plugin``
        names, over ``request``.

        The host decodes and presents the input. A client-side extractor gets it step by step; a
        plugin runs inside the host, which fetches its manifest and files, checks them against
        their pinned hashes and runs wasm sandboxed (native code only with ``allow_native``).
        Returns ``{"dataSets", "timings"}`` with the data sets the session validated,
        ``made_from`` set to the conformed original and, for a plugin, ``made_by.plugin`` naming
        the artifact that ran.

        A client-side extractor's ``request["params"]`` is checked against its
        ``params_schema`` first. Raises with ``INVALID_EMIT`` for output the session refuses,
        ``EXTRACTOR_FAILED`` when a step fails, ``EXTRACTOR_TIMEOUT`` when a client step takes
        longer than the host's step timeout (begin and end have none), a ``PLUGIN_*`` code when
        a plugin is refused, does not load, traps or runs past its limits, and
        :class:`~alpha2.Alpha2AbortError` on ``cancel``.
        """
        if (extractor is None) == (plugin is None):
            raise _invalid("extract runs either a client-side extractor or a plugin")
        problems = _project_problems(project)
        if problems:
            raise Alpha2ProjectValidationError(problems)
        runs: dict[str, Any]
        if extractor is not None:
            descriptor = extractor.describe()
            problem = schema_problem(descriptor.get("params_schema"), request.get("params", {}))
            if problem is not None:
                raise _invalid(f"invalid params: {problem}")
            runs = {"descriptor": descriptor}
        else:
            assert plugin is not None
            manifest, sha256 = plugin.get("manifest"), plugin.get("sha256")
            if not isinstance(manifest, str) or not manifest or not isinstance(sha256, str):
                raise _invalid("plugin needs its manifest and the manifest's sha256")
            runs = {
                "plugin": {
                    "manifest": manifest if _URL_SCHEME.match(manifest) else _absolute(manifest),
                    "sha256": sha256,
                    "allow_native": bool(plugin.get("allow_native", False)),
                    "force_wasm": bool(plugin.get("force_wasm", False)),
                }
            }
        inputs = _request_inputs(project, assets, proxies, None, None, media, None, fonts, None)
        payload = {**inputs, "request": dict(request), **runs}
        return self._pool.request(  # type: ignore[no-any-return]
            "extract",
            payload,
            _HostModes(self.video_decode, self.renderer, self.host_start_timeout),
            cancel,
            None,
            _Extraction(extractor, on_progress),
        )

    def close(self) -> None:
        """Ends the client's idle hosts and returns once they have exited.

        A host running a request ends when that request does. A later request starts new hosts.
        """
        self._pool.close()

    def __enter__(self) -> HostClient:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


_MAX_STILLS = 256
_MAX_STILL_DIMENSION = 16384


def _record_decode_stall(retry_error: Alpha2HostError, stall: Alpha2HostError) -> Alpha2HostError:
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
    while start < len(data) and data[start] & 0xC0 == 0x80:
        start += 1
    text = data[start:].decode("utf-8", errors="replace")
    # Invalid bytes decode to U+FFFD (three bytes), so trim from the front again to fit.
    excess = len(text.encode("utf-8")) - limit
    index = 0
    while excess > 0 and index < len(text):
        excess -= len(text[index].encode("utf-8"))
        index += 1
    return text[index:]


def _ended(returncode: int | None) -> tuple[int | None, str | None]:
    if returncode is not None and returncode < 0 and sys.platform != "win32":
        try:
            return None, signals.Signals(-returncode).name
        except ValueError:
            return None, str(-returncode)
    return returncode, None


@dataclass(frozen=True)
class _HostModes:
    """``None`` modes leave the host's default or its environment's."""

    video_decode: str | None
    renderer: str | None
    start_timeout: float | None = _host_start.DEFAULT_HOST_START_TIMEOUT

    def environment(self) -> dict[str, str]:
        """The environment a host started now would get. The host reads its renderer and decode
        path at startup, so the modes go in it."""
        named = {
            variable: value
            for variable, value in (
                ("ALPHA2_VIDEO_DECODE", self.video_decode),
                ("ALPHA2_RENDERER", self.renderer),
            )
            if value is not None
        }
        return {**os.environ, **named}

    def key(self) -> str:
        return _environment_key(self.environment())


def _environment_key(environment: Mapping[str, str]) -> str:
    """A host reads its whole environment when it starts, so hosts are kept apart by it."""
    digest = hashlib.sha256()
    for name, value in sorted(environment.items()):
        digest.update(f"{name}={value}".encode("utf-8", "surrogateescape") + b"\0")
    return digest.hexdigest()


class _Host:
    """One ``alpha2-host serve`` process of a pool.

    The pool's lock guards ``owner``, ``ready``, ``available``, ``alive`` and ``input_closed``.
    ``running`` belongs to the owner: whether a request it sent may still run there.
    """

    def __init__(self, pool: _Pool, binary: Path, environment: dict[str, str], key: str) -> None:
        self.pool = pool
        self.key = key
        self.oom_kills_at_spawn = _host_start.oom_kill_count()
        self.process = subprocess.Popen(
            [str(binary), "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        self.started_at = time.monotonic()
        self.owner: queue.Queue[tuple[str, Any]] | None = None
        self.ready = False
        self.available = True
        self.alive = True
        self.input_closed = False
        self.running = False
        self.exited = threading.Event()
        self.exit_status: tuple[int | None, str | None] = (None, None)
        self._stderr = bytearray()
        self._stderr_total = 0
        self._stderr_changed = threading.Condition()
        self._writes: queue.Queue[bytes | None] = queue.Queue()
        self._queued = 0
        self._written = 0
        self._readers = [
            threading.Thread(target=self._read_stdout, daemon=True),
            threading.Thread(target=self._read_stderr, daemon=True),
        ]

    def start(self) -> None:
        for thread in self._readers:
            thread.start()
        threading.Thread(target=self._write, daemon=True).start()
        threading.Thread(target=self._wait, daemon=True).start()

    def keep_stderr(self, data: bytes) -> None:
        with self._stderr_changed:
            self._stderr += data
            self._stderr_total += len(data)
            if len(self._stderr) > STDERR_TAIL_BYTES:
                del self._stderr[: len(self._stderr) - STDERR_TAIL_BYTES]
            self._stderr_changed.notify_all()

    def stderr_mark(self) -> int:
        with self._stderr_changed:
            return self._stderr_total

    def stderr_since(self, mark: int) -> str:
        """The tail of what the host wrote to standard error after ``mark``."""
        with self._stderr_changed:
            count = min(self._stderr_total - mark, len(self._stderr))
            data = bytes(self._stderr[len(self._stderr) - count :]) if count > 0 else b""
        return _utf8_tail(data, STDERR_TAIL_BYTES)

    def settle_stderr(self) -> None:
        """Waits, at most a second, until standard error is drained, so the diagnostics the host
        wrote before an answer stay with it."""
        deadline = time.monotonic() + _STDERR_SETTLE_SECONDS
        with self._stderr_changed:
            while time.monotonic() < deadline and self._readers[1].is_alive():
                seen = self._stderr_total
                self._stderr_changed.wait(_STDERR_QUIET_SECONDS)
                if self._stderr_total == seen and not self._stderr_pending():
                    return

    def _stderr_pending(self) -> bool:
        if sys.platform == "win32" or self.process.stderr is None:
            return False
        import select

        try:
            readable, _, _ = select.select([self.process.stderr], [], [], 0)
        except (OSError, ValueError):
            return False
        return bool(readable)

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        try:
            for kind, value in _wire.read(self.process.stdout):
                if isinstance(value, bytes):
                    self.keep_stderr(value)
                else:
                    self.pool.received(self, kind, value)
        except (OSError, ValueError):
            pass

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        try:
            while chunk := self.process.stderr.read1(65_536):  # type: ignore[attr-defined]
                self.keep_stderr(chunk)
        except (OSError, ValueError):
            pass

    def _write(self) -> None:
        """Writes on its own thread so a host that does not read never blocks a caller."""
        stdin = self.process.stdin
        assert stdin is not None
        try:
            while (framed := self._writes.get()) is not None:
                _wire.write(stdin, framed)
                self._written += 1
        except (OSError, ValueError) as error:
            self.pool.received(self, "write-failed", error)
        try:
            stdin.close()
        except (OSError, ValueError):
            pass

    def _wait(self) -> None:
        self.process.wait()
        # A grandchild may hold the pipes open, so wait only until the timeout, and leave open any
        # stream whose reader is still blocked (closing it would wait for that reader).
        deadline = time.monotonic() + _HOST_SHUTDOWN_TIMEOUT
        for thread in self._readers:
            thread.join(max(0.0, deadline - time.monotonic()))
        for thread, stream in zip(
            self._readers, (self.process.stdout, self.process.stderr), strict=True
        ):
            if stream is not None and not thread.is_alive():
                stream.close()
        self.exit_status = _ended(self.process.returncode)
        self.pool.exited(self)
        self.exited.set()

    def send(self, framed: bytes) -> int:
        """Queues a message for the host; only the thread that owns the host sends."""
        self._queued += 1
        self._writes.put(framed)
        return self._queued

    def wrote(self, sent: int) -> bool:
        """Whether the message :meth:`send` numbered ``sent`` has been written in full."""
        return self._written >= sent

    def close_input(self) -> None:
        self._writes.put(None)

    def end_after_input(self) -> None:
        """Terminates, then kills, a host that has not exited within the shutdown timeout of its
        input closing."""
        try:
            self.process.wait(_HOST_SHUTDOWN_TIMEOUT)
            return
        except subprocess.TimeoutExpired:
            self.terminate()
        try:
            self.process.wait(_HOST_SHUTDOWN_TIMEOUT)
        except subprocess.TimeoutExpired:
            self.kill()

    def terminate(self) -> None:
        if self.process.poll() is None:
            try:
                self.process.terminate()
            except OSError:
                pass

    def kill(self) -> None:
        try:
            self.process.kill()
        except OSError:
            pass

    def release_inherited(self, null: int) -> None:
        """Points a forked child's copies of the pipe ends at ``null``, so the host sees the end of
        its input when the parent closes its side. Never signals or waits on the parent's child."""
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                try:
                    os.dup2(null, stream.fileno())
                except (OSError, ValueError):
                    pass
        self.alive = False
        self.available = False
        self.input_closed = True


def _error_from_host(error: dict[str, Any], stderr: str, phase: ExportPhase) -> Alpha2HostError:
    failure = None
    if isinstance(error.get("failure"), dict):
        failure = {**error["failure"], "message": error["message"], "stderr": stderr}
    options: dict[str, Any] = {"failure": failure, "stderr": stderr, "phase": phase}
    if error["code"] == "GPU_INIT_FAILED":
        return Alpha2GpuInitializationError(error["message"], **options)
    if error["code"] == "CONFORM_FAILED" and error.get("details") is not None:
        return Alpha2ConformError(error["message"], error["details"], **options)
    details = error.get("details")
    if (
        error["code"] == "OUTPUT_PUBLISH_FAILED"
        and isinstance(details, dict)
        and isinstance(details.get("keptPath"), str)
    ):
        options["kept_path"] = details["keptPath"]
    return Alpha2HostError(error["code"], error["message"], **options)


class _Pool:
    """The hosts of one client: at most ``limit`` processes, each running one request at a time.

    Host threads report messages and exits through :meth:`received` and :meth:`exited`, which keep
    the pool's view of each host current; the request's own thread reads its events from a queue.
    """

    def __init__(self, binary: Path, limit: int) -> None:
        self.binary = binary
        self.limit = limit
        self._condition = threading.Condition()
        self._hosts: list[_Host] = []
        self._starting = 0
        self._waiting: collections.deque[object] = collections.deque()
        _pools.add(self)

    def forget_after_fork(self, null: int) -> None:
        """Drops what a forked child inherited: no thread feeds its hosts and a parent thread may
        have held the lock, so the child starts its own hosts."""
        self._condition = threading.Condition()
        for host in self._hosts:
            host.release_inherited(null)
        self._hosts = []
        self._waiting = collections.deque()
        self._starting = 0

    def received(self, host: _Host, kind: str, value: Any) -> None:
        with self._condition:
            if kind == "message":
                if value.get("type") == "state":
                    if value["state"] == "ready":
                        host.ready = True
                    else:
                        host.available = False
                elif value.get("type") == "fatal" or value.get("hostState") == "draining":
                    host.available = False
            elif kind in ("invalid", "write-failed"):
                host.available = False
            if host.owner is not None:
                host.owner.put((kind, value))
            elif not host.available:
                self._retire(host)
            self._condition.notify_all()

    def exited(self, host: _Host) -> None:
        with self._condition:
            host.alive = False
            host.available = False
            if host in self._hosts:
                self._hosts.remove(host)
            if host.owner is not None:
                host.owner.put(("exit", host.exit_status))
            self._condition.notify_all()

    def _retire(self, host: _Host) -> None:
        """Closes the input of a host that runs nothing; the pool's lock is held."""
        host.available = False
        if host.alive and not host.input_closed:
            host.input_closed = True
            host.close_input()
            threading.Thread(target=host.end_after_input, daemon=True).start()

    def close(self) -> None:
        with self._condition:
            idle = [host for host in self._hosts if host.owner is None]
            for host in self._hosts:
                host.available = False
            for host in idle:
                self._retire(host)
            self._condition.notify_all()
        for host in idle:
            host.exited.wait()

    def _acquire(
        self,
        modes: _HostModes,
        events: queue.Queue[tuple[str, Any]],
        cancel: threading.Event | None,
    ) -> tuple[_Host, bool]:
        """An idle host started with the environment the request would start one with, else a new
        one, ending an idle host of another environment to make room. Requests wait first come,
        first served. Returns the host and whether it is new (not yet ``ready``)."""
        environment = modes.environment()
        key = _environment_key(environment)
        ticket = object()
        with self._condition:
            self._waiting.append(ticket)
            try:
                while True:
                    if cancel is not None and cancel.is_set():
                        raise Alpha2AbortError()
                    if self._waiting[0] is ticket:
                        idle = [
                            host
                            for host in self._hosts
                            if host.available and host.ready and host.owner is None
                        ]
                        match = next((host for host in idle if host.key == key), None)
                        if match is not None:
                            match.owner = events
                            return match, False
                        alive = sum(1 for host in self._hosts if host.alive)
                        if alive + self._starting < self.limit:
                            self._starting += 1
                            break
                        if idle:
                            self._retire(idle[0])
                    self._condition.wait(_POLL_SECONDS)
            finally:
                self._waiting.remove(ticket)
                self._condition.notify_all()
        try:
            host = self._spawn(environment, key)
        except BaseException:
            with self._condition:
                self._starting -= 1
                self._condition.notify_all()
            raise
        with self._condition:
            self._starting -= 1
            host.owner = events
            self._hosts.append(host)
        host.start()
        return host, True

    def _spawn(self, environment: dict[str, str], key: str) -> _Host:
        binary = self.binary
        # Windows starts a batch file only through a shell, which this never asks for.
        if sys.platform == "win32" and binary.suffix.lower() in (".bat", ".cmd"):
            raise Alpha2HostError(
                "HOST_NOT_EXECUTABLE",
                f"alpha2-host must be the executable, not a batch file: {binary}",
            )
        if not os.access(binary, os.F_OK if sys.platform == "win32" else os.X_OK):
            raise Alpha2HostError("HOST_NOT_EXECUTABLE", f"cannot execute alpha2-host: {binary}")
        # macOS would hold a quarantined host on an approval prompt nobody may answer.
        if sys.platform == "darwin":
            refusal = _quarantine_check(binary)
            if refusal is not None:
                raise Alpha2HostError("HOST_QUARANTINED", refusal)
        try:
            return _Host(self, binary, environment, key)
        except OSError as cause:
            refused = _host_start.spawn_refusal(binary, cause)
            if refused is not None:
                raise Alpha2HostError(*refused) from cause
            raise Alpha2HostError(
                "HOST_START_FAILED", f"could not start alpha2-host: {cause}"
            ) from cause

    def _release(self, host: _Host) -> None:
        if host.running:
            # A host that may still run the request never serves another.
            self._end(host, kill=True)
            host.running = False
        with self._condition:
            host.owner = None
            if not host.available:
                self._retire(host)
            self._condition.notify_all()

    def _end(self, host: _Host, *, kill: bool = False) -> None:
        """Ends a host that may be running a request, and returns once its process has exited."""
        with self._condition:
            host.available = False
            if not host.input_closed:
                host.input_closed = True
                host.close_input()
        if kill:
            host.kill()
        else:
            host.terminate()
            try:
                host.process.wait(_HOST_SHUTDOWN_TIMEOUT)
            except subprocess.TimeoutExpired:
                host.kill()
        host.process.wait()

    def request(
        self,
        operation: str,
        payload: dict[str, Any],
        modes: _HostModes,
        cancel: threading.Event | None,
        on_progress: ProgressCallback | None,
        extraction: _Extraction | None = None,
    ) -> Any:
        return self.request_with_attachments(
            operation, payload, modes, cancel, on_progress, extraction
        )[0]

    def request_with_attachments(
        self,
        operation: str,
        payload: dict[str, Any],
        modes: _HostModes,
        cancel: threading.Event | None,
        on_progress: ProgressCallback | None,
        extraction: _Extraction | None = None,
    ) -> tuple[Any, list[dict[str, Any]]]:
        """The response's result, and its attachments with their ``data``."""
        if cancel is not None and cancel.is_set():
            raise Alpha2AbortError()
        request_id = f"python-{os.getpid()}-{int(time.time() * 1000)}-{next(_request_sequence)}"
        try:
            body = _wire.encode(create_request(operation, request_id, payload))
        except (TypeError, ValueError) as cause:
            raise Alpha2HostError(
                "INVALID_INPUT", f"the {operation} request cannot be written as JSON: {cause}"
            ) from cause
        # The host would create its GPU device before refusing an oversized request.
        if len(body) > _wire.MAX_REQUEST_BYTES:
            raise _invalid(
                f"the {operation} request is {len(body)} bytes of JSON; the host reads at most "
                f"{_wire.MAX_REQUEST_BYTES}"
            )
        framed = _wire.frame(body)
        resends = 0
        while True:
            events: queue.Queue[tuple[str, Any]] = queue.Queue()
            host, fresh = self._acquire(modes, events, cancel)
            try:
                return self._run(
                    host, fresh, events, framed, request_id, modes, cancel, on_progress, extraction
                )
            except Alpha2HostError as error:
                if error.code != "HOST_RETIRING" or resends >= _MAX_RESENDS:
                    raise
                resends += 1
            finally:
                self._release(host)

    def _run(
        self,
        host: _Host,
        fresh: bool,
        events: queue.Queue[tuple[str, Any]],
        framed: bytes,
        request_id: str,
        modes: _HostModes,
        cancel: threading.Event | None,
        on_progress: ProgressCallback | None,
        extraction: _Extraction | None = None,
    ) -> tuple[Any, list[dict[str, Any]]]:
        binary = self.binary
        last_phase: str | None = None

        def phase_now() -> ExportPhase:
            if last_phase == "finalizing":
                return "mux"
            return "render" if last_phase == "frames" else "prepare"

        # A new host's start belongs to the request, so its stderr counts from the spawn.
        stderr_mark = 0
        oom_kills_before = host.oom_kills_at_spawn
        sent = 0

        def send() -> None:
            nonlocal sent, stderr_mark, oom_kills_before
            if not fresh:
                stderr_mark = host.stderr_mark()
            oom_kills_before = _host_start.oom_kill_count()
            host.running = True
            sent = host.send(framed)

        if not fresh:
            send()
        # A protocol error is built once stderr has settled, since it may be read after the answer.
        outcome: tuple[bool, Any] | None = None
        exit_status: tuple[int | None, str | None] | None = None
        write_failed_at: float | None = None
        write_error: BaseException | None = None
        start_timed_out = False
        cancelled = False
        try:
            while outcome is None and exit_status is None:
                if cancel is not None and cancel.is_set():
                    outcome = (False, Alpha2AbortError())
                    cancelled = True
                    break
                if (
                    not sent
                    and modes.start_timeout is not None
                    and time.monotonic() - host.started_at > modes.start_timeout
                ):
                    host.kill()
                    host.process.wait()
                    start_timed_out = True
                    outcome = (
                        False,
                        Alpha2HostError(
                            "HOST_START_TIMEOUT",
                            _host_start.start_timeout_message(binary, modes.start_timeout),
                        ),
                    )
                    break
                if (
                    write_failed_at is not None
                    and time.monotonic() - write_failed_at > _HOST_SHUTDOWN_TIMEOUT
                ):
                    outcome = (False, Alpha2HostError("HOST_WRITE_FAILED", str(write_error)))
                    break
                try:
                    kind, value = events.get(timeout=_POLL_SECONDS)
                except queue.Empty:
                    continue
                if kind == "exit":
                    exit_status = value
                    host.running = False
                    break
                if kind == "write-failed":
                    # The host's fatal message and exit, which arrive later, name the cause.
                    write_error = value
                    write_failed_at = time.monotonic()
                    continue
                if kind == "invalid":
                    outcome = (
                        False,
                        Alpha2HostError(
                            "INVALID_RESPONSE",
                            f"alpha2-host wrote an invalid message: {value}",
                            phase=phase_now(),
                        ),
                    )
                    break
                message: dict[str, Any] = value
                if message.get("type") == "state":
                    if not sent and message["state"] == "ready":
                        send()
                    elif not sent:
                        outcome = (
                            False,
                            (
                                {
                                    "code": "HOST_RETIRING",
                                    "message": "alpha2-host began draining before it took the "
                                    f"request ({message['reason']})",
                                },
                                phase_now(),
                            ),
                        )
                        break
                    continue
                if message.get("type") == "fatal":
                    outcome = (False, (message["error"], phase_now()))
                    host.running = False
                    break
                if message.get("type") == "extract.step":
                    if message["requestId"] != request_id or extraction is None:
                        continue
                    ack = take_step(extraction.extractor, message, message.get("attachments", []))
                    seq = {"seq": message["seq"]}
                    try:
                        body = _wire.encode(create_request("extract.ack", request_id, seq | ack))
                    except (TypeError, ValueError) as cause:
                        failed = {"error": {"message": f"the step's emits cannot be sent: {cause}"}}
                        body = _wire.encode(create_request("extract.ack", request_id, seq | failed))
                    host.send(_wire.frame(body))
                    continue
                if message.get("type") == "progress" and message.get("phase") == "extract":
                    if message["requestId"] != request_id or extraction is None:
                        continue
                    if extraction.on_progress is not None:
                        try:
                            extraction.on_progress(
                                ExtractProgress(
                                    message.get("pass", 0), message["completed"], message["total"]
                                )
                            )
                        except Exception as cause:  # noqa: BLE001 - reported as the request's failure
                            error = Alpha2HostError(
                                "PROGRESS_CALLBACK_FAILED",
                                "on_progress callback raised",
                                phase=phase_now(),
                            )
                            error.__cause__ = cause
                            outcome = (False, error)
                            cancelled = True
                            break
                    continue
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
                            cancelled = True
                            break
                    continue
                # One request runs at a time, so an answer without a readable id is ours.
                if not sent or message["requestId"] not in (request_id, UNREAD_REQUEST_ID):
                    continue
                outcome = (
                    (True, (message["result"], message.get("attachments", [])))
                    if message["ok"]
                    else (False, (message["error"], phase_now()))
                )
                host.running = False
            if cancelled:
                self._cancel(host, events, request_id, sent)
                host.running = False
        except KeyboardInterrupt:
            # Ctrl-C cancels like an abort, so the partial file is removed; a second one kills.
            if not cancelled:
                try:
                    self._cancel(host, events, request_id, sent)
                except KeyboardInterrupt:
                    self._end(host, kill=True)
                    host.running = False
                    raise
                host.running = False
                raise
            self._end(host, kill=True)
            host.running = False
            raise
        except BaseException:
            # Anything else leaving mid-request (a signal handler, a task queue's time limit)
            # cancels the same way, so nothing is published after the call failed.
            if host.running:
                try:
                    self._cancel(host, events, request_id, sent)
                except BaseException:  # noqa: BLE001 - the original is re-raised
                    self._end(host, kill=True)
                host.running = False
            raise

        signal_name = exit_status[1] if exit_status is not None else None
        # A host held before it ran may have been quarantined meanwhile; check again.
        if (
            sys.platform == "darwin"
            and not sent
            and (start_timed_out or (outcome is None and signal_name == "SIGKILL"))
        ):
            refusal = _quarantine_check(binary, again=True)
            if refusal is not None:
                raise Alpha2HostError("HOST_QUARANTINED", refusal)
        if outcome is not None and outcome[0]:
            return outcome[1]  # type: ignore[no-any-return]
        if outcome is not None:
            failed = outcome[1]
            if isinstance(failed, tuple):
                protocol_error, phase = failed
                host.settle_stderr()
                raise _error_from_host(protocol_error, host.stderr_since(stderr_mark), phase)
            raise failed
        assert exit_status is not None
        code = exit_status[0]
        tail = host.stderr_since(stderr_mark)
        raise _exit_error(code, signal_name, tail, oom_kills_before, phase_now())

    def _cancel(
        self,
        host: _Host,
        events: queue.Queue[tuple[str, Any]],
        request_id: str,
        sent: int,
    ) -> None:
        """Cancels the request and waits for its answer, which comes after partial output is
        removed, at most :data:`_CANCEL_TIMEOUT`; a host that does not answer is ended.

        ``sent`` numbers the request's message, 0 when it was not sent.
        """
        if not sent:
            with self._condition:
                self._retire(host)
            return
        if not host.wrote(sent):
            # The host cannot have started a request it has not read in full.
            self._end(host)
            return
        host.send(_wire.frame(_wire.encode(create_cancel(request_id))))
        deadline = time.monotonic() + _CANCEL_TIMEOUT
        while time.monotonic() < deadline:
            try:
                kind, value = events.get(timeout=_POLL_SECONDS)
            except queue.Empty:
                continue
            if kind == "exit":
                return
            if kind == "invalid":
                break
            if kind != "message":
                continue
            if value.get("type") == "fatal":
                return
            if "ok" in value and value["requestId"] in (request_id, UNREAD_REQUEST_ID):
                return
        self._end(host)


@dataclass(frozen=True)
class _Extraction:
    """How a request with a client-side extractor takes its steps and reports its passes."""

    extractor: Any
    on_progress: Callable[[ExtractProgress], object] | None


def _exit_error(
    code: int | None,
    signal_name: str | None,
    tail: str,
    oom_kills_before: int | None,
    phase: ExportPhase,
) -> Alpha2HostError:
    """What a host's exit before its answer says, for the request it ran."""
    ended = describe_exit(code, signal_name)
    detail = tail.strip() or ended
    exit_options: dict[str, Any] = {
        "stderr": tail,
        "exit_code": code,
        "signal": signal_name,
        "has_exit": True,
    }
    if code == 70 or "GPU_INIT_FAILED" in detail:
        return Alpha2GpuInitializationError(detail, **exit_options)
    if windows_policy_blocked(code):
        # A policy refusal repeats on the same machine, so no retry is suggested.
        assert code is not None
        return Alpha2HostError(
            "HOST_BLOCKED",
            _host_start.blocked_message(_host_start.windows_status(code)),
            **exit_options,
        )
    if loader_refused(code, tail):
        # A loader failure (missing or mismatched FFmpeg) repeats, so no retry is suggested.
        return Alpha2HostError(
            "HOST_LOAD_FAILED",
            f"alpha2-host could not be loaded: {detail}{_host_start.glibc_hint(tail)}",
            **exit_options,
        )
    oom_kills_after = _host_start.oom_kill_count()
    if (
        signal_name == "SIGKILL"
        and oom_kills_before is not None
        and oom_kills_after is not None
        and oom_kills_after > oom_kills_before
    ):
        # SIGKILL plus a rise in the cgroup's OOM kills: a smaller ``memory.scale`` may help.
        return Alpha2HostError(
            "HOST_OOM_KILLED",
            f"the kernel's out-of-memory killer ended alpha2-host: {detail}",
            failure={
                "code": "OUT_OF_MEMORY",
                "message": f"the kernel's out-of-memory killer ended alpha2-host ({ended})",
                "origin": "python",
                "operation": "export",
                "backend": "native",
                "phase": phase,
                "resource": "cpu",
                "stderr": tail,
                "exitCode": code,
                "signal": signal_name,
                "ntstatus": ntstatus_name(code),
            },
            stderr=tail,
            exit_code=code,
            signal=signal_name,
        )
    # A SIGKILL without an OOM-kill rise may be external, so no cause is claimed.
    failure = {
        "code": "HOST_EXITED",
        "message": f"alpha2-host exited before responding ({ended})",
        "origin": "python",
        "operation": "export",
        "backend": "native",
        "phase": phase,
        "resource": "unknown",
        "stderr": tail,
        "exitCode": code,
        "signal": signal_name,
        "ntstatus": ntstatus_name(code),
    }
    return Alpha2HostError(
        "HOST_EXITED",
        f"alpha2-host exited before responding: {detail}",
        failure=failure,
        stderr=tail,
        exit_code=code,
        signal=signal_name,
    )


_quarantine_checks: dict[str, str | None] = {}
_quarantine_lock = threading.Lock()


def _quarantine_check(binary: Path, *, again: bool = False) -> str | None:
    """Cached per process; ``again`` checks afresh."""
    key = str(binary)
    with _quarantine_lock:
        if not again and key in _quarantine_checks:
            return _quarantine_checks[key]
    try:
        refusal = quarantine_refusal(binary)
    except OSError:
        refusal = None
    with _quarantine_lock:
        _quarantine_checks[key] = refusal
    return refusal


_shared_clients: dict[tuple[int, str], HostClient] = {}
_shared_lock = threading.Lock()

_clients: weakref.WeakSet[HostClient] = weakref.WeakSet()
_pools: weakref.WeakSet[_Pool] = weakref.WeakSet()


def _after_fork_in_child() -> None:
    """Makes inherited clients usable in a forked child without taking a lock a parent thread may
    have held at the fork."""
    global _shared_lock, _quarantine_lock
    _shared_lock = threading.Lock()
    _quarantine_lock = threading.Lock()
    _shared_clients.clear()
    for client in list(_clients):
        client._probes_lock = threading.Lock()
        # A probe still running has no thread in the child; a finished report stays cached.
        client._probes = {
            key: probe
            for key, probe in client._probes.items()
            if probe.done.is_set() and probe.error is None
        }
    null = os.open(os.devnull, os.O_RDWR)
    try:
        for pool in list(_pools):
            pool.forget_after_fork(null)
    finally:
        os.close(null)


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_after_fork_in_child)


def _shared_client(
    host_path: PathLike | None, cache_directory: PathLike | None, concurrency: int
) -> HostClient:
    """Keyed by binary and ``concurrency`` only, so calls with different modes share the bound."""
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
    renderer: RendererMode | None = None,
    cancel: threading.Event | None = None,
) -> dict[str, Any]:
    """:meth:`HostClient.capabilities` on a client shared by every module-level call with the same
    resolved binary and ``concurrency``."""
    return _shared_client(host_path, cache_directory, concurrency).capabilities(
        video_decode=video_decode, renderer=renderer, cancel=cancel
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
) -> ExportResult:
    """:meth:`HostClient.export` on a shared client (see :func:`capabilities`)."""
    return _shared_client(host_path, cache_directory, concurrency).export(
        project, output, **options
    )


def stills(
    project: Mapping[str, Any] | Compiled,
    stills: Sequence[tuple[Time, PathLike | None]],
    *,
    client: HostClient | None = None,
    host_path: PathLike | None = None,
    cache_directory: PathLike | None = None,
    concurrency: int = 1,
    **options: Any,
) -> StillResult:
    """:meth:`HostClient.stills` on ``client``, else on a shared one (see :func:`capabilities`)."""
    runner = client or _shared_client(host_path, cache_directory, concurrency)
    return runner.stills(project, stills, **options)


def still(
    project: Mapping[str, Any] | Compiled,
    time: Time,
    output: PathLike | None = None,
    *,
    client: HostClient | None = None,
    host_path: PathLike | None = None,
    cache_directory: PathLike | None = None,
    concurrency: int = 1,
    **options: Any,
) -> StillResult:
    """:meth:`HostClient.still` on ``client``, else on a shared one (see :func:`capabilities`)."""
    runner = client or _shared_client(host_path, cache_directory, concurrency)
    return runner.still(project, time, output, **options)
