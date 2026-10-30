"""The test capability contract's Python helper: ALPHA2_REQUIRE and the skip log."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import CAPABILITIES, ROOT, require_or_skip, required_capabilities


def test_alpha2_require_is_a_trimmed_comma_list_of_known_names() -> None:
    assert required_capabilities({"ALPHA2_REQUIRE": " host , ,h264,"}) == {"host", "h264"}
    assert required_capabilities({}) == set()
    for name in CAPABILITIES:
        assert name == name.lower() and all(c.isalnum() or c == "-" for c in name)


def test_an_unknown_name_fails() -> None:
    with pytest.raises(pytest.fail.Exception, match="unknown capabilities \\['hots'\\]"):
        required_capabilities({"ALPHA2_REQUIRE": "host,hots"})
    with pytest.raises(pytest.fail.Exception, match="unknown capability 'hots'"):
        require_or_skip("hots", "a typo")


def test_a_required_capability_fails_instead_of_skipping(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ALPHA2_REQUIRE", "h264")
    with pytest.raises(pytest.fail.Exception, match=r"^h264 is required \(ALPHA2_REQUIRE\): none$"):
        require_or_skip("h264", "none")


def test_a_skip_appends_one_record(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    log = tmp_path / "skips.jsonl"
    monkeypatch.setenv("ALPHA2_REQUIRE", "")
    monkeypatch.setenv("ALPHA2_SKIP_LOG", str(log))
    with pytest.raises(pytest.skip.Exception, match="no encoder"):
        require_or_skip("h264", "no encoder")
    (line,) = log.read_text(encoding="utf-8").splitlines()
    record = json.loads(line)
    assert record["lang"] == "python"
    assert record["suite"] == Path(__file__).resolve().relative_to(ROOT).as_posix()
    assert record["test"].endswith("test_a_skip_appends_one_record")
    assert record["capability"] == "h264"
    assert record["reason"] == "no encoder"


def test_a_relative_skip_log_fails(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ALPHA2_REQUIRE", "")
    monkeypatch.setenv("ALPHA2_SKIP_LOG", "skips.jsonl")
    with pytest.raises(pytest.fail.Exception, match="must be absolute"):
        require_or_skip("h264", "no encoder")
