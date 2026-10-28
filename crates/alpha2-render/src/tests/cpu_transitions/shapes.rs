//! The "Shapes" group's transitions on the CPU against the GPU.

use super::{ParamValue, Tolerance, check};

#[test]
fn box_matches() {
    check(
        "box",
        Tolerance::Edges,
        &[
            &[("rect_in", ParamValue::Bool(false))],
            &[("location", ParamValue::Enum(1))],
            &[
                ("rect_in", ParamValue::Bool(false)),
                ("location", ParamValue::Enum(4)),
            ],
        ],
    );
}

#[test]
fn chessboard_matches() {
    check(
        "chessboard",
        Tolerance::Edges,
        &[
            &[("grid_num", ParamValue::Float(4.0))],
            &[("grid_num", ParamValue::Float(23.0))],
        ],
    );
}

#[test]
fn bow_tie_matches() {
    check(
        "bow-tie",
        Tolerance::Edges,
        &[
            &[("adjust", ParamValue::Float(0.3))],
            &[
                ("adjust", ParamValue::Float(0.8)),
                ("reversed", ParamValue::Bool(true)),
            ],
            &[("adjust", ParamValue::Float(1.0))],
        ],
    );
}

#[test]
fn bow_tie_horizontal_matches() {
    check("bow-tie-horizontal", Tolerance::Edges, &[]);
}

#[test]
fn bow_tie_vertical_matches() {
    check("bow-tie-vertical", Tolerance::Edges, &[]);
}

#[test]
fn fragment_matches() {
    check("fragment", Tolerance::Edges, &[]);
}

#[test]
fn polar_function_matches() {
    check(
        "polar-function",
        Tolerance::Edges,
        &[
            &[("segments", ParamValue::Float(1.0))],
            &[("segments", ParamValue::Float(12.0))],
        ],
    );
}

#[test]
fn rectangle_matches() {
    check(
        "rectangle",
        Tolerance::Edges,
        &[
            &[("bg_color", ParamValue::Color([1.0, 0.5, 0.2, 1.0]))],
            &[("bg_color", ParamValue::Color([0.2, 0.4, 1.0, 0.5]))],
        ],
    );
}

#[test]
fn rectangle_crop_matches() {
    check(
        "rectangle-crop",
        Tolerance::Edges,
        &[
            &[("bg_color", ParamValue::Color([1.0, 0.5, 0.2, 1.0]))],
            &[("bg_color", ParamValue::Color([0.2, 0.4, 1.0, 0.5]))],
        ],
    );
}

#[test]
fn star_wipe_matches() {
    check(
        "star-wipe",
        Tolerance::Edges,
        &[
            &[
                ("border_thickness", ParamValue::Float(0.05)),
                ("border_color", ParamValue::Color([1.0, 0.2, 0.1, 0.7])),
            ],
            &[
                ("border_thickness", ParamValue::Float(0.0)),
                ("star_rotation", ParamValue::Float(-0.3)),
                ("star_center", ParamValue::Vec2([0.3, 0.6])),
            ],
        ],
    );
}

#[test]
fn shape_wipe_matches() {
    check(
        "shape-wipe",
        Tolerance::Edges,
        &[
            &[
                ("shape", ParamValue::Enum(1)),
                ("aspect", ParamValue::Float(2.0)),
                ("rotation", ParamValue::Float(30.0)),
                ("softness", ParamValue::Float(0.1)),
            ],
            &[
                ("shape", ParamValue::Enum(2)),
                ("center", ParamValue::Vec2([0.3, 0.7])),
                ("closing", ParamValue::Bool(true)),
            ],
            &[
                ("shape", ParamValue::Enum(3)),
                ("aspect", ParamValue::Float(0.5)),
                ("rotation", ParamValue::Float(-45.0)),
            ],
            &[
                ("shape", ParamValue::Enum(4)),
                ("rotation", ParamValue::Float(10.0)),
                ("softness", ParamValue::Float(0.05)),
            ],
            &[
                ("shape", ParamValue::Enum(5)),
                ("rotation", ParamValue::Float(20.0)),
                ("closing", ParamValue::Bool(true)),
            ],
        ],
    );
}
