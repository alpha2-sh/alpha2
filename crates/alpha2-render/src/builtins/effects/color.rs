//! The "Color" group: grading and color replacement.

use alpha2_shaders::{
    BLACK_AND_WHITE, CHANNEL_MIXER, COLOR_CONTROLS, COLOR_ISOLATE, DUOTONE, FILL, GRADIENT_MAP,
    HUE_SATURATION, INVERT, LEVELS, LIFT_GAMMA_GAIN, POSTERIZE, SEPIA, SOLARIZE, THRESHOLD,
    VIBRANCE,
};

use crate::builtins::{
    boolean, choice, color, degrees, float, float_step, lut_3d_definition, simple_effect,
    thresholds,
};
use crate::{EffectDef, ParamSpec};

// A table, one entry per effect.
#[allow(clippy::too_many_lines)]
pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        simple_effect(
            "color-controls",
            COLOR_CONTROLS,
            vec![
                float("exposure", 0.0, -3.0, 3.0),
                float("contrast", 1.0, 0.0, 2.0),
                float("saturation", 1.0, 0.0, 2.0),
                float("temperature", 0.0, -1.0, 1.0),
                float("tint", 0.0, -1.0, 1.0),
            ],
        ),
        lut_3d_definition(),
        simple_effect("invert", INVERT, vec![float("amount", 1.0, 0.0, 1.0)]),
        // The weights apply in linear light; the defaults are the BT.709 luminance.
        simple_effect(
            "black-and-white",
            BLACK_AND_WHITE,
            vec![
                float("amount", 1.0, 0.0, 1.0),
                float("red", 0.2126, -2.0, 3.0),
                float("green", 0.7152, -2.0, 3.0),
                float("blue", 0.0722, -2.0, 3.0),
            ],
        ),
        simple_effect("sepia", SEPIA, vec![float("amount", 1.0, 0.0, 1.0)]),
        // The colors' alpha is not used.
        simple_effect(
            "duotone",
            DUOTONE,
            vec![
                color("shadows", [0.08, 0.05, 0.25, 1.0]),
                color("highlights", [1.0, 0.85, 0.55, 1.0]),
                float("amount", 1.0, 0.0, 1.0),
            ],
        ),
        // The colors' alpha is not used; `midpoint` is the gray the midtone color sits at.
        simple_effect(
            "gradient-map",
            GRADIENT_MAP,
            vec![
                color("shadows", [0.0, 0.0, 0.0, 1.0]),
                color("midtones", [0.5, 0.5, 0.5, 1.0]),
                color("highlights", [1.0, 1.0, 1.0, 1.0]),
                float("midpoint", 0.5, 0.05, 0.95),
                float("amount", 1.0, 0.0, 1.0),
            ],
        ),
        simple_effect(
            "posterize",
            POSTERIZE,
            vec![float_step("levels", 6.0, 2.0, 64.0, 1.0)],
        ),
        simple_effect(
            "threshold",
            THRESHOLD,
            vec![
                float("level", 0.5, 0.0, 1.0),
                float("softness", 0.0, 0.0, 0.5),
            ],
        ),
        simple_effect(
            "levels",
            LEVELS,
            vec![
                choice("channel", 0, &["all", "red", "green", "blue"]),
                float("input_black", 0.0, 0.0, 1.0),
                float("input_white", 1.0, 0.0, 1.0),
                float("gamma", 1.0, 0.1, 10.0),
                float("output_black", 0.0, 0.0, 1.0),
                float("output_white", 1.0, 0.0, 1.0),
            ],
        ),
        // Saturation is HSV saturation: -1 leaves the gray of the largest channel, not the
        // luminance black-and-white gives.
        simple_effect(
            "hue-saturation",
            HUE_SATURATION,
            vec![
                degrees("hue", 0.0, -180.0, 180.0),
                float("saturation", 0.0, -1.0, 1.0),
                float("lightness", 0.0, -1.0, 1.0),
            ],
        ),
        simple_effect("vibrance", VIBRANCE, vec![float("amount", 0.0, -1.0, 1.0)]),
        simple_effect(
            "channel-mixer",
            CHANNEL_MIXER,
            vec![
                mix_row("red", [1.0, 0.0, 0.0, 0.0]),
                mix_row("green", [0.0, 1.0, 0.0, 0.0]),
                mix_row("blue", [0.0, 0.0, 1.0, 0.0]),
                boolean("monochrome", false),
            ],
        ),
        simple_effect(
            "lift-gamma-gain",
            LIFT_GAMMA_GAIN,
            vec![wheel("lift"), wheel("gamma"), wheel("gain")],
        ),
        simple_effect(
            "color-isolate",
            COLOR_ISOLATE,
            vec![
                degrees("hue", 120.0, 0.0, 360.0),
                degrees("range", 30.0, 0.0, 180.0),
                degrees("softness", 20.0, 0.0, 180.0),
                float("amount", 1.0, 0.0, 1.0),
            ],
        ),
        simple_effect(
            "solarize",
            SOLARIZE,
            vec![float("threshold", 0.5, 0.0, 1.0)],
        ),
        // The color's alpha scales `amount`: a strength, not a coverage; the pixel's alpha is
        // kept.
        simple_effect(
            "fill",
            FILL,
            vec![
                color("color", [1.0, 1.0, 1.0, 1.0]),
                float("amount", 1.0, 0.0, 1.0),
            ],
        ),
    ]
}

/// One output channel of the channel mixer: from red, green, blue, and a
/// constant, each in `-2..=2`.
fn mix_row(name: &str, default: [f32; 4]) -> ParamSpec {
    ParamSpec {
        min: Some(-2.0),
        max: Some(2.0),
        step: Some(0.01),
        ..thresholds(name, default)
    }
}

/// A grading wheel: an rgb offset and a master offset in w, each in
/// `-1..=1`; zero leaves the color as it is.
fn wheel(name: &str) -> ParamSpec {
    ParamSpec {
        min: Some(-1.0),
        max: Some(1.0),
        step: Some(0.005),
        ..thresholds(name, [0.0; 4])
    }
}
