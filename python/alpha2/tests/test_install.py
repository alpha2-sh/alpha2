"""Installing the host into the shared cache, from a release served on localhost."""

from __future__ import annotations

import bz2
import dataclasses
import hashlib
import http.server
import io
import json
import os
import select
import socket
import ssl
import sys
import tarfile
import threading
import tomllib
import warnings
import zipfile
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from conftest import ROOT

import alpha2
import alpha2.install as install_module
from alpha2 import (
    Alpha2HostInstallError,
    host_cache_directory,
    host_target,
    install_host,
    install_openh264,
)
from alpha2.client import resolve_host_binary
from alpha2.install import LINUX_GLIBC_FLOOR, host_executable_name

supported = pytest.mark.skipif(host_target() is None, reason="no archive is published here")


@dataclass
class Release:
    base: str
    download_url: str
    archive: str
    archive_sha256: str
    requests: list[str] = field(default_factory=list)


@pytest.fixture
def serve_release(tmp_path: Path) -> Iterator[object]:
    """Serves a release of this package's version for this platform, as ``install_host`` reads it:
    the archive and ``checksums.sha256`` under ``/v<version>/``."""
    servers: list[http.server.ThreadingHTTPServer] = []

    def serve(checksum: str | None = None, tls: bool = False) -> Release:
        root = tmp_path / f"release-{len(servers)}"
        base = f"alpha2-{alpha2.__version__}-{host_target()}-bundled"
        (root / base / "bin").mkdir(parents=True)
        (root / base / "lib").mkdir()
        (root / base / "bin" / host_executable_name()).write_text("host")
        (root / base / "lib" / "libavutil.so.60.1.100").write_text("lib")
        archive = root / f"{base}.{'zip' if sys.platform == 'win32' else 'tar.gz'}"
        if sys.platform == "win32":
            with zipfile.ZipFile(archive, "w") as bundle:
                for path in (root / base).rglob("*"):
                    bundle.write(path, path.relative_to(root).as_posix())
        else:
            os.symlink("libavutil.so.60.1.100", root / base / "lib" / "libavutil.so.60")
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(root / base, arcname=base)
        data = archive.read_bytes()
        archive_sha256 = hashlib.sha256(data).hexdigest()
        digest = checksum or archive_sha256
        files = {
            f"/v{alpha2.__version__}/{archive.name}": data,
            f"/v{alpha2.__version__}/checksums.sha256": (
                f"{'0' * 64}  other.tar.gz\n{digest}  {archive.name}\n"
            ).encode(),
        }
        requests: list[str] = []
        server = _serve(files, requests, tls=tls)
        servers.append(server)
        scheme = "https" if tls else "http"
        return Release(
            base,
            f"{scheme}://127.0.0.1:{server.server_address[1]}",
            archive.name,
            archive_sha256,
            requests,
        )

    yield serve
    for server in servers:
        server.shutdown()


# The Node client's test certificates: a CA, and a server certificate for 127.0.0.1 it issued.
TLS_FIXTURES = Path(__file__).resolve().parents[3] / "packages/alpha2-node/test/fixtures/tls"


def _serve(
    files: dict[str, bytes], requests: list[str], tls: bool = False
) -> http.server.ThreadingHTTPServer:
    """Serves ``files`` by path from localhost, recording every path asked for in ``requests``."""

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            requests.append(self.path)
            body = files.get(self.path)
            self.send_response(200 if body is not None else 404)
            self.send_header("Content-Length", str(len(body or b"")))
            self.end_headers()
            self.wfile.write(body or b"")

        def log_message(self, *_: object) -> None:
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    if tls:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(TLS_FIXTURES / "server.pem", TLS_FIXTURES / "server-key.pem")
        server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


