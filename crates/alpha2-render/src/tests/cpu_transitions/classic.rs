//! The "Classic" group's transitions on the CPU against the GPU.

use super::{Tolerance, check};
use crate::ParamValue;

#[test]
fn bounce_matches() {
    check(
        "bounce",
        Tolerance::Edges,
        &[
            &[
                ("shadow_color", ParamValue::Color([0.2, 0.1, 0.6, 0.9])),
                ("shadow_height", ParamValue::Float(0.2)),
            ],
            &[("bounces", ParamValue::Float(1.5))],
        ],
    );
}

#[test]
fn directional_scaled_matches() {
    check(
        "directional-scaled",
        Tolerance::Edges,
        &[
            &[
                ("direction", ParamValue::Vec2([1.0, 0.0])),
                ("scale", ParamValue::Float(0.4)),
            ],
            &[
                ("direction", ParamValue::Vec2([-0.4, 0.7])),
                ("scale", ParamValue::Float(1.6)),
            ],
            &[("direction", ParamValue::Vec2([0.0, 0.0]))],
        ],
    );
}

#[test]
fn slides_matches() {
    check(
        "slides",
        Tolerance::Edges,
        &[
            &[
                ("origin", ParamValue::Enum(5)),
                ("slide_in", ParamValue::Bool(true)),
            ],
            &[("origin", ParamValue::Enum(8))],
            &[
                ("origin", ParamValue::Enum(3)),
                ("slide_in", ParamValue::Bool(true)),
            ],
        ],
    );
}

#[test]
fn fold_matches() {
    check("fold", Tolerance::Edges, &[]);
}

#[test]
fn directional_easing_matches() {
    check(
        "directional-easing",
        Tolerance::Edges,
        &[
            &[("direction", ParamValue::Vec2([1.0, 0.0]))],
            &[("direction", ParamValue::Vec2([-0.4, 0.7]))],
            &[("direction", ParamValue::Vec2([0.0, 0.0]))],
        ],
    );
}

#[test]
fn hsv_fade_matches() {
    check("hsv-fade", Tolerance::Edges, &[]);
}

#[test]
fn left_right_matches() {
    check(
        "left-right",
        Tolerance::Edges,
        &[&[("back_color", ParamValue::Color([0.9, 0.3, 0.2, 0.5]))]],
    );
}

#[test]
fn top_bottom_matches() {
    check(
        "top-bottom",
        Tolerance::Edges,
        &[&[("back_color", ParamValue::Color([0.1, 0.8, 0.4, 0.0]))]],
    );
}

#[test]
fn multiply_blend_matches() {
    check("multiply-blend", Tolerance::Close, &[]);
}

#[test]
fn rolls_matches() {
    check(
        "rolls",
        Tolerance::Edges,
        &[
            &[
                ("corner", ParamValue::Enum(1)),
                ("rot_down", ParamValue::Bool(true)),
            ],
            &[("corner", ParamValue::Enum(2))],
            &[
                ("corner", ParamValue::Enum(3)),
                ("rot_down", ParamValue::Bool(true)),
            ],
        ],
    );
}

fn check_split_slide(name: &str) {
    check(
        name,
        Tolerance::Edges,
        &[&[("reversed", ParamValue::Bool(true))]],
    );
}

#[test]
fn split_slide_in_horizontal_matches() {
    check_split_slide("split-slide-in-horizontal");
}

#[test]
fn split_slide_in_vertical_matches() {
    check_split_slide("split-slide-in-vertical");
}

#[test]
fn split_slide_out_horizontal_matches() {
    check_split_slide("split-slide-out-horizontal");
}

#[test]
fn split_slide_out_vertical_matches() {
    check_split_slide("split-slide-out-vertical");
}

#[test]
fn split_slide_in_out_horizontal_matches() {
    check_split_slide("split-slide-in-out-horizontal");
}

#[test]
fn split_slide_in_out_vertical_matches() {
    check_split_slide("split-slide-in-out-vertical");
}

#[test]
fn rotate_matches() {
    check("rotate", Tolerance::Edges, &[]);
}

#[test]
fn x_axis_translation_matches() {
    check("x-axis-translation", Tolerance::Edges, &[]);
}

#[test]
fn cover_matches() {
    check(
        "cover",
        Tolerance::Edges,
        &[
            &[("direction", ParamValue::Enum(1))],
            &[("direction", ParamValue::Enum(2))],
            &[("direction", ParamValue::Enum(3))],
        ],
    );
}

#[test]
fn uncover_matches() {
    check(
        "uncover",
        Tolerance::Edges,
        &[
            &[("direction", ParamValue::Enum(1))],
            &[("direction", ParamValue::Enum(2))],
            &[("direction", ParamValue::Enum(3))],
        ],
    );
}
