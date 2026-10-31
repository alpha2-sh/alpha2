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


class VideoEncoderFallback(TypedDict):
    """One video encoder an export tried before the one that wrote the video."""

    encoder: str
    hardwareAccelerated: bool
    """Whether it was tried on the GPU; a hybrid encoder can be tried on the GPU, then the CPU."""
    kind: Literal["absent", "busy", "unsuitable", "unknown"]
    """What the refusal is evidence of: not on this machine, every session held by another export
    (it may open later), not for this export's size or options, or unknown."""
    reason: str
    """The typed rejection."""
    detail: str
    """The encoder's own words."""


class AdapterCandidate(TypedDict):
    """One GPU adapter a native host considered, with what it does or why not."""

    index: int
    """Its position in the backend's enumeration, which an ``ALPHA2_GPU_ADAPTER`` number names."""
    name: str
    backend: Literal["dx12", "vulkan", "metal"]
    luid: str | None
    """The DXGI adapter LUID as hex (``0x...``); ``None`` off Windows."""
    software: bool
    renders: bool
    rendersReason: str | None
    decodes: bool
    """Whether hardware decode imports into its device."""
    decodesReason: str | None
    gpuEncodes: list[str]
    """The codecs it encodes on the GPU (``h264``, ``hevc``, ``av1``)."""
    gpuEncodeReason: str | None
    """Why it encodes none, when it encodes none."""


class AdapterChoice(TypedDict):
    """Which GPU adapter a native host renders on and why: ``capabilities["adapters"]``, and a
    failure's ``diagnostics["environment"]["adapterChoice"]``."""

    policy: str
    """The ``ALPHA2_GPU_ADAPTER`` policy: ``auto``, ``performance``, ``index:<n>`` or
    ``name:<text>``. Unset, it is ``auto`` on Linux and macOS and ``performance`` on Windows."""
    chosen: int | None
    """The ``index`` of the candidate the renderer is on; ``None`` for a device none of them
    gave."""
    reason: Literal[
        "most-gpu-encoders",
        "decodes",
        "high-performance-order",
        "performance-policy",
        "pinned",
        "only-adapter",
        "no-shared-device",
    ]
    explanation: str
    """One sentence naming the chosen adapter, and the runner-up where the reason compares them."""
    candidates: list[AdapterCandidate]


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
    videoEncoderClass: str | None
    """``hardware`` or ``software``: how ``videoEncoder`` runs."""
    videoEncoderHardware: bool | None
    """Whether ``videoEncoder`` encoded on the GPU or a media engine."""
    videoEncoderPreferred: str | None
    """The encoder the request pinned, else the first the host would have taken."""
    videoEncoderFallbacks: list[VideoEncoderFallback]
    """The encoders tried before ``videoEncoder``, in the order tried."""
    adapter: str
    """The GPU adapter that rendered, or the CPU renderer's name."""
    hardwareDecodeReady: NotRequired[bool]
    """Whether the host could decode video on the GPU when the export ran."""
    hardwareDecodeUnavailable: NotRequired[str | None]
    """Why ``auto`` decode set up no hardware decoder at the host's start, when it could not."""
    audioCodec: str | None
    audioEncoder: str | None
    """The FFmpeg encoder that wrote the audio (``aac``, ``libopus``); ``None`` without audio."""
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
    unmatchedFontFamilies: list[str]
    """Families the project's texts name that no supplied font has (matched by any name a font's
    table gives, in any language, after NFC and lowercasing), sorted; those texts drew in the
    fallback order."""


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
    unmatchedFontFamilies: list[str]
    """As :attr:`ExportResult.unmatchedFontFamilies`."""
