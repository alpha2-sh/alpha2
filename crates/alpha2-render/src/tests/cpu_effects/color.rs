//! The "Color" group's effects on the CPU against the GPU.

use super::{Params, check_effect, effect, float};
use crate::tests::cpu_parity::{Parity, Tolerance, test_card};
use crate::{EffectRuntime, ParamValue};

/// Checks the built-in `name` on the test card at each of `cases`.
fn check(name: &str, cases: &[(Params, Tolerance)]) {
    let parity = Parity::new();
    let card = parity.picture(super::size(), &test_card(super::size(), false));
    check_effect(
        &parity,
        &card,
        &effect(name),
        cases,
        EffectRuntime::default(),
    );
}

fn color(value: [f32; 4]) -> ParamValue {
    ParamValue::Color(value)
}

fn vec4(value: [f32; 4]) -> ParamValue {
    ParamValue::Vec4(value)
}

#[test]
fn invert_matches() {
    check(
        "invert",
        &[
            (Vec::new(), Tolerance::Close),
            // Not exact on a resampled layer: the GPU's divide by alpha is not correctly rounded.
            (vec![("amount", float(0.0))], Tolerance::Close),
            (vec![("amount", float(0.5))], Tolerance::Close),
        ],
    );
}

#[test]
fn black_and_white_matches() {
    let weights = |red, green, blue| {
        vec![
            ("red", float(red)),
            ("green", float(green)),
            ("blue", float(blue)),
        ]
    };
    check(
        "black-and-white",
        &[
            (Vec::new(), Tolerance::Close),
            (weights(1.0, 0.0, 0.0), Tolerance::Close),
            (weights(0.0, 0.0, 1.0), Tolerance::Close),
            (weights(2.5, -1.5, 0.4), Tolerance::Close),
            (vec![("amount", float(0.5))], Tolerance::Close),
        ],
    );
}

#[test]
fn sepia_matches() {
    check(
        "sepia",
        &[
            (Vec::new(), Tolerance::Close),
            (vec![("amount", float(0.0))], Tolerance::Close),
            (vec![("amount", float(0.6))], Tolerance::Close),
        ],
    );
}

#[test]
fn duotone_matches() {
    check(
        "duotone",
        &[
            (Vec::new(), Tolerance::Close),
            (
                vec![
                    ("shadows", color([0.6, 0.0, 0.3, 0.2])),
                    ("highlights", color([0.1, 0.9, 0.8, 0.0])),
                ],
                Tolerance::Close,
            ),
            (vec![("amount", float(0.4))], Tolerance::Close),
        ],
    );
}

#[test]
fn gradient_map_matches() {
    let map = |midpoint, amount| {
        vec![
            ("shadows", color([0.1, 0.0, 0.4, 1.0])),
            ("midtones", color([0.9, 0.2, 0.1, 0.5])),
            ("highlights", color([1.0, 0.95, 0.3, 1.0])),
            ("midpoint", float(midpoint)),
            ("amount", float(amount)),
        ]
    };
    check(
        "gradient-map",
        &[
            (Vec::new(), Tolerance::Close),
            (map(0.5, 1.0), Tolerance::Close),
            (map(0.05, 0.7), Tolerance::Close),
            (map(0.95, 1.0), Tolerance::Close),
        ],
    );
}

#[test]
fn posterize_matches() {
    check(
        "posterize",
        &[
            (Vec::new(), Tolerance::Steps),
            (vec![("levels", float(2.0))], Tolerance::Steps),
            (vec![("levels", float(4.5))], Tolerance::Steps),
            (vec![("levels", float(64.0))], Tolerance::Steps),
        ],
    );
}

#[test]
fn threshold_matches() {
    let threshold = |level, softness| vec![("level", float(level)), ("softness", float(softness))];
    check(
        "threshold",
        &[
            (Vec::new(), Tolerance::Unstable),
            (threshold(0.3, 0.2), Tolerance::Close),
            (threshold(0.8, 0.5), Tolerance::Close),
            (threshold(0.0, 0.0), Tolerance::Unstable),
        ],
    );
}

#[test]
fn levels_matches() {
    let levels = |channel, input: [f32; 2], gamma, output: [f32; 2]| {
        vec![
            ("channel", ParamValue::Enum(channel)),
            ("input_black", float(input[0])),
            ("input_white", float(input[1])),
            ("gamma", float(gamma)),
            ("output_black", float(output[0])),
            ("output_white", float(output[1])),
        ]
    };
    check(
        "levels",
        &[
            (Vec::new(), Tolerance::Close),
            (levels(0, [0.1, 0.8], 2.2, [0.05, 0.9]), Tolerance::Close),
            (levels(1, [0.0, 1.0], 0.4, [0.2, 1.0]), Tolerance::Close),
            (levels(3, [0.1, 0.9], 2.5, [1.0, 0.0]), Tolerance::Close),
            // A slope near 20 magnifies the half-float step of the resampled layer.
            (levels(3, [0.3, 0.6], 2.5, [1.0, 0.0]), Tolerance::Edges),
            // `x^0.1` is steep near black, where samplers round a resampled layer differently.
            (levels(0, [0.0, 0.6], 10.0, [0.0, 1.0]), Tolerance::Unstable),
            (levels(2, [0.0, 1.0], 0.1, [0.0, 1.0]), Tolerance::Close),
        ],
    );
}

