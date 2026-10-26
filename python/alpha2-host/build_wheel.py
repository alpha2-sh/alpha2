"""Repackages a bundled native release archive as the ``alpha2-host`` platform wheel.

    python python/alpha2-host/build_wheel.py <alpha2-VERSION-TARGET-bundled.tar.gz|.zip> <out-dir>

The archive the release job builds is put into the wheel as it is, under the ``alpha2_host``
package: ``bin/`` (``alpha2-host`` and the media CLI), ``lib/`` (FFmpeg's shared libraries, kept
shared so they stay replaceable under the LGPL), ``licenses/`` and ``compliance/``. Nothing is
rebuilt or relinked, so auditwheel is not used; the platform tag is read from the binaries instead:
the newest ``GLIBC_`` symbol version they need on Linux (``manylinux_2_<n>``), their Mach-O minimum
OS on macOS. A wheel holds no symlinks, so each library is stored under the name its symlinks give
it for the loader (``libavutil.so.60``, ``libavutil.60.dylib``), and the unversioned development
links are left out.

The wheel is reproducible: entries are sorted and dated ``SOURCE_DATE_EPOCH`` (default 1980-01-01).
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import os
import re
import stat
import struct
import sys
import tarfile
import time
import zipfile
from dataclasses import dataclass
from pathlib import PurePosixPath

ARCHIVE_NAME = re.compile(
    r"^alpha2-(?P<version>\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)-(?P<target>[a-z]+-[a-z0-9]+)-bundled\.(?:tar\.gz|zip)$"
)

# The release targets, and the wheel platform each is tagged for.
TARGETS = {
    "linux-x64": ("linux", "x86_64"),
    "linux-arm64": ("linux", "aarch64"),
    "macos-arm64": ("macos", "arm64"),
    "win-x64": ("windows", "amd64"),
}

PACKAGE = "alpha2_host"


@dataclass
class Member:
    """One file of the archive, by its path inside the archive's top directory."""

    path: str
    data: bytes
    executable: bool
    link: str | None = None
    """The path inside the archive a symlink resolves to."""


def read_archive(path: str) -> tuple[str, dict[str, Member]]:
    """The archive's top directory and its files, symlinks resolved to the files they name."""
    members: dict[str, Member] = {}
    if path.endswith(".zip"):
        with zipfile.ZipFile(path) as bundle:
            for info in bundle.infolist():
                if info.is_dir():
                    continue
                mode = info.external_attr >> 16
                members[info.filename] = Member(
                    info.filename,
                    bundle.read(info),
                    bool(mode & 0o111) or info.filename.endswith((".exe", ".dll")),
                )
    else:
        links: dict[str, str] = {}
        with tarfile.open(path) as bundle:
            for info in bundle.getmembers():
                if info.issym():
                    links[info.name] = str(PurePosixPath(info.name).parent / info.linkname)
                elif info.isfile():
                    extracted = bundle.extractfile(info)
                    assert extracted is not None
                    members[info.name] = Member(
                        info.name, extracted.read(), bool(info.mode & 0o111)
                    )
                elif not info.isdir():
                    raise SystemExit(
                        f"{path}: {info.name} is neither a file, a directory nor a symlink"
                    )
        for name, target in links.items():
            seen = {name}
            while target in links:
                if target in seen:
                    raise SystemExit(f"{path}: symlink loop at {name}")
                seen.add(target)
                target = links[target]
            if target not in members:
                raise SystemExit(
                    f"{path}: {name} links to {target}, which the archive does not hold"
                )
            members[name] = Member(name, b"", False, link=target)
    tops = {name.split("/", 1)[0] for name in members}
    if len(tops) != 1:
        raise SystemExit(f"{path}: expected one top directory, found {sorted(tops)}")
    return tops.pop(), members


def development_link(name: str) -> bool:
    """``libavutil.so`` or ``libavutil.dylib``: a name only a linker building against it reads."""
    base = PurePosixPath(name).name
    return re.fullmatch(r"lib[^.]+\.(so|dylib)", base) is not None


