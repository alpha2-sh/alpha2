"""alpha2.layout and alpha2.shapes write the documents the engine reads, as the TS builders do."""

from __future__ import annotations

from typing import Any

import pytest
from conftest import corpus

import alpha2
from alpha2 import key, keyframes
from alpha2.layout import box, layout, shape, text
from alpha2.shapes import (
    circle,
    ellipse,
    fill,
    group,
    path,
    polystar,
    rect,
    shapes,
    stroke,
    subpath,
    transform,
    trim,
    vertex,
)


def project_of(generator: Any, *, width: int = 1920, height: int = 1080) -> Any:
    project = alpha2.ProjectBuilder("P", width=width, height=height, frame_rate=30)
    project.video_track().clip(project.generated(generator), at=0, duration=4)
    return project


def build(call: dict[str, Any]) -> Any:
    props = call["props"]
    if call["builder"] == "box":
        return box(*(build(child) for child in call.get("children", [])), **props)
    if call["builder"] == "text":
        return text(call["content"], **props)
    if call["builder"] == "shape":
        return shape(call["items"], **props)
    raise AssertionError(call["builder"])


def test_the_builder_writes_the_shared_cases_documents_as_the_ts_builder_does() -> None:
    # packages/alpha2/test/layout.test.mjs asserts the same cases with alpha2/layout.
    for case in corpus("layout-builder-cases.json")["cases"]:
        generator = layout(build(case["root"]))
        assert generator == case["document"], case["name"]
        assert alpha2.validate_project(project_of(generator).build()) == [], case["name"]


def test_props_are_lifted_and_absent_ones_stay_absent() -> None:
    node = box(
        padding=(0.1, "20px", 0.3, 0.4),
        margin=None,  # type: ignore[arg-type]
        translate=(0, "30px"),
        scale=2,
        inset={"left": "10%", "top": None},  # type: ignore[typeddict-item]
        border={"width": "2px", "paint": "#ff0000"},
        opacity=keyframes(key(0, 0.0), key(1, 1.0)),
    )
    assert node == {
        "padding": {
            "mode": "constant",
            "value": {"top": 0.1, "right": "20px", "bottom": 0.3, "left": 0.4},
        },
        "translate": {"mode": "constant", "value": {"x": 0, "y": "30px"}},
        "scale": {"mode": "constant", "value": {"x": 2, "y": 2}},
        "inset": {"left": {"mode": "constant", "value": "10%"}},
        "border": {
            "width": {"mode": "constant", "value": "2px"},
            "paint": {"mode": "constant", "value": "#ff0000"},
        },
        "opacity": keyframes(key(0, 0.0), key(1, 1.0)),
    }
    assert box() == {}


def test_a_bad_length_passes_through_and_validation_reports_the_engines_message() -> None:
    # No second grammar: the string is written as it is, and the engine's parser reports it.
    node = text("a", width="5 px")
    assert node["width"] == {"mode": "constant", "value": "5 px"}
    with pytest.raises(ValueError, match=r'invalid length "5 px" \(write 5px\)'):
        project_of(layout(box(node))).build()


def test_animated_text_holds_between_its_keys() -> None:
    node = text(keyframes(key(0, "0"), key(0.1, "1", "hold"), key(0.2, "2")))
    frames = node["text"]["keyframes"]  # type: ignore[index, typeddict-item]
    assert [frame["interpolation"] for frame in frames] == [{"type": "hold"}] * 3
    assert [frame["value"] for frame in frames] == ["0", "1", "2"]
    eased = {"type": "cubic_bezier", "x1": 0.2, "y1": 0, "x2": 0, "y2": 1}
    with pytest.raises(ValueError, match="only holds"):
        text(keyframes(key(0, "0", easing=eased), key(1, "1")))  # type: ignore[arg-type]


def test_a_count_up_is_one_layout_text_that_validates() -> None:
    count = keyframes(*(key(index / 30, str(index)) for index in range(60)))
    generator = layout(box(text(count, font_size=0.1), justify_content="center"))
    assert alpha2.validate_project(project_of(generator).build()) == []


