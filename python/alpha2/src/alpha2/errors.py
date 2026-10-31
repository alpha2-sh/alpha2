"""Errors the host client raises, and the export failure each one reports.

Every error carries a ``failure``: the classified export failure of ``alpha2/export-failure``, a
JSON-ready dict that crosses process boundaries unchanged. When the host reported one it is the
host's; otherwise it is the class the error's code is evidence of on its own
(:func:`failure_for_code`), as the host would have classified the same code, so a refusal made here
is as final as the host's.
"""

from __future__ import annotations

from typing import Any, Literal

__all__ = [
    "Alpha2AbortError",
    "Alpha2ComposeError",
    "Alpha2ConformError",
    "Alpha2GpuInitializationError",
    "Alpha2HostError",
    "Alpha2HostInstallError",
    "Alpha2ProjectValidationError",
    "ExportPhase",
    "failure_class_of_code",
    "failure_for_code",
]

ExportPhase = Literal[
    "prepare",
    "decode",
    "upload",
    "render",
    "readback",
    "encode",
    "mux",
    "publish",
    "download",
    "cleanup",
]

_PERMANENT = ("INVALID_INPUT", "unknown")
_UNKNOWN = ("UNKNOWN", "unknown")

# The class each error code is evidence of on its own: the host protocol's codes and the ones only a
# host client makes. tests/cases/failure-classes.json holds the same table; the tests read it.
_CODE_CLASSES: dict[str, tuple[str, str]] = {
    # The request would meet the same refusal again.
    "UNSUPPORTED_PROTOCOL": _PERMANENT,
    "INVALID_REQUEST": _PERMANENT,
    "INVALID_INPUT": _PERMANENT,
    "CONFORM_FAILED": _PERMANENT,
    "MISSING_INPUT": _PERMANENT,
    "INCOMPLETE_COMPOSITION": _PERMANENT,
    "EFFECT_ERROR": _PERMANENT,
    "HARDWARE_DECODE_UNAVAILABLE": _PERMANENT,
    "COPY_INELIGIBLE": _PERMANENT,
    "GPU_OUTPUT_UNAVAILABLE": _PERMANENT,
    "OUTPUT_IN_USE": _PERMANENT,
    "GPU_DEVICE_LOST": ("DEVICE_LOST", "gpu"),
    # The export's own submission ran long enough to stop the GPU.
    "GPU_DEVICE_HUNG": ("DEVICE_HUNG", "gpu"),
    "GPU_TIMEOUT": ("GPU_TIMEOUT", "gpu"),
    # A stuck decoder or encoder; the host names the region when it knows the session was a
    # hardware one.
    "DECODE_STALLED": ("GPU_TIMEOUT", "unknown"),
    "ENCODE_STALLED": ("GPU_TIMEOUT", "unknown"),
    "IO_ERROR": ("OUTPUT_IO_FAILED", "unknown"),
    # The output rendered in full and is kept beside its path; moving it into place failed.
    "OUTPUT_PUBLISH_FAILED": ("OUTPUT_IO_FAILED", "unknown"),
    "CANCELLED": ("CANCELLED", "unknown"),
    "RESOURCE_LIMIT": ("RESOURCE_LIMIT", "unknown"),
    "GPU_INIT_FAILED": _UNKNOWN,
    "ENCODE_ERROR": _UNKNOWN,
    # Every session of the hardware encoder is held by other exports; it frees up when they end.
    "ENCODER_BUSY": ("RESOURCE_BUSY", "gpu"),
    "INTERNAL": _UNKNOWN,
    "ALPHA2_ERROR": _UNKNOWN,
    # The client's own: a host it cannot run is setup that a retry does not change.
    "HOST_NOT_FOUND": _PERMANENT,
    "HOST_NOT_EXECUTABLE": _PERMANENT,
    # The loader refused the host (a missing or mismatched FFmpeg): the machine's setup again.
    "HOST_LOAD_FAILED": _PERMANENT,
    "HOST_START_FAILED": _UNKNOWN,
    # No first line in time: a loaded machine may start the host late, so a retry may work.
    "HOST_START_TIMEOUT": _UNKNOWN,
    # macOS would hold the host on an approval prompt, and Windows refused it by policy: setup.
    "HOST_QUARANTINED": _PERMANENT,
    "HOST_BLOCKED": _PERMANENT,
    "HOST_WRITE_FAILED": _UNKNOWN,
    "HOST_EXITED": ("HOST_EXITED", "unknown"),
    "INVALID_RESPONSE": _UNKNOWN,
    "PROGRESS_CALLBACK_FAILED": _UNKNOWN,
    "ABORTED": ("CANCELLED", "unknown"),
}


def failure_class_of_code(code: object) -> tuple[str, str]:
    """The failure class and memory resource ``code`` is evidence of; ``UNKNOWN`` for any other."""
    if isinstance(code, str):
        return _CODE_CLASSES.get(code, _UNKNOWN)
    return _UNKNOWN


