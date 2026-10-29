"""The scenes the authoring helpers were made for, built with them alone, exported and checked on
one still each: a chat bubble, a count-up and a logo.

The host is ``ALPHA2_HOST_BINARY``, else ``target/debug/alpha2-host``; without one the module is
skipped, as ``test_e2e.py`` is. If a scene needs a raw dict, the helper is what to fix.
"""

from __future__ import annotations

import os
import sys
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT, read_rgba_png

import alpha2
from alpha2 import HostClient, key, keyframes, visual
from alpha2.layout import box, layout, text
from alpha2.shapes import circle, fill, shapes

HOST = Path(
    os.environ.get("ALPHA2_HOST_BINARY")
    or ROOT / "target" / "debug" / ("alpha2-host.exe" if sys.platform == "win32" else "alpha2-host")
)
if not HOST.is_file():
    pytest.skip(
        f"no alpha2-host at {HOST}; build it or set ALPHA2_HOST_BINARY", allow_module_level=True
    )

FIXTURE = ROOT / "tests/fonts/Alpha2Fixture.ttf"
PAIR = ROOT / "tests/fonts/Alpha2FixturePair.ttc"
FRAME = alpha2.TICKS_PER_SECOND // 30
"""Ticks per frame at 30 fps."""


@pytest.fixture(scope="module")
def client() -> HostClient:
    return HostClient(host_path=HOST)


def one_clip(generator: Any, *, width: int, height: int, duration: float = 4, **clip: Any) -> Any:
    project = alpha2.ProjectBuilder("Scene", width=width, height=height, frame_rate=30)
    project.video_track().clip(project.generated(generator), at=0, duration=duration, **clip)
    return project


def export(client: HostClient, project: Any, output: Path, fonts: list[Path]) -> dict[str, Any]:
    result = client.export(
        project,
        output,
        fonts=fonts,
        video_codec="mpeg4-part2",
        include_audio=False,
        overwrite=True,
    )
    assert output.stat().st_size > 0
    return result  # type: ignore[return-value]


def rows_where(
    path: Path, matches: Callable[[int, int, int, int], bool]
) -> tuple[int, int, int, list[int]]:
    """The still's size and the rows holding a pixel that ``matches`` (r, g, b, a)."""
    width, height, pixels = read_rgba_png(path)
    rows = [
        row
        for row in range(height)
        if any(
            matches(*pixels[(row * width + column) * 4 : (row * width + column) * 4 + 4])
            for column in range(width)
        )
    ]
    return width, height, len(pixels), rows


def test_the_chat_bubble_centres_its_text_without_padding_compensation(
    client: HostClient, tmp_path: Path
) -> None:
    width, height, font_px = 640, 360, 40
    bubble = box(
        text("Hello", font_size=f"{font_px}px", line_height=1.4, color="#ffffff"),
        padding="12px 20px",
        background="#3060c0",
        border_radius="16px",
    )
    generator = layout(box(bubble, justify_content="center", align_items="center"))
    project = one_clip(generator, width=width, height=height).build()
    assert alpha2.validate_project(project) == []
    export(client, project, tmp_path / "chat.mp4", [FIXTURE])

    nodes = alpha2.TextMeasurer([FIXTURE]).layout(generator, picture=(width, height))["nodes"]
    border = nodes[1]["border"]
    box_top, box_bottom = border["top"] * height, border["bottom"] * height

    still = tmp_path / "chat.png"
    client.still(project, 1, still, fonts=[FIXTURE])
    # The bubble is blue, the text white on it.
    _, _, _, blue = rows_where(still, lambda r, g, b, a: b > 150 and r < 100)
    _, _, _, ink = rows_where(still, lambda r, g, b, a: r > 150 and g > 150)
    assert abs(blue[0] - box_top) <= 1 and abs(blue[-1] + 1 - box_bottom) <= 1
    top_gap = ink[0] - box_top
    bottom_gap = box_bottom - (ink[-1] + 1)
    # The line's leading is split above and below its em box (CSS half-leading), so the gaps
    # differ only by where the glyphs sit in the em box: no more than its descender, 0.2 em. Were
    # all the leading below the text, the bottom gap would be 0.4 em larger.
    assert abs(top_gap - bottom_gap) <= 0.2 * font_px, (top_gap, bottom_gap)


def test_the_count_up_is_one_layout_text_whose_frames_equal_static_texts(
    client: HostClient, tmp_path: Path
) -> None:
    width, height = 320, 180

    def counter(content: Any) -> Any:
        return layout(
            box(
                text(content, font_size=0.3, color="#ffffff"),
                justify_content="center",
                align_items="center",
            )
        )

    count = keyframes(*(key(alpha2.ticks(value * FRAME), str(value)) for value in range(101)))
    project = one_clip(counter(count), width=width, height=height).build()
    assert alpha2.validate_project(project) == []
    export(client, project, tmp_path / "count.mp4", [FIXTURE])

    stills: list[bytes] = []
    times: list[tuple[int, alpha2.builder.Time]] = [
        (0, 0),
        (45, alpha2.ticks(45 * FRAME)),
        (100, 3.5),
    ]
    for value, at in times:
        animated, static = tmp_path / f"count-{value}.png", tmp_path / f"static-{value}.png"
        client.still(project, at, animated, fonts=[FIXTURE])
        fixed = one_clip(counter(str(value)), width=width, height=height).build()
        client.still(fixed, at, static, fonts=[FIXTURE])
        pixels = read_rgba_png(animated)[2]
        assert pixels == read_rgba_png(static)[2], value
        stills.append(pixels)
    assert len(set(stills)) == 3


