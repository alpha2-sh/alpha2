//! The "Wipes" group's transitions on the CPU against the GPU.

use super::{ParamValue, Tolerance, check};

#[test]
fn horizontal_open_matches() {
    check("horizontal-open", Tolerance::Edges, &[]);
}

#[test]
fn horizontal_close_matches() {
    check("horizontal-close", Tolerance::Edges, &[]);
}

#[test]
fn vertical_open_matches() {
    check("vertical-open", Tolerance::Edges, &[]);
}

#[test]
fn vertical_close_matches() {
    check("vertical-close", Tolerance::Edges, &[]);
}

#[test]
fn screen_melt_matches() {
    check(
        "screen-melt",
        Tolerance::Edges,
        &[
            &[
                ("bars", ParamValue::Float(12.0)),
                ("amplitude", ParamValue::Float(4.0)),
                ("noise", ParamValue::Float(0.6)),
            ],
            &[
                ("bars", ParamValue::Float(50.0)),
                ("frequency", ParamValue::Float(2.0)),
                ("drip_scale", ParamValue::Float(0.0)),
            ],
        ],
    );
}

#[test]
fn squares_wire_matches() {
    check(
        "squares-wire",
        Tolerance::Edges,
        &[
            &[
                ("squares", ParamValue::Vec2([6.0, 4.0])),
                ("smoothness", ParamValue::Float(0.5)),
            ],
            &[
                ("direction", ParamValue::Vec2([-0.3, 1.0])),
                ("smoothness", ParamValue::Float(0.0)),
            ],
            &[("direction", ParamValue::Vec2([0.0, 0.0]))],
        ],
    );
}

#[test]
fn static_wipe_matches() {
    check(
        "static-wipe",
        Tolerance::Edges,
        &[
            &[("up_to_down", ParamValue::Bool(false))],
            &[("max_static_span", ParamValue::Float(0.1))],
            &[("max_static_span", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn zoom_left_wipe_matches() {
    check(
        "zoom-left-wipe",
        Tolerance::Edges,
        &[
            &[("zoom_quickness", ParamValue::Float(0.2))],
            &[("zoom_quickness", ParamValue::Float(0.01))],
        ],
    );
}

#[test]
fn zoom_right_wipe_matches() {
    check(
        "zoom-right-wipe",
        Tolerance::Edges,
        &[
            &[("zoom_quickness", ParamValue::Float(0.2))],
            &[("zoom_quickness", ParamValue::Float(0.01))],
        ],
    );
}

#[test]
fn gradient_wipe_matches() {
    check(
        "gradient-wipe",
        Tolerance::Edges,
        &[
            &[
                ("pattern", ParamValue::Enum(1)),
                ("center", ParamValue::Vec2([0.3, 0.6])),
                ("softness", ParamValue::Float(0.0)),
            ],
            &[
                // Not 45 degrees: at 96 x 64 that seam runs through pixel centres.
                ("pattern", ParamValue::Enum(2)),
                ("angle", ParamValue::Float(40.0)),
                ("softness", ParamValue::Float(0.3)),
            ],
            &[
                ("pattern", ParamValue::Enum(3)),
                ("reversed", ParamValue::Bool(true)),
            ],
            &[
                ("pattern", ParamValue::Enum(4)),
                ("noise_scale", ParamValue::Float(0.2)),
                ("seed", ParamValue::Float(7.0)),
            ],
            &[
                ("angle", ParamValue::Float(120.0)),
                ("softness", ParamValue::Float(0.0)),
            ],
        ],
    );
}
