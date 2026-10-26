"""Downloads ``alpha2-host`` into the cache shared with every alpha2 client.

The cache layout is the one ``@alpha2-sh/node`` uses: each release archive unpacks into
``<cache>/alpha2-<version>-<target>-<kind>/`` with ``bin/alpha2-host`` beside the ``lib/`` it loads,
so one download serves both clients. Cisco's OpenH264 library, on request, goes into
``<cache>/openh264-<version>-<target>/``, where the host looks for it.
"""

from __future__ import annotations

import bz2
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
    "OPENH264_DOWNLOAD_URL",
    "HostArchiveKind",
    "InstallResult",
    "OpenH264InstallResult",
    "cached_host_binary",
    "host_cache_directory",
    "host_executable_name",
    "host_target",
    "install_host",
    "install_openh264",
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


OPENH264_VERSION = "2.7.0"
"""The OpenH264 release :func:`install_openh264` installs."""

OPENH264_DOWNLOAD_URL = "https://ciscobinary.openh264.org"
"""Where Cisco publishes its OpenH264 binaries: ``<base>/<archive>``."""

OPENH264_NOTICE = "OpenH264 Video Codec provided by Cisco Systems, Inc."
"""The text Cisco's binary licence requires wherever the user controls the codec."""

OPENH264_LICENSE_URL = "https://www.openh264.org/BINARY_LICENSE.txt"
"""Cisco's licence for its OpenH264 binaries."""


@dataclass(frozen=True)
class _OpenH264Archive:
    """One target's OpenH264 binary: the ``.bz2`` file Cisco publishes and the library in it."""

    archive: str
    archive_sha256: str
    library: str
    library_sha256: str


# Cisco's OpenH264 binaries by release target, with their SHA-256s. It mirrors
# release/openh264/manifest.toml, which the published package does not carry.
_OPENH264_TARGETS: dict[str, _OpenH264Archive] = {
    "linux-x64": _OpenH264Archive(
        "libopenh264-2.7.0-linux64.8.so.bz2",
        "44ad0dd2c5af2bac84d26f89f2eb4e268708b8739323d15d33837f7221ce4980",
        "libopenh264-2.7.0-linux64.8.so",
        "1522f873a482923c5ec018a4415e1866898197528828c50f90fd300c94620df3",
    ),
    "linux-arm64": _OpenH264Archive(
        "libopenh264-2.7.0-linux-arm64.8.so.bz2",
        "74c7a1d02003d80daf2b4bc7bbb02f73e5548debd4c158b495e1f83d80ebd136",
        "libopenh264-2.7.0-linux-arm64.8.so",
        "9ac664b0154fd0868491c2a904397b8a05779874c0603b8b1f16ad15de1ffd82",
    ),
    "macos-arm64": _OpenH264Archive(
        "libopenh264-2.7.0-mac-arm64.dylib.bz2",
        "c9f2338359093a3e0628e01717be373c6948994a6070a706d303f052013e2b5f",
        "libopenh264-2.7.0-mac-arm64.dylib",
        "2e1ae3cd10c08328ca03a1f304fd2a1a2a7e5658c6d5a8582101a963f7995ef0",
    ),
    "win-x64": _OpenH264Archive(
        "openh264-2.7.0-win64.dll.bz2",
        "7073fda55e904ac515f787d9728c4bcf9965ec7c91c9f84eae3f260ef9b9b37c",
        "openh264-2.7.0-win64.dll",
        "0ef5e7f3b38753b15c13afa0418d4d7805fa228e6adfd3e425bbf758cff1c288",
    ),
}


@dataclass(frozen=True)
class OpenH264InstallResult:
    """What :func:`install_openh264` installed."""

    library_path: Path
    """The installed library, which ``alpha2-host`` now loads."""
    directory: Path
    """Its directory in the cache, ``openh264-<version>-<target>/``."""
    downloaded: bool
    """Whether it was downloaded now; ``False`` when it was already installed."""
    version: str
    """The OpenH264 release."""
    notice: str
    """:data:`OPENH264_NOTICE`, which Cisco's licence requires be shown to the user."""


