"""TextMeasurer: the engine's own placement code, with the fonts an export registers."""

from __future__ import annotations

import json
import threading
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT

import alpha2
from alpha2.layout import box, text

FIXTURE = ROOT / "tests/fonts/Alpha2Fixture.ttf"
PAIR = ROOT / "tests/fonts/Alpha2FixturePair.ttc"


def golden(name: str) -> Any:
    return json.loads((ROOT / "tests/golden" / name).read_text(encoding="utf-8"))


def test_every_pinned_layout_measures_to_the_same_bits_as_natively_and_in_wasm() -> None:
    pinned = golden("layout_measure.json")
    measurer = alpha2.TextMeasurer([ROOT / pinned["font"]])
    for case in pinned["cases"]:
        canvas = tuple(case["canvas"]) if "canvas" in case else None
        measured = measurer.layout(
            case["generator"],
            picture=(case["width"], case["height"]),
            time=alpha2.ticks(case["time"]),
            canvas=canvas,
        )
        # Floats compared exactly: one code path.
        assert measured == json.loads(case["expected"]), case["name"]


def test_every_pinned_text_measures_to_the_same_bits_as_natively_and_in_wasm() -> None:
    pinned = golden("text_measure.json")
    measurer = alpha2.TextMeasurer([ROOT / pinned["font"]])
    for case in pinned["cases"]:
        measured = measurer.text(case["generator"], canvas=(case["width"], case["height"]))
        assert measured == json.loads(case["expected"]), case["name"]


def test_a_layout_root_measures_as_its_generator_and_time_is_in_seconds() -> None:
    measurer = alpha2.TextMeasurer([FIXTURE])
    root = box(text("Alpha2", font_size=0.1), padding=0.05)
    whole = measurer.layout({"type": "layout", "root": root}, picture=(1280, 720))
    assert measurer.layout(root, picture=(1280, 720), time=0.5) == whole
    node = whole["nodes"][1]
    assert node["at"] == "root.children[0]"
    assert node["lineCount"] == 1
    assert node["baselines"] is not None
    assert whole["nodes"][0]["baselines"] is None


def test_one_loose_line_has_its_baseline_where_half_leading_puts_it() -> None:
    # One line at line height 1.4: the em box (0.8 above the baseline, 0.2 below) is centred in a
    # line 1.4 tall, so the baseline is (0.2 + 0.8) font sizes below the layout box's top.
    measurer = alpha2.TextMeasurer([FIXTURE])
    bounds = measurer.text(
        {"type": "text", "text": "Alpha2", "style": {"font_size": 0.1, "line_height": 1.4}},
        canvas=(1000, 1000),
    )
    baselines = bounds["baselines"]
    assert baselines["first"] == baselines["last"]
    assert baselines["first"] - bounds["layout"]["top"] == pytest.approx(
        (0.2 + 0.8) * 0.1, abs=1e-6
    )


def test_every_face_of_a_collection_is_registered_so_a_weight_reaches_the_bold() -> None:
    measurer = alpha2.TextMeasurer([PAIR])

    def ink_right(weight: int) -> float:
        bounds = measurer.text(
            {"type": "text", "text": "HI", "style": {"font_size": 0.25, "font_weight": weight}},
            canvas=(400, 200),
        )
        assert bounds["ink"] is not None
        return bounds["ink"]["right"]

    # The fixture's stems are as wide as the weight: the Bold face's "I" is wider about the same
    # centre.
    assert ink_right(700) > ink_right(400)


def test_a_font_that_cannot_be_registered_is_refused_naming_it(tmp_path: Path) -> None:
    missing = tmp_path / "Missing.ttf"
    with pytest.raises(FileNotFoundError, match=r"Missing\.ttf"):
        alpha2.TextMeasurer([missing])
    junk = tmp_path / "Junk.ttf"
    junk.write_bytes(b"not a font")
    with pytest.raises(ValueError, match=r"Junk\.ttf"):
        alpha2.TextMeasurer([junk])
    with pytest.raises(TypeError):
        alpha2.TextMeasurer(str(FIXTURE))


def test_a_generator_the_engine_refuses_raises_its_message() -> None:
    measurer = alpha2.TextMeasurer([FIXTURE])
    with pytest.raises(ValueError, match="only a text generator"):
        measurer.text({"type": "fill", "paint": "#000000"}, canvas=(100, 100))
    with pytest.raises(ValueError, match="canvas"):
        measurer.text({"type": "text", "text": "a"}, canvas=(0, 100))
    with pytest.raises(ValueError, match="picture"):
        measurer.layout(box(), picture=(1.5, 2))  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="layout generator"):
        measurer.layout(box(width="wide"), picture=(100, 100))


def test_threads_share_one_measurer() -> None:
    measurer = alpha2.TextMeasurer([FIXTURE])
    generator: Any = {"type": "text", "text": "Alpha2 shared"}
    expected = measurer.text(generator, canvas=(1920, 1080))
    results: list[Any] = []

    def measure() -> None:
        for _ in range(20):
            results.append(measurer.text(generator, canvas=(1920, 1080)))

    threads = [threading.Thread(target=measure) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert results == [expected] * 80
