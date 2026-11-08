"""Errors the host client raises, and the export failure each one reports.

Every error carries a ``failure``: a JSON-ready ``alpha2/export-failure`` dict. It is the host's
when the host reported one, otherwise classified from the error code (:func:`failure_for_code`) the
same way the host would.
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

# tests/cases/failure-classes.json holds the same table.
_CODE_CLASSES: dict[str, tuple[str, str]] = {
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
    "GPU_DEVICE_HUNG": ("DEVICE_HUNG", "gpu"),
    "GPU_TIMEOUT": ("GPU_TIMEOUT", "gpu"),
    # The host names the region when it knows the session was a hardware one.
    "DECODE_STALLED": ("GPU_TIMEOUT", "unknown"),
    "ENCODE_STALLED": ("GPU_TIMEOUT", "unknown"),
    "IO_ERROR": ("OUTPUT_IO_FAILED", "unknown"),
    "OUTPUT_PUBLISH_FAILED": ("OUTPUT_IO_FAILED", "unknown"),
    "HOST_RETIRING": ("HOST_EXITED", "unknown"),
    "INVALID_EMIT": _PERMANENT,
    "EXTRACTOR_FAILED": _PERMANENT,
    "EXTRACTOR_TIMEOUT": _UNKNOWN,
    "CANCELLED": ("CANCELLED", "unknown"),
    "RESOURCE_LIMIT": ("RESOURCE_LIMIT", "unknown"),
    "GPU_INIT_FAILED": _UNKNOWN,
    "ENCODE_ERROR": _UNKNOWN,
    "ENCODER_BUSY": ("RESOURCE_BUSY", "gpu"),
    "INTERNAL": _UNKNOWN,
    "ALPHA2_ERROR": _UNKNOWN,
    "HOST_NOT_FOUND": _PERMANENT,
    "HOST_NOT_EXECUTABLE": _PERMANENT,
    "HOST_LOAD_FAILED": _PERMANENT,
    "HOST_START_FAILED": _UNKNOWN,
    # A loaded machine may start the host late, so a retry may work.
    "HOST_START_TIMEOUT": _UNKNOWN,
    "HOST_QUARANTINED": _PERMANENT,
    "HOST_BLOCKED": _PERMANENT,
    "HOST_WRITE_FAILED": _UNKNOWN,
    "HOST_EXITED": ("HOST_EXITED", "unknown"),
    "HOST_OOM_KILLED": ("OUT_OF_MEMORY", "cpu"),
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
    """The failure classified from ``code`` alone, kept as ``hostCode``. ``exitCode`` and
    ``signal`` are recorded when ``has_exit`` is true."""
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

    ``code`` is machine-readable, such as ``HOST_NOT_FOUND`` or ``DECODE_STALLED``; ``failure``
    classifies it for a retry policy. ``stderr`` is the tail of the host's standard error;
    ``exit_code`` and ``signal`` say how a host that ended before answering ended. For
    ``OUTPUT_PUBLISH_FAILED``, ``kept_path`` is where the finished render was kept, for the caller
    to move or remove. For ``RESOURCE_BUSY`` (``ENCODER_BUSY``), :attr:`retry_after_ms` is how long
    to wait before retrying.
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
    """A compose spec that does not compile at ``path`` (``spec.body.items[3]``) for ``problem``."""

    path: str
    problem: str

    def __init__(self, message: str, path: str, problem: str) -> None:
        super().__init__(message)
        self.path = path
        self.problem = problem
