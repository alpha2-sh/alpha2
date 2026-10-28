//! The "Stylized" group's transitions on the CPU against the GPU.

use super::super::cpu_parity::Tolerance;
use super::check;
use crate::ParamValue;

#[test]
fn block_dissolve_matches() {
    check(
        "block-dissolve",
        Tolerance::Edges,
        &[
            &[("block_size", ParamValue::Float(0.13))],
            &[("block_size", ParamValue::Float(0.007))],
        ],
    );
}

#[test]
fn random_squares_matches() {
    check(
        "random-squares",
        Tolerance::Edges,
        &[
            &[
                ("size", ParamValue::Vec2([12.0, 7.0])),
                ("smoothness", ParamValue::Float(0.2)),
            ],
            &[("smoothness", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn burn_noise_matches() {
    check(
        "burn-noise",
        Tolerance::Close,
        &[
            &[("burn_color", ParamValue::Color([0.2, 0.6, 1.0, 0.5]))],
            &[("burn_color", ParamValue::Color([1.0, 1.0, 1.0, 1.0]))],
        ],
    );
}

#[test]
fn perlin_matches() {
    check(
        "perlin",
        Tolerance::Edges,
        &[
            &[
                ("scale", ParamValue::Float(11.0)),
                ("smoothness", ParamValue::Float(0.1)),
            ],
            &[
                ("smoothness", ParamValue::Float(0.0)),
                ("seed", ParamValue::Float(57.0)),
            ],
        ],
    );
}

#[test]
fn film_burn_matches() {
    check(
        "film-burn",
        Tolerance::Edges,
        &[
            &[("seed", ParamValue::Float(0.0))],
            &[("seed", ParamValue::Float(71.5))],
        ],
    );
}

#[test]
fn overexposure_matches() {
    check(
        "overexposure",
        Tolerance::Close,
        &[
            &[("strength", ParamValue::Float(1.8))],
            &[("strength", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn advanced_mosaic_matches() {
    check(
        "advanced-mosaic",
        Tolerance::Edges,
        &[
            &[("pixel_size", ParamValue::Float(17.0))],
            &[("pixel_size", ParamValue::Float(130.0))],
        ],
    );
}

#[test]
fn burn_matches() {
    check(
        "burn",
        Tolerance::Close,
        &[
            &[("color", ParamValue::Color([0.1, 0.8, 0.4, 0.5]))],
            &[("color", ParamValue::Color([1.0, 1.0, 1.0, 1.0]))],
        ],
    );
}

#[test]
fn crosshatch_matches() {
    check(
        "crosshatch",
        Tolerance::Edges,
        &[
            &[
                ("center", ParamValue::Vec2([0.2, 0.7])),
                ("threshold", ParamValue::Float(0.8)),
                ("fade_edge", ParamValue::Float(0.3)),
            ],
            &[("fade_edge", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn butterfly_wave_scrawler_matches() {
    check(
        "butterfly-wave-scrawler",
        Tolerance::Edges,
        &[
            &[
                ("amplitude", ParamValue::Float(2.5)),
                ("waves", ParamValue::Float(8.0)),
                ("color_separation", ParamValue::Float(0.8)),
            ],
            &[("amplitude", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn crazy_parametric_fun_matches() {
    check(
        "crazy-parametric-fun",
        Tolerance::Edges,
        &[
            &[
                ("a", ParamValue::Float(2.5)),
                ("b", ParamValue::Float(0.7)),
                ("amplitude", ParamValue::Float(40.0)),
                ("smoothness", ParamValue::Float(0.5)),
            ],
            &[("amplitude", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn edge_matches() {
    // The brightness gain multiplies the filtered taps' rounding: at 15 lavapipe's coarser
    // filter weights land up to 7 codes off, so the sets stay at moderate gains.
    check(
        "edge",
        Tolerance::Edges,
        &[
            &[
                ("edge_thickness", ParamValue::Float(0.004)),
                ("edge_brightness", ParamValue::Float(3.0)),
            ],
            &[
                ("edge_thickness", ParamValue::Float(0.01)),
                ("edge_brightness", ParamValue::Float(6.0)),
            ],
        ],
    );
}

#[test]
fn fly_eye_matches() {
    check(
        "fly-eye",
        Tolerance::Edges,
        &[
            &[
                ("size", ParamValue::Float(0.1)),
                ("zoom", ParamValue::Float(20.0)),
                ("color_separation", ParamValue::Float(0.6)),
            ],
            &[
                ("zoom", ParamValue::Float(120.0)),
                ("color_separation", ParamValue::Float(0.0)),
            ],
        ],
    );
}

#[test]
fn hexagonalize_matches() {
    check(
        "hexagonalize",
        Tolerance::Edges,
        &[
            &[
                ("steps", ParamValue::Float(7.0)),
                ("horizontal_hexagons", ParamValue::Float(6.0)),
            ],
            &[("steps", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn kaleidoscope_matches() {
    check(
        "kaleidoscope",
        Tolerance::Edges,
        &[
            &[
                ("speed", ParamValue::Float(3.0)),
                ("angle", ParamValue::Float(-2.0)),
                ("power", ParamValue::Float(0.5)),
            ],
            &[
                ("angle", ParamValue::Float(4.0)),
                ("power", ParamValue::Float(3.0)),
            ],
        ],
    );
}

#[test]
fn luma_burn_matches() {
    // The burn edge is a hard threshold on the sampled luma.
    check(
        "luma-burn",
        Tolerance::Unstable,
        &[
            &[
                ("line_width", ParamValue::Float(0.3)),
                ("spread_color", ParamValue::Color([0.2, 0.4, 1.0, 1.0])),
                ("hot_color", ParamValue::Color([1.0, 1.0, 1.0, 0.5])),
                ("power", ParamValue::Float(2.0)),
                ("intensity", ParamValue::Float(2.0)),
            ],
            &[("line_width", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn luminance_melt_matches() {
    // Hard compares on the sampled luma and on noise.
    check(
        "luminance-melt",
        Tolerance::Unstable,
        &[
            &[
                ("direction", ParamValue::Enum(0)),
                ("l_threshold", ParamValue::Float(0.4)),
                ("above", ParamValue::Bool(true)),
            ],
            &[("l_threshold", ParamValue::Float(1.0))],
        ],
    );
}

#[test]
fn power_kaleido_matches() {
    check(
        "power-kaleido",
        Tolerance::Edges,
        &[
            &[
                ("scale", ParamValue::Float(4.0)),
                ("z", ParamValue::Float(3.0)),
                ("speed", ParamValue::Float(1.0)),
            ],
            &[
                ("scale", ParamValue::Float(0.5)),
                ("z", ParamValue::Float(0.5)),
                ("speed", ParamValue::Float(12.0)),
            ],
        ],
    );
}

#[test]
fn random_noise_matches() {
    check("random-noise", Tolerance::Edges, &[]);
}

#[test]
fn swirl_matches() {
    check("swirl", Tolerance::Edges, &[]);
}

#[test]
fn undulating_burn_out_matches() {
    check(
        "undulating-burn-out",
        Tolerance::Edges,
        &[
            &[
                ("smoothness", ParamValue::Float(0.2)),
                ("center", ParamValue::Vec2([0.3, 0.6])),
                ("color", ParamValue::Color([0.9, 0.3, 0.1, 0.5])),
            ],
            &[("smoothness", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn water_drop_matches() {
    check(
        "water-drop",
        Tolerance::Edges,
        &[
            &[
                ("amplitude", ParamValue::Float(60.0)),
                ("speed", ParamValue::Float(10.0)),
            ],
            &[
                ("amplitude", ParamValue::Float(10.0)),
                ("speed", ParamValue::Float(70.0)),
            ],
        ],
    );
}
