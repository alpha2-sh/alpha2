"""alpha2 for Python: build, validate and render alpha2 project documents.

A project is a plain ``dict`` in the shape of :class:`alpha2.types.Project` (the engine's
``project.v1`` document), built by hand or with :class:`ProjectBuilder`. Validation runs the
engine's own validator in-process; rendering runs the native ``alpha2-host`` process, which
``python -m alpha2 install`` downloads::

    import alpha2

    project = alpha2.ProjectBuilder("Intro", width=1280, height=720, frame_rate=30)
    image = project.source("image", "title.png")
    project.video_track().clip(image, at=0, duration=3)
    alpha2.export(project.build(), "intro.mp4", assets=project.assets())
"""

from . import compose, layout, shapes
from . import effect_values as fx
from ._native import __version__
from .builder import (
    TICKS_PER_SECOND,
    CaptionTrackBuilder,
    ProjectBuilder,
    Ticks,
    TrackBuilder,
    effect,
    lint_project,
    seconds_to_ticks,
    sequential_ids,
    ticks,
    ticks_to_seconds,
    to_ticks,
    validate_project,
    visual,
)
from .client import (
    QUALITY_LEVELS,
    ExportProgress,
    HostClient,
    capabilities,
    conform,
    export,
    resolve_host_binary,
    still,
    stills,
)
from .errors import (
    Alpha2AbortError,
    Alpha2ComposeError,
    Alpha2ConformError,
    Alpha2GpuInitializationError,
    Alpha2HostError,
    Alpha2HostInstallError,
    Alpha2ProjectValidationError,
)
from .install import (
    HOST_DOWNLOAD_URL,
    OPENH264_DOWNLOAD_URL,
    InstallResult,
    OpenH264InstallResult,
    host_cache_directory,
    host_target,
    install_host,
    install_openh264,
)
from .measure import LayoutBounds, NodeBounds, TextBounds, TextMeasurer
from .protocol import PROTOCOL_VERSION
from .results import ExportResult, StillResult
from .values import constant, key, keyframes, lift, point_key, point_keyframes

__all__ = [
    "HOST_DOWNLOAD_URL",
    "OPENH264_DOWNLOAD_URL",
    "PROTOCOL_VERSION",
    "QUALITY_LEVELS",
    "TICKS_PER_SECOND",
    "Alpha2AbortError",
    "Alpha2ComposeError",
    "Alpha2ConformError",
    "Alpha2GpuInitializationError",
    "Alpha2HostError",
    "Alpha2HostInstallError",
    "Alpha2ProjectValidationError",
    "CaptionTrackBuilder",
    "ExportProgress",
    "ExportResult",
    "HostClient",
    "InstallResult",
    "LayoutBounds",
    "NodeBounds",
    "OpenH264InstallResult",
    "ProjectBuilder",
    "StillResult",
    "TextBounds",
    "TextMeasurer",
    "Ticks",
    "TrackBuilder",
    "__version__",
    "capabilities",
    "compose",
    "conform",
    "constant",
    "effect",
    "export",
    "fx",
    "host_cache_directory",
    "host_target",
    "install_host",
    "install_openh264",
    "key",
    "keyframes",
    "layout",
    "lift",
    "lint_project",
    "point_key",
    "point_keyframes",
    "resolve_host_binary",
    "seconds_to_ticks",
    "sequential_ids",
    "shapes",
    "still",
    "stills",
    "ticks",
    "ticks_to_seconds",
    "to_ticks",
    "validate_project",
    "visual",
]
