"""Downloads ``alpha2-host`` into the cache shared with every alpha2 client.

The cache layout is the one ``@alpha2-sh/node`` uses: each release archive unpacks into
``<cache>/alpha2-<version>-<target>-<kind>/`` with ``bin/alpha2-host`` beside the ``lib/`` it loads,
so one download serves both clients.
"""

from __future__ import annotations

import hashlib
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import urllib.error
import urllib.request
import zipfile
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from . import _native
from .errors import Alpha2HostInstallError

__all__ = [
    "HOST_DOWNLOAD_URL",
    "HostArchiveKind",
    "InstallResult",
    "cached_host_binary",
    "host_cache_directory",
    "host_executable_name",
    "host_target",
    "install_host",
    "package_version",
]

HostArchiveKind = Literal["bundled", "thin"]
"""A native archive's kind: ``bundled`` carries FFmpeg, ``thin`` loads the system's FFmpeg 8."""

HOST_DOWNLOAD_URL = "https://github.com/alpha2-sh/alpha2/releases/download"
"""Where release archives are downloaded from: ``<base>/v<version>/<archive>``."""

# The release targets, by platform and machine, and the kinds each is published in.
_TARGETS: dict[tuple[str, str], tuple[str, tuple[HostArchiveKind, ...]]] = {
    ("linux", "x86_64"): ("linux-x64", ("bundled", "thin")),
    ("linux", "aarch64"): ("linux-arm64", ("bundled", "thin")),
    ("darwin", "arm64"): ("macos-arm64", ("bundled", "thin")),
    ("win32", "amd64"): ("win-x64", ("bundled",)),
}

# Spellings of one machine across operating systems.
_MACHINES = {"amd64": "x86_64", "x64": "x86_64", "arm64": "aarch64"}


def _platform_key(system: str | None = None, machine: str | None = None) -> tuple[str, str]:
    """``(system, machine)`` as the target table spells them; this computer's by default."""
    system = system if system is not None else sys.platform
    machine = (machine if machine is not None else platform.machine()).lower()
    if system == "win32":
        return system, "amd64" if machine in ("amd64", "x86_64") else machine
    if system == "darwin":
        return system, "arm64" if machine in ("arm64", "aarch64") else machine
    if system.startswith("linux"):
        return "linux", _MACHINES.get(machine, machine)
    return system, machine


def host_executable_name() -> str:
    """The file name ``alpha2-host`` has here: Windows executables carry ``.exe``."""
    return "alpha2-host.exe" if sys.platform == "win32" else "alpha2-host"


def host_target(system: str | None = None, machine: str | None = None) -> str | None:
    """The release target that carries ``alpha2-host``, such as ``macos-arm64``.

    For this computer, or for ``system`` (as ``sys.platform`` names it) and ``machine`` (as
    ``platform.machine()`` does). ``None`` when no archive is published for them.
    """
    entry = _TARGETS.get(_platform_key(system, machine))
    return entry[0] if entry else None


def host_cache_directory(cwd: str | os.PathLike[str] | None = None) -> Path:
    """The directory :func:`install_host` unpacks archives into and the client looks in.

    ``ALPHA2_HOST_CACHE_DIR`` when it is set (relative to ``cwd``), else ``alpha2`` in the
    platform's cache directory: ``$XDG_CACHE_HOME`` or ``~/.cache`` on Linux,
    ``~/Library/Caches`` on macOS, ``%LOCALAPPDATA%`` on Windows.
    """
    from_environment = os.environ.get("ALPHA2_HOST_CACHE_DIR", "")
    if from_environment.strip():
        return Path(cwd or os.getcwd(), from_environment).resolve()
    if sys.platform == "win32":
        local = os.environ.get("LOCALAPPDATA", "")
        base = Path(local) if local.strip() else Path.home() / "AppData" / "Local"
        return base / "alpha2"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Caches" / "alpha2"
    xdg = os.environ.get("XDG_CACHE_HOME", "")
    base = Path(xdg) if xdg and os.path.isabs(xdg) else Path.home() / ".cache"
    return base / "alpha2"


def package_version() -> str | None:
    """This package's version, which is the version of the ``alpha2-host`` it speaks to.

    They are released together. It is spelled as the release archives are (Cargo's ``0.5.0-rc.1``,
    not Python's ``0.5.0rc1``).
    """
    return _native.__version__