@pytest.fixture
def forward_proxy() -> Iterator[tuple[str, list[str]]]:
    """An HTTP proxy on localhost that tunnels CONNECT requests; yields its URL and the request
    lines it saw."""
    lines: list[str] = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_CONNECT(self) -> None:
            lines.append(f"CONNECT {self.path}")
            host, _, port = self.path.rpartition(":")
            upstream = socket.create_connection((host, int(port)))
            self.send_response(200, "Connection Established")
            self.end_headers()
            sockets = [self.connection, upstream]
            while True:
                readable, _, _ = select.select(sockets, [], [], 5)
                if not readable:
                    break
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        upstream.close()
                        return
                    (upstream if source is self.connection else self.connection).sendall(data)
            upstream.close()

        def log_message(self, *_: object) -> None:
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_address[1]}", lines
    server.shutdown()


@pytest.fixture
def no_proxy_environment(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """No proxy variables and a fresh SSL context for the test."""
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy"):
        monkeypatch.delenv(name, raising=False)
    install_module._ssl_context.cache_clear()
    yield
    install_module._ssl_context.cache_clear()


def test_the_target_follows_the_machine_rosetta_uses_macos_arm64_windows_arm64_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    assert host_target("linux", "x86_64") == "linux-x64"
    assert host_target("linux", "aarch64") == "linux-arm64"
    assert host_target("darwin", "arm64") == "macos-arm64"
    assert host_target("win32", "AMD64") == "win-x64"
    assert host_target("win32", "ARM64") is None
    assert host_target("freebsd14", "amd64") is None
    monkeypatch.setattr(install_module, "_apple_silicon", lambda: True)
    assert host_target("darwin", "x86_64") == "macos-arm64"
    monkeypatch.setattr(install_module, "_apple_silicon", lambda: False)
    assert host_target("darwin", "x86_64") is None

    refusals = [
        ("darwin", "x86_64", "Intel Macs are not supported"),
        (
            "win32",
            "arm64",
            "there is no Windows on ARM alpha2-host; an x64 Node.js/Python runs the x64 host "
            "under emulation (not supported or tested)",
        ),
        ("freebsd14", "amd64", "no alpha2-host archive is published for freebsd14-amd64"),
    ]
    cache = tmp_path / "cache"
    for system, machine, message in refusals:
        monkeypatch.setattr(install_module, "_this_machine", lambda s=system, m=machine: (s, m))
        for install in (install_host, install_openh264):
            with pytest.raises(Alpha2HostInstallError) as caught:
                install(cache_directory=cache, download_url="http://127.0.0.1:9")
            assert caught.value.code == "UNSUPPORTED_PLATFORM"
            assert caught.value.message == message
    assert not cache.exists()


def test_windows_takes_the_interpreters_architecture_from_sysconfig(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(install_module.sys, "platform", "win32")
    for name, target in (("win-amd64", "win-x64"), ("win-arm64", None)):
        monkeypatch.setattr(install_module.sysconfig, "get_platform", lambda n=name: n)
        assert host_target() == target


@supported
def test_musl_and_old_glibc_are_refused_before_any_download_naming_the_floor(
    tmp_path: Path, serve_release: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    cache = tmp_path / "cache"
    monkeypatch.setattr(install_module, "_this_machine", lambda: ("linux", "x86_64"))
    floor = LINUX_GLIBC_FLOOR
    older = f"2.{int(floor.split('.')[1]) - 1}"
    cases = [
        (
            ("musl", None),
            f"alpha2-host is built for glibc {floor} or later; this system's C library is musl "
            "(Alpine and other musl distributions are not supported)",
        ),
        (("unknown", None), "this system's C library is not glibc"),
        (
            ("glibc", older),
            f"needs glibc {floor} or later; this system has glibc {older}. Build it from source "
            "(cargo install alpha2-host with FFmpeg 8 development files) and set ALPHA2_HOST_PATH",
        ),
        # Compared as numbers: 2.9 sorts after the floor as text but is older.
        (("glibc", "2.9"), "this system has glibc 2.9."),
    ]
    for libc, message in cases:
        monkeypatch.setattr(install_module, "_linux_libc", lambda libc=libc: libc)
        for install in (install_host, install_openh264):
            with pytest.raises(Alpha2HostInstallError) as caught:
                install(cache_directory=cache, download_url=release.download_url)
            assert caught.value.code == "UNSUPPORTED_PLATFORM"
            assert message in caught.value.message
    assert release.requests == []
    assert not cache.exists()


def test_the_glibc_floor_is_the_minimum_os_the_ffmpeg_lock_file_names_for_linux() -> None:
    lock = tomllib.loads((ROOT / "release" / "ffmpeg-lgpl" / "manifest.toml").read_text("utf-8"))
    floors = {
        target: asset["minimum_os"]
        for target, asset in lock["assets"].items()
        if target.startswith("linux-")
    }
    assert floors == {
        "linux-x64": f"glibc {LINUX_GLIBC_FLOOR}",
        "linux-arm64": f"glibc {LINUX_GLIBC_FLOOR}",
    }


@supported
def test_store_python_warns_that_the_cache_is_private(
    tmp_path: Path,
    serve_release: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    from alpha2.__main__ import main

    release: Release = serve_release()  # type: ignore[operator]
    local = tmp_path / "Local"
    store = "C:/Program Files/WindowsApps/PythonSoftwareFoundation.Python.3.12_3.12.10_x64__qbz5n2kfra8p0"
    monkeypatch.setattr(install_module.sys, "prefix", store)
    monkeypatch.setenv("LOCALAPPDATA", str(local))
    with pytest.warns(RuntimeWarning, match="this is the Microsoft Store Python"):
        install_host(cache_directory=local / "alpha2", download_url=release.download_url)
    # A cache outside %LOCALAPPDATA% is shared, so nothing is said.
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        install_host(cache_directory=tmp_path / "shared", download_url=release.download_url)

    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", str(local / "alpha2"))
    monkeypatch.setenv("ALPHA2_HOST_DOWNLOAD_URL", release.download_url)
    assert main(["install"]) == 0
    err = capsys.readouterr().err
    assert (
        "alpha2 install: this is the Microsoft Store Python: Windows keeps what it writes under "
        "%LOCALAPPDATA% private to it, so other alpha2 clients will not see this install; set "
        "ALPHA2_HOST_CACHE_DIR to a folder outside %LOCALAPPDATA% to share it"
    ) in err


def test_the_cache_is_alpha2_host_cache_dir_when_set_and_a_blank_one_is_unset(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", "cache")
    assert host_cache_directory(tmp_path) == (tmp_path / "cache").resolve()
    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", " ")
    assert host_cache_directory(tmp_path).name == "alpha2"
    assert host_cache_directory(tmp_path) != tmp_path / "cache"


@supported
def test_an_installed_archive_is_found_and_is_not_downloaded_again(
    tmp_path: Path, serve_release: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    cache = tmp_path / "cache"
    progress: list[tuple[int, int | None]] = []
    result = install_host(
        cache_directory=cache,
        download_url=f"{release.download_url}/",
        on_progress=lambda received, total: progress.append((received, total)),
    )
    host_path = cache / release.base / "bin" / host_executable_name()
    assert result == alpha2.InstallResult(
        host_path, cache / release.base, downloaded=True, verified_by="checksums"
    )
    assert progress[-1][0] == progress[-1][1]
    assert [path.name for path in cache.iterdir()] == [release.base]
    if sys.platform != "win32":
        assert (cache / release.base / "lib" / "libavutil.so.60").read_text() == "lib"

    monkeypatch.setenv("PATH", "")
    monkeypatch.delenv("ALPHA2_HOST_PATH", raising=False)
    assert resolve_host_binary(cache_directory=cache) == host_path

    requests = len(release.requests)
    again = install_host(cache_directory=cache, download_url=release.download_url)
    assert again == alpha2.InstallResult(
        host_path, cache / release.base, downloaded=False, verified_by=None
    )
    assert len(release.requests) == requests
    forced = install_host(cache_directory=cache, download_url=release.download_url, force=True)
    assert forced.downloaded is True
    assert [path.name for path in cache.iterdir()] == [release.base]


@supported
def test_an_archive_whose_checksum_does_not_match_leaves_nothing_in_the_cache(
    tmp_path: Path, serve_release: object
) -> None:
    release: Release = serve_release(checksum="a" * 64)  # type: ignore[operator]
    cache = tmp_path / "cache"
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_host(cache_directory=cache, download_url=release.download_url)
    assert caught.value.code == "CHECKSUM_MISMATCH"
    assert list(cache.iterdir()) == []


@pytest.fixture
def pins_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Where the installer reads its release pins in this test, as a published wheel carries them
    in ``alpha2/_host_archives.json``; nothing is written there until the test writes it."""
    path = tmp_path / "pins" / "_host_archives.json"
    path.parent.mkdir()
    monkeypatch.setattr(install_module, "_pins_file", lambda: path)
    return path


def _checksums_requests(release: Release) -> list[str]:
    return [path for path in release.requests if path.endswith("/checksums.sha256")]


@supported
def test_a_release_pin_wins_over_checksums_and_over_a_mirror(
    tmp_path: Path,
    serve_release: object,
    pins_file: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    from alpha2.__main__ import main

    # The mirror serves an archive and a checksums file that agree with each other.
    release: Release = serve_release()  # type: ignore[operator]
    other = "b" * 64
    pins_file.write_text(
        json.dumps({"version": alpha2.__version__, "archives": {host_target(): other}})
    )
    cache = tmp_path / "cache"
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_host(cache_directory=cache, download_url=release.download_url)
    assert caught.value.code == "CHECKSUM_MISMATCH"
    assert str(caught.value) == (
        f"{release.archive} has SHA-256 {release.archive_sha256}; the release pins {other}"
    )
    assert list(cache.iterdir()) == []

    pins_file.write_text(
        json.dumps(
            {
                "version": alpha2.__version__,
                "archives": {"linux-x64": other, host_target(): release.archive_sha256},
            }
        )
    )
    result = install_host(cache_directory=cache, download_url=release.download_url)
    assert result.downloaded is True
    assert result.verified_by == "release-pin"
    assert _checksums_requests(release) == []

    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", str(tmp_path / "cli-cache"))
    monkeypatch.setenv("ALPHA2_HOST_DOWNLOAD_URL", release.download_url)
    assert main(["install"]) == 0
    assert capsys.readouterr().out.endswith("(checked against the SHA-256 this package pins)\n")
    assert _checksums_requests(release) == []


@supported
def test_without_a_pin_the_release_checksums_are_used(
    tmp_path: Path,
    serve_release: object,
    pins_file: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    from alpha2.__main__ import main

    release: Release = serve_release()  # type: ignore[operator]
    unpinned = install_host(cache_directory=tmp_path / "cache", download_url=release.download_url)
    assert unpinned.verified_by == "checksums"
    assert len(_checksums_requests(release)) == 1

    # Pins of another version, or without this platform, do not apply.
    for index, pins in enumerate(
        [
            {"version": "0.0.1", "archives": {host_target(): "b" * 64}},
            {"version": alpha2.__version__, "archives": {}},
        ]
    ):
        pins_file.write_text(json.dumps(pins))
        result = install_host(
            cache_directory=tmp_path / f"cache-{index}", download_url=release.download_url
        )
        assert result.verified_by == "checksums"
    assert len(_checksums_requests(release)) == 3

    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", str(tmp_path / "cli-cache"))
    monkeypatch.setenv("ALPHA2_HOST_DOWNLOAD_URL", release.download_url)
    assert main(["install"]) == 0
    assert capsys.readouterr().out == (
        f"installed {tmp_path / 'cli-cache' / release.base / 'bin' / host_executable_name()} "
        "(checked against the release's checksums.sha256)\n"
    )


@supported
@pytest.mark.parametrize(
    ("pins", "reason"),
    [
        ("{", r"cannot read the release pins in .*_host_archives\.json \("),
        ('{"archives": {}}', r"\(it has no version and archives\)"),
        (None, r"entry is not a SHA-256\); reinstall the alpha2 package$"),
    ],
)
def test_a_damaged_pins_file_is_refused_before_anything_is_downloaded(
    tmp_path: Path, serve_release: object, pins_file: Path, pins: str | None, reason: str
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    if pins is None:
        pins = json.dumps({"version": alpha2.__version__, "archives": {host_target(): "ABC"}})
    pins_file.write_text(pins)
    with pytest.raises(Alpha2HostInstallError, match=reason) as caught:
        install_host(cache_directory=tmp_path / "cache", download_url=release.download_url)
    assert caught.value.code == "CHECKSUM_MISMATCH"
    assert not (tmp_path / "cache").exists()
    assert release.requests == []


@supported
def test_a_release_that_is_not_published_is_a_download_failure(
    tmp_path: Path, serve_release: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    cache = tmp_path / "cache"
    # The environment names the mirror when no argument does.
    monkeypatch.setenv("ALPHA2_HOST_DOWNLOAD_URL", f"{release.download_url}/missing")
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_host(cache_directory=cache)
    assert caught.value.code == "DOWNLOAD_FAILED"
    assert "answered 404; the release may not be published" in caught.value.message
    assert list(cache.iterdir()) == []


@supported
def test_a_cancelled_install_leaves_nothing_in_the_cache(
    tmp_path: Path, serve_release: object
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    cache = tmp_path / "cache"
    cancel = threading.Event()
    cancel.set()
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_host(cache_directory=cache, download_url=release.download_url, cancel=cancel)
    assert caught.value.code == "ABORTED"
    assert list(cache.iterdir()) == []


@supported
def test_the_command_line_installs_into_the_cache(
    tmp_path: Path,
    serve_release: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    from alpha2.__main__ import main

    release: Release = serve_release()  # type: ignore[operator]
    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", str(tmp_path / "cache"))
    monkeypatch.setenv("ALPHA2_HOST_DOWNLOAD_URL", release.download_url)
    assert main(["install"]) == 0
    assert capsys.readouterr().out.startswith("installed ")
    assert main(["install"]) == 0
    assert capsys.readouterr().out.startswith("already installed: ")
    monkeypatch.setenv("ALPHA2_HOST_DOWNLOAD_URL", f"{release.download_url}/missing")
    assert main(["install", "--force"]) == 1
    assert "answered 404" in capsys.readouterr().err


FAKE_LIBRARY = b"a stand-in for Cisco's OpenH264 library"


@dataclass
class OpenH264Mirror:
    directory_name: str
    library: str
    download_url: str
    requests: list[str] = field(default_factory=list)


@pytest.fixture
def serve_openh264(monkeypatch: pytest.MonkeyPatch) -> Iterator[object]:
    """Serves a stand-in for this platform's OpenH264 archive as ``<base>/<archive>`` and makes the
    target table expect it, or the checksums given."""
    servers: list[http.server.ThreadingHTTPServer] = []

    def serve(
        archive_sha256: str | None = None, library_sha256: str | None = None
    ) -> OpenH264Mirror:
        target = host_target()
        assert target is not None
        archive = bz2.compress(FAKE_LIBRARY)
        entry = dataclasses.replace(
            install_module._OPENH264_TARGETS[target],
            archive_sha256=archive_sha256 or hashlib.sha256(archive).hexdigest(),
            library_sha256=library_sha256 or hashlib.sha256(FAKE_LIBRARY).hexdigest(),
        )
        monkeypatch.setitem(install_module._OPENH264_TARGETS, target, entry)
        requests: list[str] = []
        server = _serve({f"/{entry.archive}": archive}, requests)
        servers.append(server)
        return OpenH264Mirror(
            f"openh264-2.7.0-{target}",
            entry.library,
            f"http://127.0.0.1:{server.server_address[1]}",
            requests,
        )

    yield serve
    for server in servers:
        server.shutdown()


@supported
def test_openh264_installs_into_the_cache_and_is_not_downloaded_again(
    tmp_path: Path, serve_openh264: object
) -> None:
    mirror: OpenH264Mirror = serve_openh264()  # type: ignore[operator]
    cache = tmp_path / "cache"
    progress: list[tuple[int, int | None]] = []
    result = install_openh264(
        cache_directory=cache,
        download_url=f"{mirror.download_url}/",
        on_progress=lambda received, total: progress.append((received, total)),
    )
    directory = cache / mirror.directory_name
    library_path = directory / mirror.library
    assert result == alpha2.OpenH264InstallResult(
        library_path,
        directory,
        downloaded=True,
        version="2.7.0",
        notice="OpenH264 Video Codec provided by Cisco Systems, Inc.",
    )
    assert progress[-1][0] == progress[-1][1]
    assert library_path.read_bytes() == FAKE_LIBRARY
    assert [path.name for path in cache.iterdir()] == [mirror.directory_name]
    assert [path.name for path in directory.iterdir()] == [mirror.library]

    requests = len(mirror.requests)
    assert install_openh264(cache_directory=cache).downloaded is False
    assert len(mirror.requests) == requests

    # A library that is not the one expected is replaced.
    library_path.write_bytes(b"damaged")
    repaired = install_openh264(cache_directory=cache, download_url=mirror.download_url)
    assert repaired.downloaded is True
    assert library_path.read_bytes() == FAKE_LIBRARY

    forced = install_openh264(cache_directory=cache, download_url=mirror.download_url, force=True)
    assert forced.downloaded is True
    assert len(mirror.requests) == requests + 2
    assert [path.name for path in cache.iterdir()] == [mirror.directory_name]


@supported
@pytest.mark.parametrize("checksum", ["archive_sha256", "library_sha256"])
def test_an_openh264_archive_or_library_whose_checksum_does_not_match_leaves_nothing(
    tmp_path: Path, serve_openh264: object, checksum: str
) -> None:
    mirror: OpenH264Mirror = serve_openh264(**{checksum: "a" * 64})  # type: ignore[operator]
    cache = tmp_path / "cache"
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_openh264(cache_directory=cache, download_url=mirror.download_url)
    assert caught.value.code == "CHECKSUM_MISMATCH"
    assert list(cache.iterdir()) == []


@supported
def test_openh264_is_downloaded_from_alpha2_openh264_download_url_when_no_url_is_given(
    tmp_path: Path, serve_openh264: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    mirror: OpenH264Mirror = serve_openh264()  # type: ignore[operator]
    cache = tmp_path / "cache"
    monkeypatch.setenv("ALPHA2_OPENH264_DOWNLOAD_URL", f"{mirror.download_url}/missing")
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_openh264(cache_directory=cache)
    assert caught.value.code == "DOWNLOAD_FAILED"
    assert "/missing/" in caught.value.message
    assert "answered 404" in caught.value.message
    assert list(cache.iterdir()) == []


@supported
def test_a_cancelled_openh264_install_leaves_nothing_in_the_cache(
    tmp_path: Path, serve_openh264: object
) -> None:
    mirror: OpenH264Mirror = serve_openh264()  # type: ignore[operator]
    cache = tmp_path / "cache"
    cancel = threading.Event()
    cancel.set()
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_openh264(cache_directory=cache, download_url=mirror.download_url, cancel=cancel)
    assert caught.value.code == "ABORTED"
    assert list(cache.iterdir()) == []


@supported
def test_a_platform_without_an_openh264_binary_is_refused_before_any_download(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delitem(install_module._OPENH264_TARGETS, host_target())
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_openh264(cache_directory=tmp_path / "cache")
    assert caught.value.code == "UNSUPPORTED_PLATFORM"
    assert not (tmp_path / "cache").exists()


@supported
def test_the_command_line_installs_openh264_and_prints_ciscos_notice_every_time(
    tmp_path: Path,
    serve_openh264: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    from alpha2.__main__ import main

    mirror: OpenH264Mirror = serve_openh264()  # type: ignore[operator]
    monkeypatch.setenv("ALPHA2_HOST_CACHE_DIR", str(tmp_path / "cache"))
    monkeypatch.setenv("ALPHA2_OPENH264_DOWNLOAD_URL", mirror.download_url)
    library_path = (tmp_path / "cache").resolve() / mirror.directory_name / mirror.library
    for first in (
        f"installed OpenH264 2.7.0: {library_path}",
        f"already installed: {library_path}",
    ):
        assert main(["install", "--openh264"]) == 0
        lines = capsys.readouterr().out.splitlines()
        assert lines[0] == first
        assert lines[1] == "OpenH264 Video Codec provided by Cisco Systems, Inc."
        assert lines[-1].endswith("set ALPHA2_OPENH264=0 to turn it off.")


def test_the_install_command_has_no_thin(capsys: pytest.CaptureFixture[str]) -> None:
    from alpha2.__main__ import main

    with pytest.raises(SystemExit) as caught:
        main(["install", "--thin"])
    assert caught.value.code == 2
    assert "unrecognized arguments: --thin" in capsys.readouterr().err


@pytest.mark.skipif(sys.platform != "linux", reason="a Linux system without a CA store")
def test_a_system_without_a_ca_store_uses_certifi(
    monkeypatch: pytest.MonkeyPatch, no_proxy_environment: None
) -> None:
    empty = ssl.DefaultVerifyPaths(None, None, "SSL_CERT_FILE", "/nonexistent", "SSL_CERT_DIR", "")  # type: ignore[call-arg]
    monkeypatch.setattr(install_module.ssl, "get_default_verify_paths", lambda: empty)
    monkeypatch.delenv("SSL_CERT_FILE", raising=False)
    monkeypatch.delenv("SSL_CERT_DIR", raising=False)
    import certifi

    context = install_module._ssl_context()
    assert context.cert_store_stats()["x509_ca"] > 100
    assert context.verify_mode == ssl.CERT_REQUIRED
    assert Path(certifi.where()).is_file()


@pytest.mark.skipif(sys.platform == "linux", reason="Linux verifies with OpenSSL's store")
def test_macos_and_windows_use_the_os_verifier(no_proxy_environment: None) -> None:
    import truststore

    assert isinstance(install_module._ssl_context(), truststore.SSLContext)


def test_download_failure_names_the_proxy_ca_and_mirror_variables(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, no_proxy_environment: None
) -> None:
    if host_target() is None:
        pytest.skip("no archive is published here")
    monkeypatch.setenv("HTTPS_PROXY", "http://127.0.0.1:9")
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_host(cache_directory=tmp_path / "cache", download_url="https://alpha2.invalid")
    message = caught.value.message
    assert caught.value.code == "DOWNLOAD_FAILED"
    assert message.startswith("cannot download https://alpha2.invalid/")
    assert "HTTPS_PROXY" in message
    assert "SSL_CERT_FILE" in message
    assert "ALPHA2_HOST_DOWNLOAD_URL" in message


@supported
@pytest.mark.skipif(
    sys.platform != "linux", reason="SSL_CERT_FILE moves the CA store on Linux only"
)
def test_https_proxy_is_used(
    tmp_path: Path,
    serve_release: object,
    forward_proxy: tuple[str, list[str]],
    monkeypatch: pytest.MonkeyPatch,
    no_proxy_environment: None,
) -> None:
    release: Release = serve_release(tls=True)  # type: ignore[operator]
    proxy, lines = forward_proxy
    monkeypatch.setenv("HTTPS_PROXY", proxy)
    monkeypatch.setenv("SSL_CERT_FILE", str(TLS_FIXTURES / "ca.pem"))
    result = install_host(cache_directory=tmp_path / "cache", download_url=release.download_url)
    assert result.downloaded is True
    authority = release.download_url.removeprefix("https://")
    assert lines == [f"CONNECT {authority}", f"CONNECT {authority}"]


@supported
def test_installs_into_a_non_ascii_cache(tmp_path: Path, serve_release: object) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    # Hangul and an emoji are outside every Western ANSI code page, and the accented letter is
    # outside the East Asian ones.
    for name in ("\uc54c\ud30c2 \uce90\uc2dc \U0001f600", "caf\u00e9 cache"):
        cache = tmp_path / name
        result = install_host(cache_directory=cache, download_url=release.download_url)
        assert result.downloaded is True
        assert result.host_path.read_text() == "host"
        assert [path.name for path in cache.iterdir()] == [release.base]


@supported
def test_force_over_a_directory_in_use_fails_and_keeps_it(
    tmp_path: Path, serve_release: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    cache = tmp_path / "cache"
    installed = install_host(cache_directory=cache, download_url=release.download_url)
    directory = installed.directory
    held = None
    if sys.platform == "win32":
        # A file open without delete sharing keeps Windows from renaming its directory.
        held = installed.host_path.open("rb")
    else:
        real_rename = install_module._rename

        def busy(source: Path, destination: Path) -> None:
            if source == directory:
                raise PermissionError(16, "Device or resource busy")
            real_rename(source, destination)

        monkeypatch.setattr(install_module, "_rename", busy)
    before = sorted(path.relative_to(directory) for path in directory.rglob("*"))
    try:
        with pytest.raises(Alpha2HostInstallError) as caught:
            install_host(cache_directory=cache, download_url=release.download_url, force=True)
    finally:
        if held is not None:
            held.close()
    assert caught.value.code == "EXTRACT_FAILED"
    assert (
        "it is in use (is an alpha2-host from it running?); the installed copy is unchanged"
        in caught.value.message
    )
    assert sorted(path.relative_to(directory) for path in directory.rglob("*")) == before
    assert [path.name for path in cache.iterdir()] == [release.base]


@supported
def test_a_transiently_refused_rename_is_retried(
    tmp_path: Path, serve_release: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    release: Release = serve_release()  # type: ignore[operator]
    real_rename = install_module._rename
    refused = []

    def flaky(source: Path, destination: Path) -> None:
        if len(refused) < 2:
            refused.append(source)
            raise PermissionError(13, "Access is denied")
        real_rename(source, destination)

    monkeypatch.setattr(install_module, "_retries_renames", lambda: True)
    monkeypatch.setattr(install_module, "_rename", flaky)
    cache = tmp_path / "cache"
    result = install_host(cache_directory=cache, download_url=release.download_url)
    assert result.downloaded is True
    assert len(refused) == 2
    assert [path.name for path in cache.iterdir()] == [release.base]


@pytest.mark.skipif(sys.platform == "win32", reason="Windows files have no numeric owner")
def test_extraction_never_restores_archive_owners(tmp_path: Path) -> None:
    archive = tmp_path / "owned.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        for name, data in (("top", None), ("top/file", b"x")):
            info = tarfile.TarInfo(name)
            info.uid = info.gid = 1001
            info.uname = info.gname = "runner"
            if data is None:
                info.type = tarfile.DIRTYPE
                info.mode = 0o755
                bundle.addfile(info)
            else:
                info.size = len(data)
                bundle.addfile(info, io.BytesIO(data))
    destination = tmp_path / "out"
    destination.mkdir()
    install_module._extract(archive, destination)
    for path in (destination / "top", destination / "top" / "file"):
        assert path.stat().st_uid == os.getuid()
        assert path.stat().st_gid == os.getgid()