def test_a_circle_is_round_on_its_picture() -> None:
    # 1920x1080: the shorter side is the height, so a diameter of 0.5 is 540 px each way.
    assert circle((0.5, 0.5), 0.5, picture=(1920, 1080)) == ellipse((0.5, 0.5), (0.28125, 0.5))
    grown = circle((0.5, 0.5), keyframes(key(0, 0.2), key(1, 0.4)), picture=(1080, 1920))
    assert [frame["value"] for frame in grown["size"]["keyframes"]] == [  # type: ignore[typeddict-item]
        {"x": 0.2, "y": 0.2 * 1080 / 1920},
        {"x": 0.4, "y": 0.4 * 1080 / 1920},
    ]
    with pytest.raises(ValueError, match="picture"):
        circle((0.5, 0.5), 0.5, picture=(0, 10))


def generated(name: str, source_id: str) -> Any:
    project = corpus(name)
    return next(source["generator"] for source in project["sources"] if source["id"] == source_id)


def test_the_helpers_write_the_hand_written_parity_corpus_shapes() -> None:
    t150, t180 = alpha2.ticks(150_000), alpha2.ticks(180_000)
    ease: Any = {"type": "cubic_bezier", "x1": 0.4, "y1": 0.0, "x2": 0.2, "y2": 1.0}

    def color(red: float, green: float, blue: float, alpha: float = 1) -> Any:
        return {"red": red, "green": green, "blue": blue, "alpha": alpha}

    def radial(center: tuple[float, float], radius: float, inner: Any, outer: Any) -> Any:
        return {
            "type": "radial_gradient",
            "center": {"x": center[0], "y": center[1]},
            "radius": radius,
            "stops": [{"offset": 0.0, "color": inner}, {"offset": 1.0, "color": outer}],
        }

    def outline(
        *points: tuple[tuple[float, float], tuple[float, float], tuple[float, float]],
    ) -> Any:
        return {
            "subpaths": [
                subpath(
                    *(vertex(at, in_tangent=into, out_tangent=out) for at, into, out in points),
                    closed=True,
                )
            ]
        }

    star = group(
        polystar(
            (0, 0),
            5,
            keyframes(key(0, 0.18), key(t180, 0.24, "hold")),
            kind="star",
            inner_radius=0.08,
            outer_roundness=0.25,
        ),
        trim(
            0,
            keyframes(key(0, 0.3), key(t150, 1.0, "hold")),
            keyframes(key(0, 0), key(t180, 0.5, "hold")),
        ),
        stroke(
            0.02,
            {"type": "solid", "color": color(1, 1, 1)},
            join="round",
            cap="round",
            dash=[0.05, 0.025],
            dash_offset=keyframes(key(0, 0), key(t180, 0.2, "hold")),
        ),
        fill(
            keyframes(
                key(0, radial((0.0, 0.0), 0.2, color(1, 0.8, 0.2), color(0.9, 0.2, 0.3, 0.8))),
                key(
                    t180,
                    radial((0.05, 0.0), 0.3, color(0.2, 0.8, 1), color(0.3, 0.1, 0.9, 0.6)),
                    "hold",
                ),
            )
        ),
        transform=transform(
            position=alpha2.point_keyframes(
                alpha2.point_key(0, (0.25, 0.6), easing=ease, out_tangent=(0.1, -0.4)),
                alpha2.point_key(t180, (0.75, 0.5), "hold", in_tangent=(-0.1, -0.3)),
            ),
            rotation=keyframes(key(0, 0), key(t180, 120, "hold")),
            scale=(1.2, 0.8),
            opacity=0.85,
        ),
    )
    blob = group(
        path(
            keyframes(
                key(
                    0,
                    outline(
                        ((0.0, -0.2), (-0.1, 0.0), (0.1, 0.0)),
                        ((0.2, 0.05), (0.0, -0.1), (0.0, 0.1)),
                        ((-0.05, 0.2), (0.12, 0.0), (-0.12, 0.0)),
                        ((-0.2, 0.0), (0.0, 0.1), (0.0, -0.1)),
                    ),
                ),
                key(
                    t180,
                    outline(
                        ((0.05, -0.25), (-0.05, -0.05), (0.15, 0.05)),
                        ((0.25, 0.0), (0.05, -0.15), (-0.02, 0.12)),
                        ((0.0, 0.25), (0.2, 0.0), (-0.1, 0.0)),
                        ((-0.15, 0.05), (0.0, 0.15), (0.05, -0.2)),
                    ),
                    "hold",
                ),
            )
        ),
        ellipse((0.03, 0.0), keyframes(key(0, (0.12, 0.1)), key(t180, (0.2, 0.16), "hold"))),
        stroke(
            0.012,
            {
                "type": "conic_gradient",
                "center": {"x": 0.0, "y": 0.0},
                "angle_degrees": 30,
                "stops": [
                    {"offset": 0.0, "color": color(1, 1, 0)},
                    {"offset": 0.5, "color": color(0, 1, 1)},
                    {"offset": 1.0, "color": color(1, 1, 0)},
                ],
            },
            join="miter",
            miter_limit=3,
        ),
        fill(
            {
                "type": "linear_gradient",
                "start": {"x": -0.2, "y": -0.2},
                "end": {"x": 0.2, "y": 0.2},
                "stops": [
                    {"offset": 0.0, "color": color(0.2, 0.9, 0.4)},
                    {"offset": 1.0, "color": color(0.1, 0.3, 0.9)},
                ],
                "spread": "reflect",
            },
            rule="evenodd",
        ),
        transform=transform(position=(0.72, 0.35)),
    )
    built = shapes(
        star,
        blob,
        rect(
            (0.5, 0.85),
            keyframes(key(0, (0.1, 0.06)), key(t180, (0.9, 0.06), "hold")),
            corner_radius=0.02,
        ),
        fill(
            keyframes(
                key(0, {"type": "solid", "color": color(0.9, 0.9, 0.9)}),
                key(t180, {"type": "solid", "color": color(0.9, 0.3, 0.1, 0.7)}, "hold"),
            )
        ),
    )
    assert built == generated("host-parity-shapes-project.json", "shapes")


