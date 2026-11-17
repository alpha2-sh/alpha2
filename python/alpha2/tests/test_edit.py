from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

import alpha2

CORPUS = json.loads(
    (Path(__file__).parents[3] / "tests" / "cases" / "surface-conformance.json").read_text("utf-8")
)


@pytest.mark.parametrize("case", CORPUS["edit"], ids=lambda case: case["name"])
def test_each_case_answers_as_rust(case: dict) -> None:
    project = CORPUS["projects"][case["project"]]
    expected = case["expected"]
    if "refusal" in expected:
        with pytest.raises(alpha2.Alpha2EditError) as raised:
            alpha2.edit.apply(project, case["request"])
        assert (raised.value.code, raised.value.refusal) == (expected["code"], expected["refusal"])
        return
    edited = alpha2.edit.apply(project, case["request"])
    assert hashlib.sha256(edited.project_json.encode()).hexdigest() == expected["project_sha256"]
    assert edited.report == expected["report"]
    assert edited.project == json.loads(edited.project_json)


def test_an_invalid_project_is_refused() -> None:
    with pytest.raises(alpha2.Alpha2EditError) as raised:
        alpha2.edit.apply({"schema_version": 1}, {"op": "bake", "targets": []})
    assert raised.value.refusal == "invalid_project"
