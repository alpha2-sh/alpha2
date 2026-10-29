"""``python -m alpha2``: install the host, and render or inspect from the command line."""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Sequence
from pathlib import Path

from .builder import TICKS_PER_SECOND
from .client import ExportProgress, HostClient
from .errors import Alpha2HostError, Alpha2HostInstallError
from .install import (
    OPENH264_LICENSE_URL,
    OpenH264InstallResult,
    install_host,
    install_openh264,
)


def _install(arguments: argparse.Namespace) -> int:
    # Progress goes to standard error, in whole percents, and only to a terminal.
    what = "OpenH264" if arguments.openh264 else "alpha2-host"
    shown = -1

    def progress(received: int, total: int | None) -> None:
        nonlocal shown
        if not sys.stderr.isatty() or total is None:
            return
        percent = received * 100 // total
        if percent != shown:
            shown = percent
            sys.stderr.write(f"\rdownloading {what} {percent}%")

    try:
        result = (
            install_openh264(force=arguments.force, on_progress=progress)
            if arguments.openh264
            else install_host(
                kind="thin" if arguments.thin else "bundled",
                force=arguments.force,
                on_progress=progress,
            )
        )
    except Alpha2HostInstallError as error:
        if shown >= 0:
            sys.stderr.write("\n")
        print(f"alpha2 install: {error}", file=sys.stderr)
        return 1
    if shown >= 0:
        sys.stderr.write("\n")
    if isinstance(result, OpenH264InstallResult):
        print(
            f"installed OpenH264 {result.version}: {result.library_path}"
            if result.downloaded
            else f"already installed: {result.library_path}"
        )
        # Cisco's licence asks for its notice wherever the user controls the codec.
        print(result.notice)
        print(f"It is Cisco's binary, under Cisco's terms: {OPENH264_LICENSE_URL}")
        print(
            "alpha2-host uses it for H.264 when no hardware encoder is available; "
            "set ALPHA2_OPENH264=0 to turn it off."
        )
        return 0
    print(
        f"installed {result.host_path}"
        if result.downloaded
        else f"already installed: {result.host_path}"
    )
    return 0


def _pairs(values: Sequence[str], flag: str) -> dict[str, str]:
    pairs: dict[str, str] = {}
    for value in values:
        key, separator, path = value.partition("=")
        if not separator or not key or not path:
            raise SystemExit(f"alpha2: {flag} takes <id>=<path>, not {value}")
        pairs[key] = path
    return pairs


def _export(arguments: argparse.Namespace) -> int:
    project = json.loads(Path(arguments.project).read_text(encoding="utf-8"))
    shown = -1

    def progress(report: ExportProgress) -> None:
        nonlocal shown
        if not sys.stderr.isatty() or report.total == 0:
            return
        percent = report.completed * 100 // report.total
        if percent != shown:
            shown = percent
            sys.stderr.write(f"\rexporting {percent}%")

    try:
        client = HostClient(host_path=arguments.host)
        result = client.export(
            project,
            arguments.output,
            assets=_pairs(arguments.asset, "--asset"),
            fonts=arguments.font,
            end_ticks=arguments.end_ticks,
            start_ticks=arguments.start_ticks,
            include_audio=not arguments.no_audio,
            video_codec=arguments.video_codec,
            video_quality=arguments.video_quality,
            on_progress=progress,
        )
    except Alpha2HostError as error:
        if shown >= 0:
            sys.stderr.write("\n")
        print(f"alpha2 export: {error.code}: {error}", file=sys.stderr)
        return 1
    if shown >= 0:
        sys.stderr.write("\n")
    if arguments.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"wrote {arguments.output} ({result['frames']} frames, {result['renderMs']} ms)")
    return 0