def wheel_files(top: str, members: dict[str, Member]) -> dict[str, Member]:
    """The archive's files as the wheel stores them, by their path under the package."""
    loader_names: dict[str, list[str]] = {}
    for member in members.values():
        if member.link is not None and not development_link(member.path):
            loader_names.setdefault(member.link, []).append(member.path)
    files: dict[str, Member] = {}
    for member in members.values():
        if member.link is not None:
            continue
        # A library is stored once per name its symlinks give the loader, else under its own.
        for name in sorted(loader_names.get(member.path, [member.path])):
            relative = name[len(top) + 1 :]
            files[relative] = Member(relative, member.data, member.executable)
    return files


def glibc_baseline(files: dict[str, Member]) -> tuple[int, int]:
    """The newest glibc symbol version the ELF files need: what a ``manylinux`` tag promises."""
    newest = (2, 17)
    for member in files.values():
        if not member.data.startswith(b"\x7fELF"):
            continue
        for match in re.finditer(rb"GLIBC_2\.(\d+)(?:\.(\d+))?\x00", member.data):
            newest = max(newest, (2, int(match.group(1))))
    return newest


LC_VERSION_MIN_MACOSX = 0x24
LC_BUILD_VERSION = 0x32


def macos_minimum(files: dict[str, Member]) -> tuple[int, int]:
    """The newest minimum macOS the Mach-O files name: what a ``macosx`` tag promises."""
    newest = (11, 0)
    for member in files.values():
        data = member.data
        if data[:4] != struct.pack("<I", 0xFEEDFACF):
            continue
        commands = struct.unpack_from("<I", data, 16)[0]
        offset = 32
        for _ in range(commands):
            command, size = struct.unpack_from("<II", data, offset)
            if command == LC_BUILD_VERSION:
                version = struct.unpack_from("<I", data, offset + 12)[0]
            elif command == LC_VERSION_MIN_MACOSX:
                version = struct.unpack_from("<I", data, offset + 8)[0]
            else:
                offset += size
                continue
            newest = max(newest, (version >> 16, (version >> 8) & 0xFF))
            offset += size
    return newest


def platform_tag(system: str, machine: str, files: dict[str, Member]) -> str:
    if system == "linux":
        major, minor = glibc_baseline(files)
        return f"manylinux_{major}_{minor}_{machine}"
    if system == "macos":
        major, minor = macos_minimum(files)
        # From macOS 11 on, pip reads only the major version.
        return f"macosx_{major}_{0 if major >= 11 else minor}_{machine}"
    return f"win_{machine}"


def init_source(version: str, executable: str) -> str:
    return f'''"""The alpha2-host renderer, for the alpha2 Python package.

This wheel carries the native release archive of alpha2 {version}: ``bin/`` (``{executable}``),
the FFmpeg shared libraries it loads in ``lib/``, and their licenses in ``licenses/``. The alpha2
package finds the host with :func:`host_path`.
"""

from pathlib import Path

__version__ = "{version}"


def host_path() -> str:
    """The ``{executable}`` this wheel installed."""
    return str(Path(__file__).resolve().parent / "bin" / "{executable}")
'''


def metadata(version: str, licenses: list[str]) -> str:
    lines = [
        "Metadata-Version: 2.4",
        "Name: alpha2-host",
        f"Version: {version}",
        "Summary: The native alpha2 renderer (alpha2-host) for the alpha2 Python package.",
        "Home-page: https://github.com/alpha2-sh/alpha2",
        "Project-URL: Repository, https://github.com/alpha2-sh/alpha2",
        "Requires-Python: >=3.11",
        "Classifier: Programming Language :: Rust",
        "Classifier: Topic :: Multimedia :: Video",
        *(f"License-File: {name}" for name in licenses),
        "Description-Content-Type: text/markdown",
        "",
        "The `alpha2-host` process the [alpha2](https://pypi.org/project/alpha2/) package renders "
        "with: the native release archive repackaged as a wheel, its FFmpeg libraries shared and "
        "replaceable, with their licenses in `alpha2_host/licenses/`. Install `alpha2`, which "
        "depends on it where an archive is published.",
        "",
    ]
    return "\n".join(lines)


