"""The alpha2-host wheel: repackaging a release archive, and finding the host it installs."""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import io
import os
import struct
import sys
import tarfile
import types
import zipfile
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT

import alpha2
from alpha2.client import resolve_host_binary
from alpha2.install import host_executable_name

spec = importlib.util.spec_from_file_location(
    "build_wheel", ROOT / "python" / "alpha2-host" / "build_wheel.py"
)
assert spec is not None and spec.loader is not None
build_wheel: Any = importlib.util.module_from_spec(spec)
# Registered first: its dataclasses look their module up while it loads.
sys.modules["build_wheel"] = build_wheel
spec.loader.exec_module(build_wheel)


def elf(*versions: str) -> bytes:
    """Bytes that read as an ELF file needing these glibc symbol versions."""
    return b"\x7fELF" + b"\x00" * 60 + b"".join(f"GLIBC_{v}\x00".encode() for v in versions)


def macho(major: int, minor: int) -> bytes:
    """A 64-bit Mach-O header with one LC_BUILD_VERSION naming macOS ``major.minor``."""
    command = struct.pack("<IIIIII", 0x32, 24, 1, (major << 16) | (minor << 8), 0, 0)
    header = struct.pack("<IiiIIIII", 0xFEEDFACF, 0x0100000C, 0, 2, 1, len(command), 0, 0)
    return header + command


def archive(tmp_path: Path, name: str, files: dict[str, bytes], links: dict[str, str]) -> Path:
    base = name.removesuffix(".tar.gz")
    path = tmp_path / name
    with tarfile.open(path, "w:gz") as bundle:
        for relative, data in files.items():
            info = tarfile.TarInfo(f"{base}/{relative}")
            info.size = len(data)
            info.mode = 0o755 if relative.startswith(("bin/", "lib/")) else 0o644
            bundle.addfile(info, io.BytesIO(data))
        for relative, target in links.items():
            info = tarfile.TarInfo(f"{base}/{relative}")
            info.type = tarfile.SYMTYPE
            info.linkname = target
            bundle.addfile(info)
    return path


def assert_record_lists_every_entry(bundle: zipfile.ZipFile, record: str) -> None:
    """The wheel's RECORD lists every entry with its hash and size."""
    lines = bundle.read(record).decode().splitlines()
    listed = {line.split(",")[0]: line.split(",")[1:] for line in lines}
    assert set(listed) == set(bundle.namelist())
    for name, (digest, size) in listed.items():
        if name == record:
            continue
        data = bundle.read(name)
        expected = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
        assert digest == f"sha256={expected.decode()}" and int(size) == len(data), name


def test_a_linux_archive_becomes_a_manylinux_wheel_of_its_libraries_sonames(
    tmp_path: Path,
) -> None:
    source = archive(
        tmp_path,
        "alpha2-0.4.2-linux-x64-bundled.tar.gz",
        {
            "bin/alpha2-host": elf("2.17", "2.34"),
            "bin/alpha2": elf("2.28"),
            "lib/libavutil.so.60.26.103": elf("2.35"),
            "LICENSE.md": b"MIT",
            "licenses/FFmpeg/COPYING.LGPLv2.1": b"LGPL",
        },
        {
            "lib/libavutil.so.60": "libavutil.so.60.26.103",
            "lib/libavutil.so": "libavutil.so.60.26.103",
        },
    )
    wheel = Path(build_wheel.build(str(source), str(tmp_path / "out")))
    assert wheel.name == "alpha2_host-0.4.2-py3-none-manylinux_2_35_x86_64.whl"
    with zipfile.ZipFile(wheel) as bundle:
        names = bundle.namelist()
        assert names[-1] == "alpha2_host-0.4.2.dist-info/RECORD"
        # The loader's name only: no full-version file, no development link.
        assert [name for name in names if "/lib/" in name] == ["alpha2_host/lib/libavutil.so.60"]
        assert bundle.read("alpha2_host/lib/libavutil.so.60") == elf("2.35")
        for name in ("alpha2_host/bin/alpha2-host", "alpha2_host/lib/libavutil.so.60"):
            assert (bundle.getinfo(name).external_attr >> 16) & 0o777 == 0o755, name
        assert (bundle.getinfo("alpha2_host/__init__.py").external_attr >> 16) & 0o111 == 0
        assert "alpha2_host/licenses/FFmpeg/COPYING.LGPLv2.1" in names
        assert (
            "Tag: py3-none-manylinux_2_35_x86_64"
            in bundle.read("alpha2_host-0.4.2.dist-info/WHEEL").decode()
        )
        assert_record_lists_every_entry(bundle, "alpha2_host-0.4.2.dist-info/RECORD")
        namespace: dict[str, Any] = {"__file__": "/site/alpha2_host/__init__.py"}
        exec(bundle.read("alpha2_host/__init__.py"), namespace)
        assert namespace["__version__"] == "0.4.2"
        assert namespace["host_path"]().endswith(os.path.join("bin", "alpha2-host"))
    # The same archive makes the same bytes.
    again = Path(build_wheel.build(str(source), str(tmp_path / "again")))
    assert again.read_bytes() == wheel.read_bytes()


