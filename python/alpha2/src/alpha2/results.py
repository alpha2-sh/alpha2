"""What the host answers, typed: the results of an export and of a still request.

Hand-written from the host protocol (``docs/host-protocol.md``); the keys are the protocol's
camelCase ones. Nested reports (``performance``, ``color``, ``media``, ...) stay plain dicts: they
grow with the host, and reading them needs no import. ``@alpha2-sh/node`` declares the same shape as
``NativeExportResult``.
"""

from typing import Any, Literal, NotRequired, TypedDict

VideoPipeline = Literal["readback", "gpu-resident", "packet-copy"]
"""How an export's frames reached the encoder: read back from the GPU, rendered into the encoder's
own GPU images, or copied as the source's packets."""


class EffectiveRange(TypedDict):
    """The range an export covered, widened to its frame grid, in ticks."""

    startTicks: int
    endTicks: int


class PublishReport(TypedDict):
    """How a finished output was moved into place."""

    method: Literal["rename-no-replace", "placeholder-rename", "rename-replace"]
    """``rename-no-replace``: one rename that refuses an existing file. ``placeholder-rename``: the
    file system refused that rename (exFAT on macOS, FUSE mounts such as rclone), so an empty file
    was created at the path, refusing an existing one, and renamed over. ``rename-replace``: the
    request overwrites, and the rename replaced what was there."""
    stagedLocally: bool
    """The output folder cannot rewrite a file at an earlier offset, so the output was written to
    a staging file in the temporary directory and copied into the folder once finished."""
    retries: int
    """Tries repeated while another program held a file without sharing deletion (Windows)."""


class ExportResult(TypedDict):
    """The result of :func:`alpha2.export`."""

    output: str
    """The absolute path of the file written."""
    frames: int
    """Video frames written."""
    videoPipeline: VideoPipeline
    """How the frames reached the encoder; not a file path (that is ``output``)."""
    container: Literal["mp4", "mov", "mkv", "webm"]
    videoCodec: str | None
    """``None`` for a packet copy, which encodes nothing."""
    videoEncoder: str | None
    """The FFmpeg encoder that wrote the video; ``None`` for a packet copy."""
    audioCodec: str | None
    audioEncoder: str | None
    videoDecodeMode: NotRequired[Literal["software", "auto", "hardware-required"]]
    """How video was decoded; absent for a packet copy, which decodes nothing."""
    audio: bool
    """Whether the file has an audio track."""
    renderMs: float
    fps: float
    renderer: Literal["gpu", "cpu"]
    rendererReason: str | None
    rendererThreads: NotRequired[int]
    """The CPU renderer's threads; absent on a GPU host."""
    effectiveRange: EffectiveRange
    inputPolicy: Literal["auto", "gpu-required", "cpu-readback"]
    strategy: Literal["render", "copy"]
    requestedStrategy: Literal["render", "prefer-copy", "require-copy"]
    copyIneligible: str | None
    audioStrategy: NotRequired[Literal["copy", "encode"] | None]
    audioCopyIneligible: NotRequired[str | None]
    copiedSegments: NotRequired[int]
    gpuOutputPathUnavailable: str | None
    gpuOutputPathFallback: dict[str, Any] | None
    publish: PublishReport
    """How the finished file was moved to ``output``."""
    videoTranscode: Literal["yuv-repack"] | None
    yuvTranscodeIneligible: str | None
    videoEncoderLibrary: NotRequired[dict[str, Any] | None]
    videoRateControl: dict[str, Any] | None
    audioRateControl: dict[str, Any] | None
    loudness: dict[str, Any] | None
    color: dict[str, Any]
    renderCache: dict[str, Any] | None
    resultCache: dict[str, Any]
    media: dict[str, Any]
    """What each source read, by source id."""
    memoryPolicy: dict[str, Any]
    performance: dict[str, Any]
    """Timings and counters; the decoder session keys among them are documented in the protocol."""
    providerDiagnostics: list[dict[str, Any]]
    providerDiagnosticsOmitted: int
    decodeDiagnostics: list[Any]


class StillFrame(TypedDict):
    """One PNG a still request wrote."""

    output: str
    """The absolute path of the file written."""
    frameTime: int
    """The time of the frame drawn, in ticks: the frame of the project's grid that holds the time
    asked for."""
    width: int
    height: int
    publish: PublishReport
    """How the PNG was moved to ``output``."""


class StillTimings(TypedDict):
    decodeMs: float
    renderMs: float
    writeMs: float


class StillResult(TypedDict):
    """The result of :func:`alpha2.stills` and :func:`alpha2.still`."""

    stills: list[StillFrame]
    """In the order they were asked for."""
    renderer: Literal["gpu", "cpu"]
    renderMs: float
    timings: StillTimings
    media: dict[str, Any]
    """What each source read, by source id."""
    decodeDiagnostics: list[Any]