def python_version(version: str) -> str:
    """A release version as Python spells it (PEP 440): ``0.5.0-rc.1`` is ``0.5.0rc1``."""
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-(alpha|beta|rc)\.(\d+))?", version)
    if match is None:
        raise SystemExit(f"version {version} has no Python spelling")
    release, kind, number = match.groups()
    return (
        release if kind is None else f"{release}{dict(alpha='a', beta='b', rc='rc')[kind]}{number}"
    )


def record_hash(data: bytes) -> str:
    digest = hashlib.sha256(data).digest()
    return "sha256=" + base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def build(archive: str, out_dir: str) -> str:
    match = ARCHIVE_NAME.match(os.path.basename(archive))
    if match is None:
        raise SystemExit(
            f"{archive}: not a bundled release archive (alpha2-VERSION-TARGET-bundled)"
        )
    version, target = match["version"], match["target"]
    if target not in TARGETS:
        raise SystemExit(f"{archive}: no wheel platform for target {target}")
    system, machine = TARGETS[target]
    top, members = read_archive(archive)
    files = wheel_files(top, members)
    executable = "alpha2-host.exe" if system == "windows" else "alpha2-host"
    if f"bin/{executable}" not in files:
        raise SystemExit(f"{archive}: no bin/{executable}")
    tag = platform_tag(system, machine, files)
    # The wheel is named in Python's spelling; the package keeps the archive's, which the alpha2
    # package compares with its own.
    wheel_version = python_version(version)
    dist_info = f"alpha2_host-{wheel_version}.dist-info"
    entries: dict[str, tuple[bytes, bool]] = {
        f"{PACKAGE}/{path}": (member.data, member.executable or path.startswith("bin/"))
        for path, member in files.items()
    }
    entries[f"{PACKAGE}/__init__.py"] = (init_source(version, executable).encode(), False)
    entries[f"{PACKAGE}/py.typed"] = (b"", False)
    licenses = sorted(path for path in files if path == "LICENSE.md")
    for path in licenses:
        entries[f"{dist_info}/licenses/{path}"] = (files[path].data, False)
    entries[f"{dist_info}/METADATA"] = (metadata(wheel_version, licenses).encode(), False)
    entries[f"{dist_info}/WHEEL"] = (
        (
            "Wheel-Version: 1.0\n"
            "Generator: alpha2 python/alpha2-host/build_wheel.py\n"
            "Root-Is-Purelib: false\n"
            f"Tag: py3-none-{tag}\n"
        ).encode(),
        False,
    )
    record = f"{dist_info}/RECORD"
    lines = [
        f"{name},{record_hash(data)},{len(data)}" for name, (data, _) in sorted(entries.items())
    ]
    entries[record] = ("\n".join([*lines, f"{record},,", ""]).encode(), False)

    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "315532800"))
    date = time.gmtime(max(epoch, 315532800))[:6]
    os.makedirs(out_dir, exist_ok=True)
    wheel = os.path.join(out_dir, f"alpha2_host-{wheel_version}-py3-none-{tag}.whl")
    buffer = io.BytesIO()
    # RECORD goes last, as the wheel specification asks; everything else sorted.
    names = [*sorted(name for name in entries if name != record), record]
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as bundle:
        for name in names:
            data, executable_bit = entries[name]
            info = zipfile.ZipInfo(name, date)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | (0o755 if executable_bit else 0o644)) << 16
            bundle.writestr(info, data)
    with open(wheel, "wb") as file:
        file.write(buffer.getvalue())
    return wheel


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("archive", help="alpha2-VERSION-TARGET-bundled.tar.gz (or .zip)")
    parser.add_argument("out_dir", help="where the wheel is written")
    arguments = parser.parse_args(argv)
    print(build(arguments.archive, arguments.out_dir))
    return 0


if __name__ == "__main__":
    sys.exit(main())
