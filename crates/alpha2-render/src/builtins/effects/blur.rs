//! The "Blur & Sharpen" group.

use alpha2_shaders::{EXPANDED_EFFECT, LENS_BLUR, RADIAL_BLUR, SHARPEN, SURFACE_BLUR};

use crate::EffectDef;
use crate::builtins::{
    DIRECTIONAL_BLUR_ID, TILT_SHIFT_ID, UNSHARP_MASK_ID, choice, degrees, float, float_step,
    gaussian_blur_definition, simple_effect, vec2_range,
};

pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        gaussian_blur_definition(),
        simple_effect("sharpen", SHARPEN, vec![float("amount", 0.6, 0.0, 3.0)]),
        directional_blur_definition(),
        simple_effect(
            "radial-blur",
            RADIAL_BLUR,
            vec![
                choice("mode", 0, &["zoom", "spin"]),
                float("amount", 0.1, 0.0, 1.0),
                degrees("angle", 10.0, 0.0, 90.0),
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
            ],
        ),
        unsharp_mask_definition(),
        tilt_shift_definition(),
        simple_effect(
            "lens-blur",
            LENS_BLUR,
            vec![
                float_step("radius", 0.008, 0.0, 0.02, 0.0005),
                float("highlights", 0.0, 0.0, 10.0),
            ],
        ),
        simple_effect(
            "surface-blur",
            SURFACE_BLUR,
            vec![
                float_step("radius", 0.004, 0.0, 0.02, 0.0005),
                float_step("threshold", 0.1, 0.001, 1.0, 0.005),
            ],
        ),
    ]
}

/// A Gaussian along one direction: its standard deviation is half its
/// `length` (output heights), as the blur's is half its radius, and `angle`
/// turns it counter-clockwise from horizontal. A render graph draws it as
/// internal passes; its own body is never drawn.
pub(crate) fn directional_blur_definition() -> EffectDef {
    EffectDef {
        id: DIRECTIONAL_BLUR_ID.into(),
        ..simple_effect(
            "directional-blur",
            EXPANDED_EFFECT,
            vec![
                float_step("length", 0.02, 0.0, 0.5, 0.001),
                degrees("angle", 0.0, -360.0, 360.0),
            ],
        )
    }
}

/// The source pushed away from its Gaussian blur of `radius` by `amount`,
/// where the detail's luminance passes `threshold`. A render graph draws it
/// as internal passes; its own body is never drawn.
pub(crate) fn unsharp_mask_definition() -> EffectDef {
    EffectDef {
        id: UNSHARP_MASK_ID.into(),
        ..simple_effect(
            "unsharp-mask",
            EXPANDED_EFFECT,
            vec![
                float("amount", 1.0, 0.0, 5.0),
                float_step("radius", 0.004, 0.0, 0.1, 0.0005),
                float("threshold", 0.0, 0.0, 1.0),
            ],
        )
    }
}

/// A miniature look: sharp in a band `focus` wide through `position`
/// (a height in uv), turned by `angle`, blurring over `falloff` to a
/// Gaussian of half `blur` (output heights). Two blurs are mixed, so the
/// blur seems to grow with the distance though no pixel's radius changes.
/// A render graph draws it as internal passes; its own body is never drawn.
pub(crate) fn tilt_shift_definition() -> EffectDef {
    EffectDef {
        id: TILT_SHIFT_ID.into(),
        ..simple_effect(
            "tilt-shift",
            EXPANDED_EFFECT,
            vec![
                float_step("blur", 0.02, 0.0, 0.2, 0.001),
                float("position", 0.5, 0.0, 1.0),
                degrees("angle", 0.0, -90.0, 90.0),
                float("focus", 0.15, 0.0, 1.0),
                float_step("falloff", 0.25, 0.001, 1.0, 0.005),
            ],
        )
    }
}