def test_the_helpers_write_the_hand_written_parity_corpus_grid() -> None:
    tile = {"border_radius": "4px"}
    built = layout(
        box(
            text("Q1", background="#ffb020", **tile),  # type: ignore[arg-type]
            text("Q2", background="#4c8dff", **tile),  # type: ignore[arg-type]
            text("Q3", background="#ff5c7a", justify_self="end", padding="0 4px", **tile),  # type: ignore[arg-type]
            box(grid_row="1 / 3", grid_column=-2, background="#2b5c3a"),
            box(grid_column="1 / span 3", background="#f5f5f0c0"),
            display="grid",
            grid_template_columns="repeat(3, 1fr) 24px",
            grid_template_rows="22px 10px",
            align_content="start",
            gap="3px",
            padding="4px 6px",
            color="#0b0d12",
            text_align="center",
            font_size=keyframes(key(0, "12px"), key(alpha2.ticks(120_000), "18px", "hold")),
        )
    )
    assert built == generated("host-parity-layout-project.json", "grid")


def test_every_shape_constructor_validates_dashed_strokes_among_them() -> None:
    star = polystar((0.5, 0.5), 5, 0.3, kind="star", inner_radius=0.12, rotation=10)
    outline = path(
        subpath((0.1, 0.1), vertex((0.9, 0.1), in_tangent=(-0.1, 0.1)), (0.5, 0.9), closed=True)
    )
    generator = shapes(
        group(
            rect((0.25, 0.25), (0.3, 0.2), corner_radius=0.02),
            fill("#1e2230", 0.8),
            stroke(
                0.01,
                "#ffb020",
                dash=[0.02, keyframes(key(0, 0.01), key(1, 0.03))],
                dash_offset=0.005,
                cap="round",
            ),
            transform=transform(position=(0.1, 0), scale=1.2, rotation=5, opacity=0.9),
        ),
        star,
        fill({"type": "solid", "color": "#ffffff"}, rule="evenodd"),
        outline,
        stroke(0.004, "#000000", join="bevel", miter_limit=2),
        trim(0, keyframes(key(0, 0.0), key(1, 1.0)), 0.25, mode="individually"),
        circle((0.8, 0.8), 0.1, picture=(1920, 1080)),
        fill("#ff0000"),
    )
    dashed: Any = generator["items"][0]["items"][2]  # type: ignore[typeddict-item]
    assert dashed["dash"]["pattern"][0] == {"mode": "constant", "value": 0.02}
    assert dashed["dash"]["offset"] == {"mode": "constant", "value": 0.005}
    assert alpha2.validate_project(project_of(generator).build()) == []
    # The same items in a layout's shape box.
    boxed = layout(box(shape(generator["items"], width=0.3, aspect_ratio=1)))
    assert alpha2.validate_project(project_of(boxed).build()) == []
    with pytest.raises(ValueError, match="dash"):
        stroke(0.01, "#000000", dash_offset=0.1)
