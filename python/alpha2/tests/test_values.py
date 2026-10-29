"""The value, effect-value and visual constructors write the documents the engine reads."""

from __future__ import annotations

from typing import Any

import pytest

import alpha2
from alpha2 import constant, effect, fx, key, keyframes, lift, visual


def project_with(**clip_fields: Any) -> alpha2.ProjectBuilder:
    project = alpha2.ProjectBuilder("P", width=1920, height=1080, frame_rate=30)
    source = project.generated({"type": "fill", "paint": "#ff0000"})
    project.video_track().clip(source, at=0, duration=2, **clip_fields)
    return project


def test_lift_makes_a_constant_and_leaves_an_animated_value_as_it_is() -> None:
    assert lift(0.5) == {"mode": "constant", "value": 0.5}
    assert lift(lift(0.5)) == lift(0.5)
    fade = keyframes(key(0, 0.0), key(1, 1.0))
    assert lift(fade) is fade
    assert lift({"x": 1, "y": 2}) == constant({"x": 1, "y": 2})


def test_a_key_is_at_its_time_in_ticks() -> None:
    assert key(1.5, 1.0) == {
        "time": int(1.5 * alpha2.TICKS_PER_SECOND),
        "value": 1.0,
        "interpolation": {"type": "linear"},
    }
    assert key(alpha2.ticks(7), "a", "hold")["time"] == 7
    assert key(alpha2.ticks(7), "a", "hold")["interpolation"] == {"type": "hold"}
    ease = {"type": "cubic_bezier", "x1": 0.2, "y1": 0.0, "x2": 0.0, "y2": 1.0}
    assert key(0, 1.0, easing=ease)["interpolation"] == ease  # type: ignore[arg-type]
    assert key(0, 1.0, ease)["interpolation"] == ease
    with pytest.raises(ValueError, match="not both"):
        key(0, 1.0, "hold", easing=ease)  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="not an interpolation"):
        key(0, 1.0, "smooth")  # type: ignore[arg-type]


def test_keyframes_out_of_order_or_at_one_time_are_refused_not_sorted() -> None:
    with pytest.raises(ValueError, match="ascending"):
        keyframes(key(1, 0.0), key(0, 1.0))
    with pytest.raises(ValueError, match="ascending"):
        keyframes(key(1, 0.0), key(1, 1.0))
    with pytest.raises(ValueError, match="at least one"):
        keyframes()


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        (0.25, {"type": "float", "value": 0.25}),
        (True, {"type": "bool", "value": True}),
        (False, {"type": "bool", "value": False}),
        ((0.1, 0.9), {"type": "vec2", "value": [0.1, 0.9]}),
        ("#ff000080", {"type": "color", "value": [1.0, 0.0, 0.0, 128 / 255]}),
        ("#f00", {"type": "color", "value": [1.0, 0.0, 0.0, 1.0]}),
        (fx.enum(2), {"type": "enum", "value": 2}),
        (fx.vec4(1, 2, 3, 4), {"type": "vec4", "value": [1.0, 2.0, 3.0, 4.0]}),
        (fx.color((1, 0.5, 0)), {"type": "color", "value": [1.0, 0.5, 0.0, 1.0]}),
        (fx.scalar(1), {"type": "float", "value": 1.0}),
        (fx.flag(True), {"type": "bool", "value": True}),
    ],
)
def test_fx_param_lifts_a_value_that_is_not_ambiguous(value: Any, expected: Any) -> None:
    assert fx.param(value) == constant(expected)


def test_fx_param_refuses_an_ambiguous_value_naming_both_constructors() -> None:
    with pytest.raises(
        TypeError, match=r"1 is ambiguous: write fx\.scalar\(1\.0\) or fx\.enum\(1\)"
    ):
        fx.param(1)
    with pytest.raises(TypeError, match=r"fx\.vec4\(\.\.\.\) or fx\.color\(\.\.\.\)"):
        fx.param((1.0, 0.0, 0.0, 1.0))
    with pytest.raises(TypeError, match="not an effect value"):
        fx.param(None)
    with pytest.raises(ValueError, match="hex color"):
        fx.param("red")
    with pytest.raises(TypeError):
        fx.enum(True)


