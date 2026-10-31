"""Downloads ``alpha2-host`` into the cache shared with every alpha2 client.

The cache layout is the one ``@alpha2-sh/node`` uses: each release archive unpacks into
``<cache>/alpha2-<version>-<target>-bundled/`` with ``bin/alpha2-host`` beside the ``lib/`` it
loads, so one download serves both clients. Cisco's OpenH264 library, on request, goes into
``<cache>/openh264-<version>-<target>/``, where the host looks for it.
"""

from __future__ import annotations

import bz2
import functools
import glob
import hashlib
import os
import platform
import re
import secrets
import shutil
import ssl
import subprocess
import sys
import sysconfig
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request
import warnings
import zipfile
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from . import _native
from .errors import Alpha2HostInstallError

__all__ = [
    "HOST_DOWNLOAD_URL",
    "LINUX_GLIBC_FLOOR",
    "OPENH264_DOWNLOAD_URL",
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

HOST_DOWNLOAD_URL = "https://github.com/alpha2-sh/alpha2/releases/download"
"""Where release archives are downloaded from: ``<base>/v<version>/<archive>``."""

LINUX_GLIBC_FLOOR = "2.35"
"""The oldest glibc the Linux archives run on."""

# Spellings of one machine across operating systems.
_MACHINES = {"amd64": "x86_64", "x64": "x86_64", "arm64": "aarch64"}


@functools.cache
def _linux_libc() -> tuple[str, str | None]:
    """This Linux system's C library: ``("glibc", version)``, ``("musl", None)`` or
    ``("unknown", None)``."""
    if sys.platform == "win32":
        return "unknown", None
    try:
        reported = os.confstr("CS_GNU_LIBC_VERSION")
    except (ValueError, OSError):
        reported = None
    if reported and reported.startswith("glibc "):
        return "glibc", reported.split()[1]
    return ("musl" if glob.glob("/lib/ld-musl-*.so.1") else "unknown"), None


@functools.cache
def _apple_silicon() -> bool:
    """Whether this Mac has an Apple silicon CPU, which an x86_64 Python under Rosetta does not
    report."""
    try:
        completed = subprocess.run(
            ["/usr/sbin/sysctl", "-n", "hw.optional.arm64"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return False
    return completed.stdout.strip() == "1"


def _this_machine() -> tuple[str, str]:
    """``(sys.platform, machine)`` of this interpreter.

    On Windows the machine is the interpreter's own architecture, as ``sysconfig`` and pip see it:
    ``platform.machine()`` reports the CPU there (since 3.12), so an x64 Python on Windows on ARM
    would otherwise ask for an ARM64 host it cannot load.
    """
    if sys.platform == "win32":
        name = sysconfig.get_platform()
        return sys.platform, {"win-amd64": "amd64", "win-arm64": "arm64"}.get(name, name)
    return sys.platform, platform.machine()


def _target_for(system: str, machine: str) -> tuple[str | None, str]:
    """The release target for ``system`` and ``machine``, or ``None`` and why there is none."""
    machine = machine.lower()
    unpublished = f"no alpha2-host archive is published for {system}-{machine}"
    if system == "darwin":
        if machine in ("arm64", "aarch64"):
            return "macos-arm64", ""
        if machine in ("x86_64", "amd64"):
            # An x86_64 Python under Rosetta runs the arm64 host as its own, native process.
            if _apple_silicon():
                return "macos-arm64", ""
            return None, "Intel Macs are not supported"
        return None, unpublished
    if system == "win32":
        if machine in ("amd64", "x86_64", "x64"):
            return "win-x64", ""
        if machine in ("arm64", "aarch64"):
            return None, (
                "there is no Windows on ARM alpha2-host; an x64 Node.js/Python runs the x64 host "
                "under emulation (not supported or tested)"
            )
        return None, unpublished
    if system.startswith("linux"):
        machine = _MACHINES.get(machine, machine)
        if machine == "x86_64":
            return "linux-x64", ""
        if machine == "aarch64":
            return "linux-arm64", ""
    return None, unpublished


def host_executable_name() -> str:
    """The file name ``alpha2-host`` has here: Windows executables carry ``.exe``."""
    return "alpha2-host.exe" if sys.platform == "win32" else "alpha2-host"


def host_target(system: str | None = None, machine: str | None = None) -> str | None:
    """The release target that carries ``alpha2-host``, such as ``macos-arm64``.

    For this interpreter, or for ``system`` (as ``sys.platform`` names it) and ``machine`` (as
    ``platform.machine()`` does). ``None`` when no archive is published for them. An x86_64 Python
    on an Apple silicon Mac gets ``macos-arm64``; on Windows the interpreter's architecture
    decides.
    """
    this_system, this_machine = _this_machine()
    target, _ = _target_for(
        system if system is not None else this_system,
        machine if machine is not None else this_machine,
    )
    return target


def _version_tuple(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in re.findall(r"\d+", version))


def _install_target(version: str | None) -> str:
    """The release target to install for, after refusing a platform without an archive and a Linux
    C library other than glibc :data:`LINUX_GLIBC_FLOOR` or later."""
    system, machine = _this_machine()
    target, refusal = _target_for(system, machine)
    if target is None:
        raise Alpha2HostInstallError("UNSUPPORTED_PLATFORM", refusal)
    if system.startswith("linux"):
        kind, libc_version = _linux_libc()
        if kind != "glibc" or libc_version is None:
            library = (
                "is musl (Alpine and other musl distributions are not supported)"
                if kind == "musl"
                else "is not glibc"
            )
            raise Alpha2HostInstallError(
                "UNSUPPORTED_PLATFORM",
                f"alpha2-host is built for glibc {LINUX_GLIBC_FLOOR} or later; "
                f"this system's C library {library}",
            )
        if _version_tuple(libc_version) < _version_tuple(LINUX_GLIBC_FLOOR):
            host = f"alpha2-host {version}" if version else "alpha2-host"
            raise Alpha2HostInstallError(
                "UNSUPPORTED_PLATFORM",
                f"{host} needs glibc {LINUX_GLIBC_FLOOR} or later; this system has glibc "
                f"{libc_version}. Build it from source (cargo install alpha2-host with FFmpeg 8 "
                "development files) and set ALPHA2_HOST_PATH",
            )
    return target


def _is_store_python() -> bool:
    """Whether this is the Microsoft Store's Python, whose writes under ``%LOCALAPPDATA%`` Windows
    redirects into the app's private storage."""
    return "WindowsApps" in Path(sys.prefix).parts or (
        "PythonSoftwareFoundation.Python." in sys.prefix
    )


_STORE_PYTHON_WARNING = (
    "this is the Microsoft Store Python: Windows keeps what it writes under %LOCALAPPDATA% "
    "private to it, so other alpha2 clients will not see this install; set "
    "ALPHA2_HOST_CACHE_DIR to a folder outside %LOCALAPPDATA% to share it"
)


def _warn_if_private(cache: Path) -> None:
    """Warns when the Store Python would install into a cache only it can see."""
    local = os.environ.get("LOCALAPPDATA", "")
    if not local.strip() or not _is_store_python():
        return
    if os.path.normcase(os.path.abspath(cache)).startswith(
        os.path.normcase(os.path.abspath(local)) + os.sep
    ):
        warnings.warn(RuntimeWarning(_STORE_PYTHON_WARNING), stacklevel=3)


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


def _archive_base(version: str, target: str) -> str:
    return f"alpha2-{version}-{target}-bundled"


def cached_host_binary(cache_directory: str | os.PathLike[str]) -> Path | None:
    """The installed ``alpha2-host`` of this package's version in ``cache_directory``, or ``None``
    when it is not there."""
    version = package_version()
    target = host_target()
    if version is None or target is None:
        return None
    candidate = (
        Path(cache_directory) / _archive_base(version, target) / "bin" / host_executable_name()
    )
    return candidate if candidate.is_file() else None


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
    other's partial work, and ``force`` over a copy that is in use leaves that copy as it was.
    Downloads honour ``HTTPS_PROXY``/``HTTP_PROXY``/``NO_PROXY`` and verify with the operating
    system's certificate store (``certifi`` on a Linux system without one). Linux systems need
    glibc :data:`LINUX_GLIBC_FLOOR` or later; musl systems are refused before anything is
    downloaded. ``on_progress(received, total)`` is called as it downloads; ``total`` is ``None``
    when the server did not say. Setting ``cancel`` stops the install.

    Raises :class:`~alpha2.Alpha2HostInstallError`.
    """
    version = package_version()
    if version is None:
        raise Alpha2HostInstallError("UNSUPPORTED_PLATFORM", "cannot read this package's version")
    target = _install_target(version)

    cache = Path(cache_directory) if cache_directory is not None else host_cache_directory()
    _warn_if_private(cache)
    base = _archive_base(version, target)
    directory = cache / base
    host_path = directory / "bin" / host_executable_name()
    if not force and host_path.is_file():
        return InstallResult(host_path, directory, downloaded=False)

    archive = f"{base}.{'zip' if target == 'win-x64' else 'tar.gz'}"
    from_environment = os.environ.get("ALPHA2_HOST_DOWNLOAD_URL", "")
    url = download_url or (from_environment if from_environment.strip() else HOST_DOWNLOAD_URL)
    release_url = f"{url.rstrip('/')}/v{version}"

    cache.mkdir(parents=True, exist_ok=True)
    _remove_stale(cache, base)
    # Staged in the cache itself, so the finished directory moves into place with one rename.
    staging = Path(tempfile.mkdtemp(prefix=".install-", dir=cache))
    try:
        listing = _download_text(f"{release_url}/checksums.sha256", "host", cancel)
        expected = _checksum_of(listing, archive)
        if expected is None:
            raise Alpha2HostInstallError(
                "DOWNLOAD_FAILED", f"{release_url}/checksums.sha256 lists no {archive}"
            )
        archive_path = staging / archive
        actual = _download_file(
            f"{release_url}/{archive}", archive_path, "host", on_progress, cancel
        )
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
        downloaded = _replace_directory(
            staged, directory, force=force, is_installed=host_path.is_file
        )
        return InstallResult(host_path, directory, downloaded=downloaded)
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
    target = _install_target(package_version())
    entry = _OPENH264_TARGETS.get(target)
    if entry is None:
        raise Alpha2HostInstallError(
            "UNSUPPORTED_PLATFORM", f"there is no OpenH264 binary for alpha2-host on {target}"
        )

    cache = Path(cache_directory) if cache_directory is not None else host_cache_directory()
    _warn_if_private(cache)
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
    _remove_stale(cache, base)
    # Staged in the cache itself, so the finished directory moves into place with one rename.
    staging = Path(tempfile.mkdtemp(prefix=".install-", dir=cache))
    try:
        archive_path = staging / entry.archive
        actual = _download_file(
            url, archive_path, "openh264", on_progress, cancel, f"; {entry.archive} is not there"
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

        return result(_replace_directory(staged, directory, force=force, is_installed=installed))
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


_DAY_SECONDS = 24 * 60 * 60


def _remove_stale(cache: Path, base: str) -> None:
    """Removes what earlier installs into ``cache`` left behind more than a day ago.

    Staging directories, and copies of ``base`` moved aside by a replacement that could not delete
    them (a host from them was still running). Anything younger may belong to an install still
    running.
    """
    try:
        names = os.listdir(cache)
    except OSError:
        return
    now = time.time()
    for name in names:
        if not name.startswith((".install-", f"{base}.old-")):
            continue
        path = cache / name
        try:
            if now - path.stat().st_mtime > _DAY_SECONDS:
                shutil.rmtree(path)
        except OSError:
            # Gone already, or still in use: the next install tries again.
            pass


# Delays between rename attempts on Windows, the last repeated until ten seconds have passed.
_RENAME_DELAYS = (0.05, 0.1, 0.2, 0.4, 0.8, 1.0)
_RENAME_BUDGET = 10.0


def _retries_renames() -> bool:
    return sys.platform == "win32"


def _rename(source: Path, destination: Path) -> None:
    os.rename(source, destination)


def _rename_with_retry(source: Path, destination: Path) -> None:
    """Renames ``source`` to ``destination``.

    On Windows a scanner or indexer that has just opened a fresh file makes the rename fail with
    access denied or sharing violation for a moment, so those are retried for up to ten seconds;
    elsewhere it is one attempt.
    """
    waited = 0.0
    attempt = 0
    while True:
        try:
            _rename(source, destination)
            return
        except PermissionError as error:
            delay = _RENAME_DELAYS[min(attempt, len(_RENAME_DELAYS) - 1)]
            transient = getattr(error, "winerror", None) in (None, 5, 32)
            if not _retries_renames() or not transient or waited + delay > _RENAME_BUDGET:
                raise
            time.sleep(delay)
            waited += delay
            attempt += 1


def _replace_directory(
    staged: Path, directory: Path, *, force: bool, is_installed: Callable[[], bool]
) -> bool:
    """Moves the finished ``staged`` directory to ``directory``; returns whether it did.

    ``False`` when a concurrent install got there first. A copy already at ``directory`` is
    replaced when ``force`` is set or it is not a complete install: it is renamed aside first, so
    a copy in use fails the replacement and stays whole, and is deleted only once the new one is
    in place.
    """
    aside: Path | None = None
    if directory.exists() and (force or not is_installed()):
        aside = directory.with_name(f"{directory.name}.old-{secrets.token_hex(4)}")
        try:
            _rename_with_retry(directory, aside)
        except OSError as error:
            raise Alpha2HostInstallError(
                "EXTRACT_FAILED",
                f"cannot replace {directory}: it is in use (is an alpha2-host from it running?); "
                "the installed copy is unchanged",
            ) from error
    try:
        _rename_with_retry(staged, directory)
    except OSError as error:
        if not force and is_installed():
            return False
        if aside is not None:
            try:
                os.rename(aside, directory)
            except OSError:
                pass
        raise Alpha2HostInstallError(
            "EXTRACT_FAILED", f"cannot move {directory.name} into {directory.parent}: {error}"
        ) from error
    if aside is not None:
        shutil.rmtree(aside, ignore_errors=True)
    return True


_CHUNK = 1 << 16
_TIMEOUT_SECONDS = 60


def _check_cancel(cancel: threading.Event | None) -> None:
    if cancel is not None and cancel.is_set():
        raise Alpha2HostInstallError("ABORTED", "the install was aborted")


_RELEASE_NOT_FOUND = "; the release may not be published yet, or not for this platform"


@functools.cache
def _ssl_context() -> ssl.SSLContext:
    """The context downloads verify with.

    macOS and Windows: the operating system's verifier (``truststore``), which knows the roots an
    administrator or a TLS-inspecting proxy added. Linux: OpenSSL's default store when the system
    has one (or ``SSL_CERT_FILE``/``SSL_CERT_DIR`` names one), else ``certifi``'s bundle, as in a
    container image without ca-certificates.
    """
    if sys.platform in ("darwin", "win32"):
        try:
            import truststore
        except ImportError:
            return ssl.create_default_context()
        return truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    paths = ssl.get_default_verify_paths()
    has_store = bool(paths.cafile) or bool(paths.capath and _non_empty_directory(paths.capath))
    if has_store or os.environ.get("SSL_CERT_FILE") or os.environ.get("SSL_CERT_DIR"):
        return ssl.create_default_context()
    try:
        import certifi
    except ImportError:
        return ssl.create_default_context()
    return ssl.create_default_context(cafile=certifi.where())


def _non_empty_directory(path: str) -> bool:
    try:
        return any(os.scandir(path))
    except OSError:
        return False


def _remedy(kind: str) -> str:
    """What a failed download suggests, for the host archive (``host``) or OpenH264."""
    base, variable = (
        (HOST_DOWNLOAD_URL, "ALPHA2_HOST_DOWNLOAD_URL")
        if kind == "host"
        else (OPENH264_DOWNLOAD_URL, "ALPHA2_OPENH264_DOWNLOAD_URL")
    )
    return (
        "Behind a proxy set HTTPS_PROXY (and NO_PROXY); a proxy that inspects TLS needs its root "
        "certificate in the system store or in SSL_CERT_FILE; or download from a mirror laid out "
        f"like {base} with {variable}"
    )


def _failure(url: str, kind: str, error: BaseException) -> Alpha2HostInstallError:
    reason = error.reason if isinstance(error, urllib.error.URLError) else error
    return Alpha2HostInstallError(
        "DOWNLOAD_FAILED", f"cannot download {url}: {reason}\n{_remedy(kind)}"
    )


def _open(  # type: ignore[no-untyped-def]
    url: str, kind: str, cancel: threading.Event | None, not_found: str = _RELEASE_NOT_FOUND
):
    _check_cancel(cancel)
    try:
        return urllib.request.urlopen(url, timeout=_TIMEOUT_SECONDS, context=_ssl_context())
    except urllib.error.HTTPError as error:
        hint = not_found if error.code == 404 else f"\n{_remedy(kind)}"
        raise Alpha2HostInstallError(
            "DOWNLOAD_FAILED", f"{url} answered {error.code}{hint}"
        ) from error
    except (urllib.error.URLError, OSError) as error:
        raise _failure(url, kind, error) from error


def _download_text(url: str, kind: str, cancel: threading.Event | None) -> str:
    with _open(url, kind, cancel) as response:
        try:
            data: bytes = response.read()
        except OSError as error:
            raise _failure(url, kind, error) from error
    return data.decode("utf-8", errors="replace")


def _download_file(
    url: str,
    path: Path,
    kind: str,
    on_progress: Callable[[int, int | None], None] | None,
    cancel: threading.Event | None,
    not_found: str = _RELEASE_NOT_FOUND,
) -> str:
    """Writes ``url`` to ``path`` and returns its SHA-256 in lower-case hex."""
    digest = hashlib.sha256()
    with _open(url, kind, cancel, not_found) as response, path.open("wb") as file:
        length = response.headers.get("Content-Length")
        total = int(length) if length and length.isdigit() and int(length) > 0 else None
        received = 0
        while True:
            _check_cancel(cancel)
            try:
                chunk = response.read(_CHUNK)
            except OSError as error:
                raise _failure(url, kind, error) from error
            if not chunk:
                break
            digest.update(chunk)
            file.write(chunk)
            received += len(chunk)
            if on_progress is not None:
                on_progress(received, total)
    return digest.hexdigest()


def _extract(archive: Path, destination: Path) -> None:
    """Unpacks ``archive`` into ``destination``, refusing members that would land outside it.

    Tar members go through the ``data`` filter, which also leaves owners, groups and special bits
    as the extracting user's.
    """
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
        with tarfile.open(archive) as bundle:
            bundle.extractall(destination, filter="data")
    except (tarfile.TarError, zipfile.BadZipFile, OSError) as error:
        raise Alpha2HostInstallError(
            "EXTRACT_FAILED", f"cannot unpack {archive.name}: {error}"
        ) from error
