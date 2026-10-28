//! The "Blur & Sharpen" group's effects on the CPU against the GPU.

use super::{check_effect, effect, float, size};
use crate::tests::cpu_parity::{Parity, Tolerance, test_card};
use crate::{EffectRuntime, ParamValue};

#[test]
fn directional_blur_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let blur = |length, angle| vec![("length", float(length)), ("angle", float(angle))];
    check_effect(
        &parity,
        &card,
        &effect("directional-blur"),
        &[
            (Vec::new(), Tolerance::Edges),
            (blur(0.0, 30.0), Tolerance::Edges),
            (blur(0.1, 90.0), Tolerance::Edges),
            (blur(0.05, 45.0), Tolerance::Edges),
            (blur(0.3, -160.0), Tolerance::Edges),
            (blur(0.5, 0.0), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn radial_blur_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let blur = |mode, amount, angle, center: [f32; 2]| {
        vec![
            ("mode", ParamValue::Enum(mode)),
            ("amount", float(amount)),
            ("angle", float(angle)),
            ("center", ParamValue::Vec2(center)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("radial-blur"),
        &[
            (Vec::new(), Tolerance::Edges),
            (blur(0, 0.6, 10.0, [0.3, 0.7]), Tolerance::Edges),
            (blur(1, 0.1, 30.0, [0.5, 0.5]), Tolerance::Edges),
            (blur(1, 0.1, 90.0, [0.8, 0.2]), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn unsharp_mask_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let sharpen = |amount, radius, threshold| {
        vec![
            ("amount", float(amount)),
            ("radius", float(radius)),
            ("threshold", float(threshold)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("unsharp-mask"),
        &[
            (Vec::new(), Tolerance::Edges),
            (sharpen(2.5, 0.03, 0.0), Tolerance::Edges),
            (sharpen(1.0, 0.01, 0.1), Tolerance::Edges),
            (sharpen(5.0, 0.0, 0.0), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn tilt_shift_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let tilt = |blur, position, angle, focus, falloff| {
        vec![
            ("blur", float(blur)),
            ("position", float(position)),
            ("angle", float(angle)),
            ("focus", float(focus)),
            ("falloff", float(falloff)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("tilt-shift"),
        &[
            (Vec::new(), Tolerance::Edges),
            (tilt(0.1, 0.3, 30.0, 0.1, 0.2), Tolerance::Edges),
            (tilt(0.2, 0.7, -90.0, 0.0, 0.001), Tolerance::Edges),
            (tilt(0.0, 0.5, 0.0, 0.15, 0.25), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn lens_blur_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let blur =
        |radius, highlights| vec![("radius", float(radius)), ("highlights", float(highlights))];
    check_effect(
        &parity,
        &card,
        &effect("lens-blur"),
        &[
            (Vec::new(), Tolerance::Edges),
            (blur(0.02, 0.0), Tolerance::Edges),
            (blur(0.015, 10.0), Tolerance::Edges),
            (blur(0.0, 3.0), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn surface_blur_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let blur = |radius, threshold| vec![("radius", float(radius)), ("threshold", float(threshold))];
    check_effect(
        &parity,
        &card,
        &effect("surface-blur"),
        &[
            (Vec::new(), Tolerance::Unstable),
            (blur(0.02, 0.3), Tolerance::Unstable),
            (blur(0.01, 0.001), Tolerance::Unstable),
            (blur(0.02, 1.0), Tolerance::Unstable),
        ],
        EffectRuntime::default(),
    );
}