#[test]
fn hue_saturation_matches() {
    let adjust = |hue, saturation, lightness| {
        vec![
            ("hue", float(hue)),
            ("saturation", float(saturation)),
            ("lightness", float(lightness)),
        ]
    };
    check(
        "hue-saturation",
        &[
            (Vec::new(), Tolerance::Edges),
            (adjust(90.0, 0.5, 0.2), Tolerance::Edges),
            (adjust(-150.0, -1.0, -0.6), Tolerance::Edges),
            (adjust(180.0, 1.0, 1.0), Tolerance::Edges),
        ],
    );
}

#[test]
fn vibrance_matches() {
    check(
        "vibrance",
        &[
            (Vec::new(), Tolerance::Close),
            (vec![("amount", float(1.0))], Tolerance::Close),
            (vec![("amount", float(-1.0))], Tolerance::Close),
            (vec![("amount", float(0.4))], Tolerance::Close),
        ],
    );
}

#[test]
fn channel_mixer_matches() {
    let mixer = |red, green, blue, monochrome| {
        vec![
            ("red", vec4(red)),
            ("green", vec4(green)),
            ("blue", vec4(blue)),
            ("monochrome", ParamValue::Bool(monochrome)),
        ]
    };
    check(
        "channel-mixer",
        &[
            (Vec::new(), Tolerance::Close),
            (
                mixer(
                    [0.0, 0.0, 1.0, 0.1],
                    [0.5, 0.5, 0.0, -0.2],
                    [1.2, -0.4, 0.3, 0.0],
                    false,
                ),
                Tolerance::Close,
            ),
            (
                mixer(
                    [0.3, 0.6, 0.1, 0.05],
                    [2.0, 2.0, 2.0, 2.0],
                    [-2.0, 0.0, 0.0, 0.0],
                    true,
                ),
                Tolerance::Close,
            ),
        ],
    );
}

#[test]
fn lift_gamma_gain_matches() {
    let grade = |lift, gamma, gain| {
        vec![
            ("lift", vec4(lift)),
            ("gamma", vec4(gamma)),
            ("gain", vec4(gain)),
        ]
    };
    check(
        "lift-gamma-gain",
        &[
            (Vec::new(), Tolerance::Close),
            (
                grade(
                    [0.05, 0.0, -0.05, 0.02],
                    [0.1, 0.0, -0.1, 0.2],
                    [0.0, 0.1, 0.2, -0.1],
                ),
                Tolerance::Close,
            ),
            (
                grade(
                    [-0.2, 0.3, 0.0, 0.1],
                    [0.0, 0.0, 0.0, -0.6],
                    [1.0, 0.0, -0.5, 0.0],
                ),
                Tolerance::Close,
            ),
            (
                grade([0.0; 4], [-1.0, 1.0, 0.5, 0.0], [0.0; 4]),
                Tolerance::Close,
            ),
        ],
    );
}

#[test]
fn color_isolate_matches() {
    let isolate = |hue, range, softness, amount| {
        vec![
            ("hue", float(hue)),
            ("range", float(range)),
            ("softness", float(softness)),
            ("amount", float(amount)),
        ]
    };
    check(
        "color-isolate",
        &[
            (Vec::new(), Tolerance::Edges),
            (isolate(240.0, 60.0, 40.0, 0.5), Tolerance::Edges),
            (isolate(10.0, 90.0, 180.0, 1.0), Tolerance::Edges),
            // No softness: a hue on the edge of the range is kept or turned gray.
            (isolate(0.0, 20.0, 0.0, 1.0), Tolerance::Steps),
        ],
    );
}

#[test]
fn solarize_matches() {
    check(
        "solarize",
        &[
            (Vec::new(), Tolerance::Unstable),
            (vec![("threshold", float(0.0))], Tolerance::Unstable),
            (vec![("threshold", float(0.25))], Tolerance::Unstable),
            (vec![("threshold", float(1.0))], Tolerance::Close),
        ],
    );
}

#[test]
fn fill_matches() {
    let fill = |value, amount| vec![("color", color(value)), ("amount", float(amount))];
    check(
        "fill",
        &[
            (Vec::new(), Tolerance::Close),
            (fill([0.9, 0.2, 0.1, 1.0], 0.6), Tolerance::Close),
            (fill([0.1, 0.4, 1.0, 0.5], 1.0), Tolerance::Close),
            (fill([0.0, 0.0, 0.0, 1.0], 0.0), Tolerance::Close),
        ],
    );
}
