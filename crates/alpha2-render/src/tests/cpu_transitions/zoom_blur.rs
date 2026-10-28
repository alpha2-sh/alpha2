//! The "Zoom & Blur" group's transitions on the CPU against the GPU.

use super::{ParamValue, Tolerance, check};

#[test]
fn defocus_blur_matches() {
    check(
        "defocus-blur",
        Tolerance::Edges,
        &[
            &[("blur_size", ParamValue::Float(0.1))],
            &[("blur_size", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn dreamy_zoom_matches() {
    check(
        "dreamy-zoom",
        Tolerance::Edges,
        &[
            &[
                ("rotation", ParamValue::Float(-60.0)),
                ("scale", ParamValue::Float(2.5)),
            ],
            &[("scale", ParamValue::Float(0.5))],
        ],
    );
}

#[test]
fn revolve_matches() {
    check(
        "revolve",
        Tolerance::Edges,
        &[
            &[
                ("center", ParamValue::Vec2([0.3, 0.6])),
                ("direction", ParamValue::Float(0.7)),
                ("max_rotation", ParamValue::Float(200.0)),
                ("swirl", ParamValue::Float(60.0)),
                ("peak_zoom", ParamValue::Float(0.6)),
                ("barrel", ParamValue::Float(1.2)),
                ("shadow", ParamValue::Float(0.8)),
            ],
            &[
                ("motion_blur", ParamValue::Float(3.0)),
                ("switch_start", ParamValue::Float(0.6)),
                ("switch_end", ParamValue::Float(0.2)),
            ],
            &[
                ("motion_blur", ParamValue::Float(0.0)),
                ("switch_start", ParamValue::Float(0.4)),
                ("switch_end", ParamValue::Float(0.4)),
            ],
        ],
    );
}

#[test]
fn linear_blur_matches() {
    check(
        "linear-blur",
        Tolerance::Edges,
        &[&[("intensity", ParamValue::Float(0.6))]],
    );
}

#[test]
fn mosaic_tiles_matches() {
    check(
        "mosaic-tiles",
        Tolerance::Edges,
        &[
            &[
                ("end_x", ParamValue::Float(-3.0)),
                ("end_y", ParamValue::Float(4.0)),
            ],
            &[
                ("end_x", ParamValue::Float(0.0)),
                ("end_y", ParamValue::Float(0.0)),
            ],
        ],
    );
}

#[test]
fn rotate_scale_fade_matches() {
    check(
        "rotate-scale-fade",
        Tolerance::Edges,
        &[
            &[
                ("center", ParamValue::Vec2([0.3, 0.7])),
                ("rotations", ParamValue::Float(-2.5)),
                ("scale", ParamValue::Float(3.0)),
                ("back_color", ParamValue::Color([0.8, 0.2, 0.1, 0.5])),
            ],
            &[("scale", ParamValue::Float(0.4))],
        ],
    );
}

#[test]
fn rotate_scale_vanish_matches() {
    check(
        "rotate-scale-vanish",
        Tolerance::Edges,
        &[
            &[
                ("fade_in_second", ParamValue::Bool(false)),
                ("reverse_rotation", ParamValue::Bool(true)),
            ],
            &[
                ("reverse_effect", ParamValue::Bool(true)),
                ("back_color", ParamValue::Color([0.2, 0.6, 0.9, 0.75])),
            ],
        ],
    );
}

#[test]
fn scale_in_matches() {
    check("scale-in", Tolerance::Edges, &[]);
}

#[test]
fn simple_zoom_out_matches() {
    check(
        "simple-zoom-out",
        Tolerance::Edges,
        &[
            &[("zoom_quickness", ParamValue::Float(0.4))],
            &[
                ("zoom_quickness", ParamValue::Float(0.6)),
                ("fade", ParamValue::Bool(false)),
            ],
        ],
    );
}

#[test]
fn zoom_in_circles_matches() {
    check("zoom-in-circles", Tolerance::Edges, &[]);
}

#[test]
fn zoom_in_out_matches() {
    check("zoom-in-out", Tolerance::Edges, &[]);
}

#[test]
fn zoom_blur_matches() {
    check(
        "zoom-blur",
        Tolerance::Edges,
        &[
            &[
                ("center", ParamValue::Vec2([0.2, 0.7])),
                ("strength", ParamValue::Float(1.0)),
            ],
            &[("strength", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn whip_pan_matches() {
    check(
        "whip-pan",
        Tolerance::Edges,
        &[
            &[
                ("direction", ParamValue::Enum(1)),
                ("blur", ParamValue::Float(1.0)),
            ],
            &[("direction", ParamValue::Enum(2))],
            &[
                ("direction", ParamValue::Enum(3)),
                ("blur", ParamValue::Float(0.0)),
            ],
        ],
    );
}