def install_openh264(
    *,
    cache_directory: str | os.PathLike[str] | None = None,
    force: bool = False,
    download_url: str | None = None,
    on_progress: Callable[[int, int | None], None] | None = None,
    cancel: threading.Event | None = None,
) -> OpenH264InstallResult:
    """Downloads Cisco's prebuilt OpenH264 library for this platform.

    It is the software H.264 encoder ``alpha2-host`` loads when no hardware encoder is available,
    installed into the cache as ``openh264-<version>-<target>/<library>``, where the host looks for
    it. Nothing else downloads it: Cisco's patent licence covers its binary only when the user
    fetches it from Cisco. It comes from :data:`OPENH264_DOWNLOAD_URL` (or ``download_url``, a
    mirror of it; default ``ALPHA2_OPENH264_DOWNLOAD_URL`` when set), and the ``.bz2`` file and the
    library it unpacks to are both checked against their SHA-256s. A library already installed is
    not downloaded again unless ``force`` is set. It appears in the cache whole or not at all, like
    :func:`install_host`'s archives. ``on_progress(received, total)`` is called as it downloads;
    ``total`` is ``None`` when the server did not say. Setting ``cancel`` stops the install.

    Raises :class:`~alpha2.Alpha2HostInstallError`.
    """
    target = host_target()
    entry = _OPENH264_TARGETS.get(target) if target is not None else None
    if target is None or entry is None:
        system, machine = _platform_key()
        raise Alpha2HostInstallError(
            "UNSUPPORTED_PLATFORM",
            f"there is no OpenH264 binary for alpha2-host on {system}-{machine}",
        )

    cache = Path(cache_directory) if cache_directory is not None else host_cache_directory()
    base = f"openh264-{OPENH264_VERSION}-{target}"
    directory = cache / base
    library_path = directory / entry.library

    def installed() -> bool:
        return _sha256_of_file(library_path) == entry.library_sha256

    def result(downloaded: bool) -> OpenH264InstallResult:
        return OpenH264InstallResult(
            library_path, directory, downloaded, OPENH264_VERSION, OPENH264_NOTICE
        )

    if not force and installed():
        return result(False)

    from_environment = os.environ.get("ALPHA2_OPENH264_DOWNLOAD_URL", "")
    url = download_url or (from_environment if from_environment.strip() else OPENH264_DOWNLOAD_URL)
    url = f"{url.rstrip('/')}/{entry.archive}"

    cache.mkdir(parents=True, exist_ok=True)
    # Staged in the cache itself, so the finished directory moves into place with one rename.
    staging = Path(tempfile.mkdtemp(prefix=".install-", dir=cache))
    try:
        archive_path = staging / entry.archive
        actual = _download_file(
            url, archive_path, on_progress, cancel, f"; {entry.archive} is not there"
        )
        if actual != entry.archive_sha256:
            raise Alpha2HostInstallError(
                "CHECKSUM_MISMATCH",
                f"{entry.archive} has SHA-256 {actual}; "
                f"OpenH264 {OPENH264_VERSION} has {entry.archive_sha256}",
            )
        _check_cancel(cancel)
        try:
            library = bz2.decompress(archive_path.read_bytes())
        except (OSError, ValueError, EOFError) as error:
            raise Alpha2HostInstallError(
                "EXTRACT_FAILED", f"cannot decompress {entry.archive}: {error}"
            ) from error
        library_hash = hashlib.sha256(library).hexdigest()
        if library_hash != entry.library_sha256:
            raise Alpha2HostInstallError(
                "CHECKSUM_MISMATCH",
                f"{entry.library} has SHA-256 {library_hash}; "
                f"OpenH264 {OPENH264_VERSION} has {entry.library_sha256}",
            )
        staged = staging / base
        staged.mkdir()
        (staged / entry.library).write_bytes(library)

        # A directory there holds no valid library (or ``force`` replaces it).
        if force or not installed():
            shutil.rmtree(directory, ignore_errors=True)
        try:
            os.rename(staged, directory)
        except OSError as error:
            # Another install moved the same library into place first.
            if not installed():
                raise Alpha2HostInstallError(
                    "EXTRACT_FAILED", f"cannot move {base} into {cache}: {error}"
                ) from error
        return result(True)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def _sha256_of_file(path: Path) -> str | None:
    """The SHA-256 of the file at ``path`` in lower-case hex, or ``None`` when there is none."""
    try:
        data = path.read_bytes()
    except OSError:
        return None
    return hashlib.sha256(data).hexdigest()


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


_RELEASE_NOT_FOUND = "; the release may not be published yet, or not for this platform"


def _open(  # type: ignore[no-untyped-def]
    url: str, cancel: threading.Event | None, not_found: str = _RELEASE_NOT_FOUND
):
    _check_cancel(cancel)
    try:
        return urllib.request.urlopen(url, timeout=_TIMEOUT_SECONDS)
    except urllib.error.HTTPError as error:
        hint = not_found if error.code == 404 else ""
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
    not_found: str = _RELEASE_NOT_FOUND,
) -> str:
    """Writes ``url`` to ``path`` and returns its SHA-256 in lower-case hex."""
    digest = hashlib.sha256()
    with _open(url, cancel, not_found) as response, path.open("wb") as file:
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
