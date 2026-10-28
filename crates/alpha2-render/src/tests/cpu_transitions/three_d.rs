//! The "3D" group's transitions on the CPU against the GPU.

use super::{Tolerance, check};
use crate::ParamValue;

#[test]
fn cube_matches() {
    check(
        "cube",
        Tolerance::Edges,
        &[
            &[
                ("persp", ParamValue::Float(0.2)),
                ("unzoom", ParamValue::Float(0.8)),
                ("reflection", ParamValue::Float(0.9)),
                ("floating", ParamValue::Float(20.0)),
            ],
            &[
                ("persp", ParamValue::Float(1.0)),
                ("unzoom", ParamValue::Float(0.0)),
                ("back_color", ParamValue::Color([0.2, 0.5, 0.9, 0.5])),
            ],
        ],
    );
}

#[test]
fn doorway_matches() {
    check(
        "doorway",
        Tolerance::Edges,
        &[
            &[
                ("reflection", ParamValue::Float(0.9)),
                ("perspective", ParamValue::Float(1.5)),
                ("depth", ParamValue::Float(7.0)),
            ],
            &[
                ("perspective", ParamValue::Float(0.0)),
                ("depth", ParamValue::Float(1.0)),
                ("back_color", ParamValue::Color([0.2, 0.5, 0.9, 0.5])),
            ],
        ],
    );
}

#[test]
fn swap_matches() {
    check(
        "swap",
        Tolerance::Edges,
        &[
            &[
                ("reflection", ParamValue::Float(0.9)),
                ("perspective", ParamValue::Float(0.3)),
                ("depth", ParamValue::Float(1.5)),
            ],
            &[
                ("perspective", ParamValue::Float(0.0)),
                ("depth", ParamValue::Float(1.0)),
                ("back_color", ParamValue::Color([0.2, 0.5, 0.9, 0.5])),
            ],
        ],
    );
}

#[test]
fn grid_flip_matches() {
    check(
        "grid-flip",
        Tolerance::Edges,
        &[
            &[
                ("size", ParamValue::Vec2([5.0, 3.0])),
                ("pause", ParamValue::Float(0.3)),
                ("divider_width", ParamValue::Float(0.2)),
                ("randomness", ParamValue::Float(0.6)),
            ],
            &[
                ("divider_width", ParamValue::Float(0.0)),
                ("bg_color", ParamValue::Color([0.2, 0.5, 0.9, 0.5])),
            ],
        ],
    );
}

#[test]
fn tiles_wave_matches() {
    check(
        "tiles-wave",
        Tolerance::Edges,
        &[
            &[
                ("tile_count", ParamValue::Vec2([3.0, 2.0])),
                ("flip_x", ParamValue::Bool(false)),
                ("flip_y", ParamValue::Bool(true)),
            ],
            &[
                ("tile_count", ParamValue::Vec2([1.0, 1.0])),
                ("flip_y", ParamValue::Bool(true)),
            ],
        ],
    );
}

#[test]
fn book_flip_matches() {
    check("book-flip", Tolerance::Edges, &[]);
}

#[test]
fn simple_flip_matches() {
    check(
        "simple-flip",
        Tolerance::Edges,
        &[&[("back_color", ParamValue::Color([0.2, 0.5, 0.9, 0.5]))]],
    );
}

#[test]
fn page_peel_matches() {
    check(
        "page-peel",
        Tolerance::Edges,
        &[
            &[
                ("angle", ParamValue::Float(-30.0)),
                ("radius", ParamValue::Float(0.25)),
                ("back_shade", ParamValue::Float(0.0)),
            ],
            &[
                ("angle", ParamValue::Float(90.0)),
                ("radius", ParamValue::Float(0.02)),
                ("back_shade", ParamValue::Float(1.0)),
            ],
        ],
    );
}