def _archive_base(version: str, target: str, kind: str) -> str:
    return f"alpha2-{version}-{target}-{kind}"


def cached_host_binary(cache_directory: str | os.PathLike[str]) -> Path | None:
    """The installed ``alpha2-host`` of this package's version in ``cache_directory``.

    The bundled archive's before the thin one's; ``None`` when neither is there.
    """
    version = package_version()
    entry = _TARGETS.get(_platform_key())
    if version is None or entry is None:
        return None
    target, kinds = entry
    for kind in kinds:
        candidate = (
            Path(cache_directory)
            / _archive_base(version, target, kind)
            / "bin"
            / host_executable_name()
        )
        if candidate.is_file():
            return candidate
    return None


@dataclass(frozen=True)
class InstallResult:
    """What :func:`install_host` installed."""

    host_path: Path
    """The installed ``alpha2-host``, which the client now finds."""
    directory: Path
    """The archive's directory in the cache, which holds ``bin/`` and ``lib/``."""
    downloaded: bool
    """Whether it was downloaded now; ``False`` when it was already installed."""


def install_host(
    *,
    cache_directory: str | os.PathLike[str] | None = None,
    kind: HostArchiveKind = "bundled",
    force: bool = False,
    download_url: str | None = None,
    on_progress: Callable[[int, int | None], None] | None = None,
    cancel: threading.Event | None = None,
) -> InstallResult:
    """Downloads the native archive of this package's version for this platform.

    The archive comes from the GitHub release (or ``download_url``, a mirror of
    :data:`HOST_DOWNLOAD_URL`; default ``ALPHA2_HOST_DOWNLOAD_URL`` when set), is checked against
    the release's ``checksums.sha256``, and is unpacked into the cache, where the client finds
    it. An archive already installed is not downloaded again unless ``force`` is set. The archive
    appears in the cache whole or not at all, so installs running at the same time do not see each
    other's partial work. ``on_progress(received, total)`` is called as it downloads; ``total``
    is ``None`` when the server did not say. Setting ``cancel`` stops the install.

    Raises :class:`~alpha2.Alpha2HostInstallError`.
    """
    entry = _TARGETS.get(_platform_key())
    if entry is None:
        system, machine = _platform_key()
        raise Alpha2HostInstallError(
            "UNSUPPORTED_PLATFORM", f"no alpha2-host archive is published for {system}-{machine}"
        )
    target, kinds = entry
    if kind not in kinds:
        raise Alpha2HostInstallError(
            "UNSUPPORTED_PLATFORM",
            f"{target} has no {kind} archive; it is published as {', '.join(kinds)}",
        )
    version = package_version()
    if version is None:
        raise Alpha2HostInstallError("UNSUPPORTED_PLATFORM", "cannot read this package's version")

    cache = Path(cache_directory) if cache_directory is not None else host_cache_directory()
    base = _archive_base(version, target, kind)
    directory = cache / base
    host_path = directory / "bin" / host_executable_name()
    if not force and host_path.is_file():
        return InstallResult(host_path, directory, downloaded=False)

    archive = f"{base}.{'zip' if sys.platform == 'win32' else 'tar.gz'}"
    from_environment = os.environ.get("ALPHA2_HOST_DOWNLOAD_URL", "")
    url = download_url or (from_environment if from_environment.strip() else HOST_DOWNLOAD_URL)
    release_url = f"{url.rstrip('/')}/v{version}"

    cache.mkdir(parents=True, exist_ok=True)
    # Staged in the cache itself, so the finished directory moves into place with one rename.
    staging = Path(tempfile.mkdtemp(prefix=".install-", dir=cache))
    try:
        listing = _download_text(f"{release_url}/checksums.sha256", cancel)
        expected = _checksum_of(listing, archive)
        if expected is None:
            raise Alpha2HostInstallError(
                "DOWNLOAD_FAILED", f"{release_url}/checksums.sha256 lists no {archive}"
            )
        archive_path = staging / archive
        actual = _download_file(f"{release_url}/{archive}", archive_path, on_progress, cancel)
        if actual != expected:
            raise Alpha2HostInstallError(
                "CHECKSUM_MISMATCH",
                f"{archive} has SHA-256 {actual}; checksums.sha256 says {expected}",
            )
        unpacked = staging / "unpacked"
        unpacked.mkdir()
        _extract(archive_path, unpacked)
        staged = unpacked / base
        if not (staged / "bin" / host_executable_name()).is_file():
            raise Alpha2HostInstallError(
                "EXTRACT_FAILED", f"{archive} does not hold {base}/bin/{host_executable_name()}"
            )
        if force:
            shutil.rmtree(directory, ignore_errors=True)
        try:
            os.rename(staged, directory)
        except OSError as error:
            # Another install moved the same archive into place first.
            if not host_path.is_file():
                raise Alpha2HostInstallError(
                    "EXTRACT_FAILED", f"cannot move {base} into {cache}: {error}"
                ) from error
        return InstallResult(host_path, directory, downloaded=True)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def _checksum_of(listing: str, file: str) -> str | None:
    """The SHA-256 ``sha256sum`` output lists for ``file``, in lower case."""
    for line in listing.splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](.+)", line)
        if match and match.group(2) == file:
            return match.group(1).lower()
    return None


