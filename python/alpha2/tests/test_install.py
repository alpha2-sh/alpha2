"""Installing the host into the shared cache, from a release served on localhost."""

from __future__ import annotations

import hashlib
import http.server
import os
import sys
import tarfile
import threading
import zipfile
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path

import pytest

import alpha2
from alpha2 import Alpha2HostInstallError, host_cache_directory, host_target, install_host
from alpha2.client import resolve_host_binary
from alpha2.install import host_executable_name

supported = pytest.mark.skipif(host_target() is None, reason="no archive is published here")


@dataclass
class Release:
    base: str
    download_url: str
    requests: list[str] = field(default_factory=list)


@pytest.fixture
def serve_release(tmp_path: Path) -> Iterator[object]:
    """Serves a release of this package's version for this platform, as ``install_host`` reads it:
    the archive and ``checksums.sha256`` under ``/v<version>/``."""
    servers: list[http.server.ThreadingHTTPServer] = []

    def serve(kind: str = "bundled", checksum: str | None = None) -> Release:
        root = tmp_path / f"release-{len(servers)}"
        base = f"alpha2-{alpha2.__version__}-{host_target()}-{kind}"
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
        digest = checksum or hashlib.sha256(data).hexdigest()
        files = {
            f"/v{alpha2.__version__}/{archive.name}": data,
            f"/v{alpha2.__version__}/checksums.sha256": (
                f"{'0' * 64}  other.tar.gz\n{digest}  {archive.name}\n"
            ).encode(),
        }
        requests: list[str] = []

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
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
        return Release(base, f"http://127.0.0.1:{server.server_address[1]}", requests)

    yield serve
    for server in servers:
        server.shutdown()


def test_the_release_target_is_named_for_each_published_platform() -> None:
    assert host_target("linux", "x86_64") == "linux-x64"
    assert host_target("linux", "aarch64") == "linux-arm64"
    assert host_target("darwin", "arm64") == "macos-arm64"
    assert host_target("win32", "AMD64") == "win-x64"
    assert host_target("darwin", "x86_64") is None
    assert host_target("freebsd14", "amd64") is None


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
    assert result == alpha2.InstallResult(host_path, cache / release.base, downloaded=True)
    assert progress[-1][0] == progress[-1][1]
    assert [path.name for path in cache.iterdir()] == [release.base]
    if sys.platform != "win32":
        assert (cache / release.base / "lib" / "libavutil.so.60").read_text() == "lib"

    monkeypatch.setenv("PATH", "")
    monkeypatch.delenv("ALPHA2_HOST_PATH", raising=False)
    assert resolve_host_binary(cache_directory=cache) == host_path

    requests = len(release.requests)
    again = install_host(cache_directory=cache, download_url=release.download_url)
    assert again.downloaded is False
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


@pytest.mark.skipif(sys.platform != "win32", reason="every other platform has a thin archive")
def test_a_kind_the_platform_is_not_published_in_is_refused_before_any_download() -> None:
    with pytest.raises(Alpha2HostInstallError) as caught:
        install_host(kind="thin")
    assert caught.value.code == "UNSUPPORTED_PLATFORM"


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