def _still(arguments: argparse.Namespace) -> int:
    project = json.loads(Path(arguments.project).read_text(encoding="utf-8"))
    try:
        result = HostClient(host_path=arguments.host).still(
            project,
            arguments.at,
            arguments.output,
            assets=_pairs(arguments.asset, "--asset"),
            fonts=arguments.font,
            size=tuple(arguments.size) if arguments.size else None,
            overwrite=arguments.overwrite,
        )
    except Alpha2HostError as error:
        print(f"alpha2 still: {error.code}: {error}", file=sys.stderr)
        return 1
    if arguments.json:
        print(json.dumps(result, indent=2))
    else:
        frame = result["stills"][0]
        print(
            f"wrote {frame['output']} ({frame['width']}x{frame['height']}, "
            f"frame at {frame['frameTime'] / TICKS_PER_SECOND:g} s)"
        )
    return 0


def _capabilities(arguments: argparse.Namespace) -> int:
    try:
        report = HostClient(host_path=arguments.host).capabilities()
    except Alpha2HostError as error:
        print(f"alpha2 capabilities: {error.code}: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2))
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="alpha2", description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    install = commands.add_parser(
        "install",
        help="download alpha2-host for this version and platform into the shared cache",
        description="Downloads alpha2-host for this package's version and platform into the "
        "cache (ALPHA2_HOST_CACHE_DIR, else the platform's cache directory), shared with "
        "@alpha2-sh/node.",
    )
    what = install.add_mutually_exclusive_group()
    what.add_argument(
        "--thin", action="store_true", help="the archive that loads the system's FFmpeg 8"
    )
    what.add_argument(
        "--openh264",
        action="store_true",
        help="download Cisco's OpenH264 library instead, the software H.264 encoder "
        "alpha2-host loads when no hardware encoder is available",
    )
    install.add_argument(
        "--force", action="store_true", help="download and unpack again when already installed"
    )
    install.set_defaults(run=_install)

    export = commands.add_parser("export", help="render a project JSON file")
    export.add_argument("project", help="the project.v1 JSON file")
    export.add_argument("output", help="the file to write (.mp4, .m4v, .mov, .mkv or .webm)")
    export.add_argument("--asset", action="append", default=[], metavar="ID=PATH")
    export.add_argument("--font", action="append", default=[], metavar="PATH")
    export.add_argument("--start-ticks", type=int, default=0)
    export.add_argument("--end-ticks", type=int, help="default: where the project ends")
    export.add_argument("--no-audio", action="store_true")
    export.add_argument("--video-codec")
    export.add_argument(
        "--video-quality", choices=["very-low", "low", "medium", "high", "very-high"]
    )
    export.add_argument("--json", action="store_true", help="print the host's export result")
    export.add_argument("--host", help="the alpha2-host binary to run")
    export.set_defaults(run=_export)

    still = commands.add_parser(
        "still",
        help="render one frame of a project JSON file to a PNG",
        description="Renders the frame of the project's frame grid that holds --at, as an export "
        "renders it, to an 8-bit RGBA PNG.",
    )
    still.add_argument("project", help="the project.v1 JSON file")
    still.add_argument("output", help="the PNG to write")
    still.add_argument(
        "--at", type=float, required=True, metavar="SECONDS", help="the timeline time"
    )
    still.add_argument("--asset", action="append", default=[], metavar="ID=PATH")
    still.add_argument("--font", action="append", default=[], metavar="PATH")
    still.add_argument(
        "--size", type=int, nargs=2, metavar=("WIDTH", "HEIGHT"), help="default: the canvas"
    )
    still.add_argument(
        "--overwrite", action="store_true", help="replace a file already at the output"
    )
    still.add_argument("--json", action="store_true", help="print the host's result")
    still.add_argument("--host", help="the alpha2-host binary to run")
    still.set_defaults(run=_still)

    report = commands.add_parser("capabilities", help="print what the host can decode and encode")
    report.add_argument("--host", help="the alpha2-host binary to run")
    report.set_defaults(run=_capabilities)

    arguments = parser.parse_args(argv)
    return int(arguments.run(arguments))


if __name__ == "__main__":
    sys.exit(main())