def test_a_macos_archive_is_tagged_with_its_minimum_os_and_a_prerelease_in_pythons_spelling(
    tmp_path: Path,
) -> None:
    source = archive(
        tmp_path,
        "alpha2-0.5.0-rc.1-macos-arm64-bundled.tar.gz",
        {"bin/alpha2-host": macho(13, 0), "lib/libavutil.60.26.103.dylib": macho(14, 0)},
        {
            "lib/libavutil.60.dylib": "libavutil.60.26.103.dylib",
            "lib/libavutil.dylib": "libavutil.60.26.103.dylib",
        },
    )
    wheel = Path(build_wheel.build(str(source), str(tmp_path / "out")))
    assert wheel.name == "alpha2_host-0.5.0rc1-py3-none-macosx_14_0_arm64.whl"
    with zipfile.ZipFile(wheel) as bundle:
        assert [name for name in bundle.namelist() if "/lib/" in name] == [
            "alpha2_host/lib/libavutil.60.dylib"
        ]
        namespace: dict[str, Any] = {"__file__": "/site/alpha2_host/__init__.py"}
        exec(bundle.read("alpha2_host/__init__.py"), namespace)
        # The package keeps the archive's spelling, which alpha2 compares with its own.
        assert namespace["__version__"] == "0.5.0-rc.1"
        assert (
            "Version: 0.5.0rc1" in bundle.read("alpha2_host-0.5.0rc1.dist-info/METADATA").decode()
        )


def test_a_windows_archive_becomes_a_win_amd64_wheel(tmp_path: Path) -> None:
    # As Windows' archivers write it: the libraries beside the executable, no symlinks, and no
    # POSIX mode bits for the wheel to keep.
    base = "alpha2-0.4.2-win-x64-bundled"
    files = {
        "bin/alpha2-host.exe": b"MZ host",
        "bin/avutil-60.dll": b"MZ avutil",
        "bin/dxcompiler.dll": b"MZ dxc",
        "LICENSE.md": b"MIT",
    }
    source = tmp_path / f"{base}.zip"
    with zipfile.ZipFile(source, "w") as bundle:
        for relative, data in files.items():
            info = zipfile.ZipInfo(f"{base}/{relative}")
            info.create_system = 0
            info.external_attr = 0
            bundle.writestr(info, data)
    wheel = Path(build_wheel.build(str(source), str(tmp_path / "out")))
    assert wheel.name == "alpha2_host-0.4.2-py3-none-win_amd64.whl"
    with zipfile.ZipFile(wheel) as bundle:
        names = bundle.namelist()
        assert [name for name in names if name.startswith("alpha2_host/bin/")] == [
            "alpha2_host/bin/alpha2-host.exe",
            "alpha2_host/bin/avutil-60.dll",
            "alpha2_host/bin/dxcompiler.dll",
        ]
        for relative, data in files.items():
            assert bundle.read(f"alpha2_host/{relative}") == data, relative
        assert (
            "Tag: py3-none-win_amd64" in bundle.read("alpha2_host-0.4.2.dist-info/WHEEL").decode()
        )
        assert_record_lists_every_entry(bundle, "alpha2_host-0.4.2.dist-info/RECORD")
        namespace: dict[str, Any] = {"__file__": "/site/alpha2_host/__init__.py"}
        exec(bundle.read("alpha2_host/__init__.py"), namespace)
        assert namespace["host_path"]().endswith(os.path.join("bin", "alpha2-host.exe"))


def test_an_archive_that_is_not_a_bundled_release_is_refused(tmp_path: Path) -> None:
    other = archive(tmp_path, "alpha2-0.4.2-linux-x64-debug.tar.gz", {"bin/alpha2-host": elf()}, {})
    with pytest.raises(SystemExit, match="bundled"):
        build_wheel.build(str(other), str(tmp_path / "out"))
    empty = archive(tmp_path, "alpha2-0.4.2-linux-x64-bundled.tar.gz", {"bin/alpha2": elf()}, {})
    with pytest.raises(SystemExit, match="bin/alpha2-host"):
        build_wheel.build(str(empty), str(tmp_path / "out"))


def test_the_host_the_wheel_installed_is_found_after_alpha2_host_path_and_before_the_cache(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    installed = tmp_path / "site" / "alpha2_host" / "bin" / "alpha2-host"
    installed.parent.mkdir(parents=True)
    installed.write_text("")
    module = types.ModuleType("alpha2_host")
    module.__version__ = alpha2.__version__  # type: ignore[attr-defined]
    module.host_path = lambda: str(installed)  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "alpha2_host", module)
    monkeypatch.delenv("ALPHA2_HOST_PATH", raising=False)
    monkeypatch.setenv("PATH", "")
    target = alpha2.host_target()
    cache = tmp_path / "cache"
    if target is not None:
        cached = (
            cache / f"alpha2-{alpha2.__version__}-{target}-bundled" / "bin" / host_executable_name()
        )
        cached.parent.mkdir(parents=True)
        cached.write_text("")
    assert resolve_host_binary(cache_directory=cache) == installed
    explicit = tmp_path / "explicit"
    explicit.write_text("")
    monkeypatch.setenv("ALPHA2_HOST_PATH", str(explicit))
    assert resolve_host_binary(cache_directory=cache) == explicit
    monkeypatch.delenv("ALPHA2_HOST_PATH")
    # A wheel of another version speaks another protocol: it is passed over.
    module.__version__ = "0.0.1"  # type: ignore[attr-defined]
    if target is not None:
        assert resolve_host_binary(cache_directory=cache) == cached
    else:
        with pytest.raises(alpha2.Alpha2HostError):
            resolve_host_binary(cache_directory=cache)
