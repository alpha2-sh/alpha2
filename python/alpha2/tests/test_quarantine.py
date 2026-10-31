"""macOS quarantine: the refusal of a host Gatekeeper would hold on an approval prompt."""

from __future__ import annotations

import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path

import pytest
from conftest import require_or_skip

from alpha2 import Alpha2HostError, HostClient
from alpha2._quarantine import quarantine_refusal


def archive_tree(root: Path, libraries: list[str]) -> Path:
    """An unpacked archive's layout: ``bin/alpha2-host`` and ``lib/`` with ``libraries``."""
    (root / "bin").mkdir(parents=True)
    (root / "lib").mkdir()
    (root / "bin" / "alpha2-host").write_bytes(b"")
    for name in libraries:
        (root / "lib" / name).write_bytes(b"")
    return root / "bin" / "alpha2-host"


class Probe:
    """Says ``quarantined`` and ``accepted`` of every file, recording what it assessed."""

    def __init__(self, quarantined: bool, accepted: bool | None) -> None:
        self._quarantined = quarantined
        self._accepted = accepted
        self.assessed: list[tuple[Path, str]] = []

    def quarantined(self, path: Path) -> bool:
        return self._quarantined

    def accepted(self, path: Path, kind: str) -> bool | None:
        self.assessed.append((path, kind))
        return self._accepted


def test_quarantine_refusal_lists_quarantined_files_that_gatekeeper_rejects(
    tmp_path: Path,
) -> None:
    binary = archive_tree(
        tmp_path,
        [
            "libavcodec.62.dylib",
            "libavformat.62.dylib",
            "libavutil.60.dylib",
            "libswscale.9.dylib",
            "notes.txt",
        ],
    )
    probe = Probe(quarantined=True, accepted=False)
    message = quarantine_refusal(binary, probe)
    assert message is not None
    assert message.startswith("macOS quarantined 5 file(s) alpha2-host needs")
    assert "and 2 more." in message
    assert str(binary) in message
    assert "notes.txt" not in message
    assert [kind for _, kind in probe.assessed] == ["execute"] + ["library"] * 4
    assert "python -m alpha2 install" in message


def test_a_quarantined_file_gatekeeper_accepts_is_not_refused(tmp_path: Path) -> None:
    binary = archive_tree(tmp_path, ["libavcodec.62.dylib"])
    assert quarantine_refusal(binary, Probe(quarantined=True, accepted=True)) is None
    # Files that are not quarantined are never assessed.
    probe = Probe(quarantined=False, accepted=False)
    assert quarantine_refusal(binary, probe) is None
    assert probe.assessed == []


def test_an_unknown_assessment_does_not_refuse(tmp_path: Path) -> None:
    binary = archive_tree(tmp_path, ["libavcodec.62.dylib"])
    assert quarantine_refusal(binary, Probe(quarantined=True, accepted=None)) is None


def test_the_remedy_names_the_archive_root(tmp_path: Path) -> None:
    binary = archive_tree(tmp_path / "archive", ["libavcodec.62.dylib"])
    rejected = Probe(quarantined=True, accepted=False)
    message = quarantine_refusal(binary, rejected)
    assert message is not None
    assert f"xattr -dr com.apple.quarantine '{tmp_path / 'archive'}'" in message
    # A host outside an archive's bin/ names its own directory.
    alone = tmp_path / "alone" / "alpha2-host"
    alone.parent.mkdir()
    alone.write_bytes(b"")
    message = quarantine_refusal(alone, rejected)
    assert message is not None
    assert f"xattr -dr com.apple.quarantine '{alone.parent}'" in message


@pytest.mark.skipif(sys.platform != "darwin", reason="macOS only: quarantine and Gatekeeper")
def test_a_quarantined_ad_hoc_host_is_refused_before_it_starts(tmp_path: Path) -> None:
    if shutil.which("cc") is None:
        require_or_skip("c-compiler", "no cc to build an unsigned host")
    binary = archive_tree(tmp_path / "archive", [])
    source = tmp_path / "main.c"
    source.write_text("int main(void) { return 0; }\n", encoding="utf-8")
    subprocess.run(["cc", "-o", str(binary), str(source)], check=True)
    value = f"0083;{int(time.time()):x};Safari;{str(uuid.uuid4()).upper()}"
    subprocess.run(["/usr/bin/xattr", "-w", "com.apple.quarantine", value, str(binary)], check=True)
    started = time.monotonic()
    with pytest.raises(Alpha2HostError) as caught:
        HostClient(host_path=binary).capabilities()
    assert time.monotonic() - started < 5
    assert caught.value.code == "HOST_QUARANTINED", caught.value.message
    assert f"xattr -dr com.apple.quarantine '{tmp_path / 'archive'}'" in caught.value.message
    assert caught.value.failure["code"] == "INVALID_INPUT"
    log = subprocess.run(
        [
            "/usr/bin/log",
            "show",
            "--last",
            "1m",
            "--predicate",
            'process == "syspolicyd"',
            "--style",
            "compact",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    prompts = [
        line for line in log.stdout.splitlines() if "Prompt shown" in line and str(binary) in line
    ]
    assert prompts == []