def test_fx_param_lifts_each_keyframe_value_and_passes_fx_values_through() -> None:
    radius = keyframes(key(0, 0.0), key(1, fx.scalar(0.02)))
    assert fx.param(radius) == {
        "mode": "keyframes",
        "keyframes": [
            {
                "time": 0,
                "value": {"type": "float", "value": 0.0},
                "interpolation": {"type": "linear"},
            },
            {
                "time": alpha2.TICKS_PER_SECOND,
                "value": {"type": "float", "value": 0.02},
                "interpolation": {"type": "linear"},
            },
        ],
    }
    assert fx.param(fx.param(radius)) == fx.param(radius)


def test_effect_writes_the_hand_written_invocation() -> None:
    assert effect("alpha2.effect.gaussian-blur", radius=0.02) == {
        "id": "alpha2.effect.gaussian-blur",
        "version": 1,
        "params": {"radius": {"mode": "constant", "value": {"type": "float", "value": 0.02}}},
    }
    assert effect("alpha2.effect.lut", 2, resources={"lut": "grade"}) == {
        "id": "alpha2.effect.lut",
        "version": 2,
        "resources": {"lut": "grade"},
    }


def test_visual_writes_only_the_fields_given_lifted() -> None:
    assert visual() == {}
    assert visual(opacity=0.5) == {"opacity": constant(0.5)}
    assert visual(position=(0.25, 0.75), anchor=(0, 0), scale=2, rotation=15) == {
        "position": constant({"x": 0.25, "y": 0.75}),
        "anchor": {"x": 0, "y": 0},
        "scale": constant({"x": 2, "y": 2}),
        "rotation_degrees": constant(15),
    }
    moving = visual(
        position=keyframes(key(0, (0.0, 0.5)), key(1, (1.0, 0.5))),
        scale=keyframes(key(0, 1.0), key(1, (2.0, 1.0))),  # type: ignore[arg-type]
    )
    assert [frame["value"] for frame in moving["position"]["keyframes"]] == [  # type: ignore[typeddict-item]
        {"x": 0.0, "y": 0.5},
        {"x": 1.0, "y": 0.5},
    ]
    assert [frame["value"] for frame in moving["scale"]["keyframes"]] == [  # type: ignore[typeddict-item]
        {"x": 1.0, "y": 1.0},
        {"x": 2.0, "y": 1.0},
    ]


def test_curved_positions_carry_their_tangents() -> None:
    path = alpha2.point_keyframes(
        alpha2.point_key(0, (0.2, 0.5), out_tangent=(0.1, -0.2)),
        alpha2.point_key(1, (0.8, 0.5), in_tangent=(-0.1, -0.2)),
    )
    assert path["keyframes"][0]["out_tangent"] == {"x": 0.1, "y": -0.2}
    assert alpha2.validate_project(project_with(visual=visual(position=path)).build()) == []


def test_the_constructed_documents_validate() -> None:
    fade = visual(opacity=keyframes(key(0, 0.0), key(1, 1.0)))
    blur = effect("alpha2.effect.gaussian-blur", radius=keyframes(key(0, 0.0), key(1, 0.02)))
    project = project_with(visual=fade, effects=[blur]).build()
    assert alpha2.validate_project(project) == []
    # The clip shows composition time 0 to 2 s: from 0 to 3 s the radius moves toward the key at
    # 3 s, but no frame depends on the key at 5 s. The lint says so.
    late = keyframes(key(0, 0.0), key(3, 0.02), key(5, 0.04))
    issues = alpha2.lint_project(
        project_with(effects=[effect("alpha2.effect.gaussian-blur", radius=late)]).build()
    )
    assert [(issue["kind"], issue["time"]) for issue in issues] == [
        ("unreached_keyframe", 5 * alpha2.TICKS_PER_SECOND)
    ]