_CHUNK = 1 << 16
_TIMEOUT_SECONDS = 60


def _check_cancel(cancel: threading.Event | None) -> None:
    if cancel is not None and cancel.is_set():
        raise Alpha2HostInstallError("ABORTED", "the install was aborted")


def _open(url: str, cancel: threading.Event | None):  # type: ignore[no-untyped-def]
    _check_cancel(cancel)
    try:
        return urllib.request.urlopen(url, timeout=_TIMEOUT_SECONDS)
    except urllib.error.HTTPError as error:
        hint = (
            "; the release may not be published yet, or not for this platform"
            if error.code == 404
            else ""
        )
        raise Alpha2HostInstallError(
            "DOWNLOAD_FAILED", f"{url} answered {error.code}{hint}"
        ) from error
    except (urllib.error.URLError, OSError) as error:
        raise Alpha2HostInstallError(
            "DOWNLOAD_FAILED", f"cannot download {url} ({error})"
        ) from error


def _download_text(url: str, cancel: threading.Event | None) -> str:
    with _open(url, cancel) as response:
        try:
            data: bytes = response.read()
        except OSError as error:
            raise Alpha2HostInstallError(
                "DOWNLOAD_FAILED", f"cannot download {url} ({error})"
            ) from error
    return data.decode("utf-8", errors="replace")


def _download_file(
    url: str,
    path: Path,
    on_progress: Callable[[int, int | None], None] | None,
    cancel: threading.Event | None,
) -> str:
    """Writes ``url`` to ``path`` and returns its SHA-256 in lower-case hex."""
    digest = hashlib.sha256()
    with _open(url, cancel) as response, path.open("wb") as file:
        length = response.headers.get("Content-Length")
        total = int(length) if length and length.isdigit() and int(length) > 0 else None
        received = 0
        while True:
            _check_cancel(cancel)
            try:
                chunk = response.read(_CHUNK)
            except OSError as error:
                raise Alpha2HostInstallError(
                    "DOWNLOAD_FAILED", f"cannot download {url} ({error})"
                ) from error
            if not chunk:
                break
            digest.update(chunk)
            file.write(chunk)
            received += len(chunk)
            if on_progress is not None:
                on_progress(received, total)
    return digest.hexdigest()


def _extract(archive: Path, destination: Path) -> None:
    """Unpacks ``archive`` into ``destination``, refusing members that would land outside it."""
    try:
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as bundle:
                root = destination.resolve()
                for name in bundle.namelist():
                    if not (root / name).resolve().is_relative_to(root):
                        raise Alpha2HostInstallError(
                            "EXTRACT_FAILED", f"{archive.name} has a member outside it: {name}"
                        )
                bundle.extractall(destination)
            return
        if hasattr(tarfile, "data_filter"):
            with tarfile.open(archive) as bundle:
                bundle.extractall(destination, filter="data")
            return
        # A Python without extraction filters (before 3.11.4) unpacks with the system's tar.
        completed = subprocess.run(
            ["tar", "-xf", str(archive), "-C", str(destination)],
            capture_output=True,
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            raise Alpha2HostInstallError(
                "EXTRACT_FAILED",
                f"tar exited with {completed.returncode} unpacking the archive:\n"
                f"{completed.stderr.strip()}",
            )
    except (tarfile.TarError, zipfile.BadZipFile, OSError) as error:
        raise Alpha2HostInstallError(
            "EXTRACT_FAILED", f"cannot unpack {archive.name}: {error}"
        ) from error