def test_the_logo_mark_sits_on_the_letters_baseline(client: HostClient, tmp_path: Path) -> None:
    width, height = 640, 360
    letters = layout(
        box(
            text("HIH", font_size=0.3, letter_spacing=0.1, font_weight=700, color="#ffffff"),
            justify_content="center",
            align_items="center",
        )
    )
    measurer = alpha2.TextMeasurer([PAIR])
    word = measurer.layout(letters, picture=(width, height))["nodes"][1]
    assert word["baselines"] is not None
    baseline = word["baselines"]["first"]
    # A mark right of the word, its bottom on the baseline.
    diameter = 0.08
    radius_y = diameter * min(width, height) / height / 2
    mark = shapes(
        circle(
            (word["border"]["right"] + 0.05, baseline - radius_y), diameter, picture=(width, height)
        ),
        fill("#ffb020"),
    )
    project = alpha2.ProjectBuilder("Logo", width=width, height=height, frame_rate=30)
    project.video_track().clip(project.generated(letters), at=0, duration=2)
    project.video_track().clip(project.generated(mark), at=0, duration=2)
    document = project.build()
    export(client, document, tmp_path / "logo.mp4", [PAIR])

    still = tmp_path / "logo.png"
    client.still(document, 1, still, fonts=[PAIR])
    _, _, _, orange = rows_where(still, lambda r, g, b, a: r > 150 and b < 100)
    assert abs((orange[-1] + 1) - baseline * height) <= 1, (orange[-1], baseline * height)
    # The weight reached the collection's Bold face: its stems cover more than the Regular's.
    regular = alpha2.ProjectBuilder("Regular", width=width, height=height, frame_rate=30)
    regular.video_track().clip(
        regular.generated(
            layout(
                box(
                    text("HIH", font_size=0.3, letter_spacing=0.1, color="#ffffff"),
                    justify_content="center",
                    align_items="center",
                )
            )
        ),
        at=0,
        duration=2,
    )
    thin = tmp_path / "regular.png"
    client.still(regular.build(), 1, thin, fonts=[PAIR])

    def white(path: Path) -> int:
        pixels = read_rgba_png(path)[2]
        return sum(1 for index in range(0, len(pixels), 4) if min(pixels[index : index + 3]) > 200)

    assert white(still) > white(thin)


def test_a_circle_is_square_in_the_still(client: HostClient, tmp_path: Path) -> None:
    dot = shapes(circle((0.5, 0.5), 0.5, picture=(1920, 1080)), fill("#ffffff"))
    assert dot["items"][0]["size"] == {"mode": "constant", "value": {"x": 0.28125, "y": 0.5}}  # type: ignore[typeddict-item]
    project = one_clip(dot, width=1920, height=1080, duration=1).build()
    still = tmp_path / "dot.png"
    client.still(project, 0, still, size=(384, 216))
    width, _, pixels = read_rgba_png(still)
    covered = [
        (index // 4 % width, index // 4 // width)
        for index in range(0, len(pixels), 4)
        if pixels[index] >= 128
    ]
    columns = {column for column, _ in covered}
    rows = {row for _, row in covered}
    assert abs((max(columns) - min(columns)) - (max(rows) - min(rows))) <= 1


def test_a_fading_clip_built_with_visual_exports(client: HostClient, tmp_path: Path) -> None:
    project = one_clip(
        {"type": "fill", "paint": "#ff0000"},
        width=64,
        height=48,
        duration=1,
        visual=visual(opacity=keyframes(key(0, 0.0), key(1, 1.0))),
    ).build()
    result = export(client, project, tmp_path / "fade.mp4", [])
    assert result["frames"] == 30


def test_a_count_up_in_one_clip_against_sixty_text_clips(
    client: HostClient, tmp_path: Path
) -> None:
    # Recorded, not gated: run with -s to see the times.
    width, height = 640, 360
    count = keyframes(*(key(alpha2.ticks(value * FRAME), str(value)) for value in range(60)))
    one = one_clip(
        layout(box(text(count, font_size=0.2), justify_content="center")),
        width=width,
        height=height,
        duration=2,
    ).build()
    many = alpha2.ProjectBuilder("Many", width=width, height=height, frame_rate=30)
    track = many.video_track()
    for value in range(60):
        number = many.generated({"type": "text", "text": str(value), "style": {"font_size": 0.2}})
        track.clip(number, at=alpha2.ticks(value * FRAME), duration=alpha2.ticks(FRAME))
    started = time.perf_counter()
    export(client, one, tmp_path / "one.mp4", [FIXTURE])
    one_seconds = time.perf_counter() - started
    started = time.perf_counter()
    export(client, many.build(), tmp_path / "many.mp4", [FIXTURE])
    many_seconds = time.perf_counter() - started
    print(
        f"count-up export: one layout clip {one_seconds:.2f} s, 60 text clips {many_seconds:.2f} s"
    )