def failure_for_code(
    code: str | None,
    *,
    message: str,
    phase: ExportPhase = "prepare",
    origin: str = "python",
    backend: str = "unknown",
    stderr: str | None = None,
    exit_code: int | None = None,
    signal: str | None = None,
    has_exit: bool = False,
) -> dict[str, Any]:
    """The failure an error with host or client code ``code`` and no failure of its own reports.

    It is classified by the code, which it keeps as ``hostCode``. ``exitCode`` and ``signal`` are
    recorded when ``has_exit`` says the process's end is known (either may be ``None``).
    """
    failure_code, resource = failure_class_of_code(code)
    failure: dict[str, Any] = {
        "code": failure_code,
        "message": message,
        "origin": origin,
        "operation": "export",
        "backend": backend,
        "phase": phase,
        "resource": resource,
    }
    if code is not None:
        failure["hostCode"] = code
    if stderr is not None:
        failure["stderr"] = stderr
    if has_exit:
        failure["exitCode"] = exit_code
        failure["signal"] = signal
    return failure


class Alpha2HostError(Exception):
    """Every error the host client raises.

    ``code`` is machine-readable, such as ``HOST_NOT_FOUND``, ``INVALID_INPUT`` or
    ``DECODE_STALLED``; ``failure`` classifies it for a retry policy. ``stderr`` is the last part of
    the host's standard error, and ``exit_code`` and ``signal`` say how the host ended when it ended
    before answering. For ``OUTPUT_PUBLISH_FAILED``, ``kept_path`` is the absolute path the
    finished render was kept at, beside the output, when the host could keep it; the file is the
    caller's to move or remove. For a ``RESOURCE_BUSY`` failure (``ENCODER_BUSY``),
    :attr:`retry_after_ms` is how long to wait before trying the same export again.
    """

    code: str
    failure: dict[str, Any]
    stderr: str | None
    exit_code: int | None
    signal: str | None
    kept_path: str | None

    def __init__(
        self,
        code: str,
        message: str,
        *,
        failure: dict[str, Any] | None = None,
        phase: ExportPhase = "prepare",
        stderr: str | None = None,
        exit_code: int | None = None,
        signal: str | None = None,
        has_exit: bool = False,
        kept_path: str | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.stderr = stderr
        self.exit_code = exit_code
        self.signal = signal
        self.kept_path = kept_path
        self.failure = (
            failure
            if failure is not None
            else failure_for_code(
                code,
                message=message,
                phase=phase,
                backend="native",
                stderr=stderr,
                exit_code=exit_code,
                signal=signal,
                has_exit=has_exit,
            )
        )

    @property
    def retry_after_ms(self) -> int | None:
        """How long to wait before retrying a busy device, from the failure; ``None`` otherwise."""
        value = self.failure.get("retryAfterMs")
        return value if isinstance(value, int) and not isinstance(value, bool) else None


class Alpha2ConformError(Alpha2HostError):
    """An export refused before it rendered: the project's media is not ready.

    ``report`` says what each source would read and what blocks it.
    """

    report: dict[str, Any]

    def __init__(self, message: str, report: dict[str, Any], **options: Any) -> None:
        super().__init__("CONFORM_FAILED", message, **options)
        self.report = report


class Alpha2GpuInitializationError(Alpha2HostError):
    """The host could not create its GPU device (``GPU_INIT_FAILED``)."""

    def __init__(self, message: str, **options: Any) -> None:
        super().__init__("GPU_INIT_FAILED", message, **options)


class Alpha2ProjectValidationError(Alpha2HostError):
    """The project failed validation before a host started (``INVALID_INPUT``).

    ``problems`` lists why.
    """

    problems: list[str]

    def __init__(self, problems: list[str]) -> None:
        super().__init__("INVALID_INPUT", "invalid alpha2 project:\n" + "\n".join(problems))
        self.problems = list(problems)


class Alpha2AbortError(Alpha2HostError):
    """The request was cancelled (``ABORTED``); its host has ended and removed its partial file."""

    def __init__(self) -> None:
        super().__init__("ABORTED", "alpha2 export was aborted")


class Alpha2HostInstallError(Exception):
    """An :func:`alpha2.install_host` failure. Nothing it was installing is left in the cache.

    ``code`` is ``UNSUPPORTED_PLATFORM``, ``DOWNLOAD_FAILED``, ``CHECKSUM_MISMATCH``,
    ``EXTRACT_FAILED`` or ``ABORTED``.
    """

    code: str

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class Alpha2ComposeError(ValueError):
    """A compose spec that does not compile: ``path`` is where in it (``spec.body.items[3]``),
    ``problem`` what is wrong there, and the message both."""

    path: str
    problem: str

    def __init__(self, message: str, path: str, problem: str) -> None:
        super().__init__(message)
        self.path = path
        self.problem = problem
