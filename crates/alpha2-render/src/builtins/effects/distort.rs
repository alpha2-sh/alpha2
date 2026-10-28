//! The "Distort" group: effects that move a picture's pixels.

use alpha2_shaders::{
    BULGE, CORNER_PIN, CROP, DISPLACEMENT_MAP, KALEIDOSCOPE_EFFECT, LENS_DISTORTION, MIRROR,
    OFFSET, PIXELATE_EFFECT, POLAR_COORDINATES, RIPPLE_EFFECT, SHAKE, SWIRL_EFFECT, TILE,
    TURBULENT_DISPLACE, WAVE_WARP,
};

use crate::builtins::{boolean, choice, degrees, float, float_step, simple_effect, vec2_range};
use crate::{EffectDef, InputSpec, ParamSpec};

/// What a distortion shows where it reads past the source.
fn edge(default: u32) -> ParamSpec {
    choice(
        "edge",
        default,
        &["transparent", "clamp", "repeat", "mirror"],
    )
}

const CHANNELS: &[&str] = &["red", "green", "blue", "alpha", "luma"];

#[allow(clippy::too_many_lines)]
pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        simple_effect(
            "pixelate",
            PIXELATE_EFFECT,
            vec![
                float("blocks", 48.0, 2.0, 400.0),
                choice("shape", 0, &["square", "hexagon"]),
            ],
        ),
        simple_effect(
            "crop",
            CROP,
            vec![
                vec2_range("top_left", [0.0, 0.0], 0.0, 1.0, 0.005),
                vec2_range("bottom_right", [0.0, 0.0], 0.0, 1.0, 0.005),
                float_step("feather", 0.004, 0.0, 0.2, 0.002),
            ],
        ),
        simple_effect(
            "mirror",
            MIRROR,
            vec![
                choice("axis", 0, &["horizontal", "vertical"]),
                boolean("keep_second_half", false),
            ],
        ),
        simple_effect(
            "swirl",
            SWIRL_EFFECT,
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("radius", 0.4, 0.0, 2.0),
                degrees("angle", 180.0, -1080.0, 1080.0),
                edge(0),
            ],
        ),
        simple_effect(
            "bulge",
            BULGE,
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("radius", 0.4, 0.0, 2.0),
                // Positive magnifies, negative pinches.
                float("strength", 0.5, -1.0, 1.0),
                edge(0),
            ],
        ),
        simple_effect(
            "wave-warp",
            WAVE_WARP,
            vec![
                float_step("amplitude", 0.01, 0.0, 0.2, 0.0005),
                float("wavelength", 0.2, 0.01, 2.0),
                // The direction the wave travels.
                degrees("angle", 0.0, -360.0, 360.0),
                // Cycles per second.
                float_step("speed", 0.0, -10.0, 10.0, 0.05),
                degrees("phase", 0.0, -360.0, 360.0),
                edge(1),
            ],
        ),
        simple_effect(
            "ripple",
            RIPPLE_EFFECT,
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float_step("amplitude", 0.01, 0.0, 0.1, 0.0005),
                float_step("wavelength", 0.05, 0.005, 1.0, 0.001),
                float_step("speed", 1.0, -10.0, 10.0, 0.05),
                float("decay", 2.0, 0.0, 10.0),
                edge(1),
            ],
        ),
        simple_effect(
            "lens-distortion",
            LENS_DISTORTION,
            vec![
                // Positive is barrel distortion, negative pincushion.
                float("k1", 0.2, -1.0, 1.0),
                float("k2", 0.0, -1.0, 1.0),
                float("zoom", 1.0, 0.5, 2.0),
                edge(0),
            ],
        ),
        simple_effect(
            "polar-coordinates",
            POLAR_COORDINATES,
            vec![
                choice("mode", 0, &["rect_to_polar", "polar_to_rect"]),
                float("amount", 1.0, 0.0, 1.0),
                edge(0),
            ],
        ),
        simple_effect(
            "offset",
            OFFSET,
            vec![vec2_range("shift", [0.0, 0.0], -1.0, 1.0, 0.001), edge(2)],
        ),
        simple_effect(
            "corner-pin",
            CORNER_PIN,
            vec![
                vec2_range("top_left", [0.0, 0.0], -1.0, 2.0, 0.001),
                vec2_range("top_right", [1.0, 0.0], -1.0, 2.0, 0.001),
                vec2_range("bottom_right", [1.0, 1.0], -1.0, 2.0, 0.001),
                vec2_range("bottom_left", [0.0, 1.0], -1.0, 2.0, 0.001),
            ],
        ),
        simple_effect(
            "shake",
            SHAKE,
            vec![
                float_step("amplitude", 0.02, 0.0, 0.2, 0.0005),
                float_step("frequency", 8.0, 0.1, 30.0, 0.1),
                degrees("rotation", 0.0, 0.0, 30.0),
                float_step("zoom", 1.05, 1.0, 2.0, 0.005),
                float_step("seed", 0.0, 0.0, 1000.0, 1.0),
                edge(1),
            ],
        ),
        simple_effect(
            "turbulent-displace",
            TURBULENT_DISPLACE,
            vec![
                float_step("amount", 0.02, 0.0, 0.2, 0.0005),
                float("size", 0.2, 0.01, 2.0),
                float_step("complexity", 3.0, 1.0, 4.0, 1.0),
                // How far the noise drifts per second.
                float_step("evolution", 0.0, 0.0, 5.0, 0.01),
                float_step("seed", 0.0, 0.0, 1000.0, 1.0),
                edge(1),
            ],
        ),
        EffectDef {
            inputs: vec![InputSpec { name: "map".into() }],
            ..simple_effect(
                "displacement-map",
                DISPLACEMENT_MAP,
                vec![
                    choice("channel_x", 0, CHANNELS),
                    choice("channel_y", 1, CHANNELS),
                    float_step("amount_x", 0.02, -0.2, 0.2, 0.0005),
                    float_step("amount_y", 0.02, -0.2, 0.2, 0.0005),
                    edge(1),
                ],
            )
        },
        simple_effect(
            "kaleidoscope",
            KALEIDOSCOPE_EFFECT,
            vec![
                float_step("segments", 6.0, 2.0, 32.0, 1.0),
                degrees("angle", 0.0, -360.0, 360.0),
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                edge(3),
            ],
        ),
        simple_effect(
            "tile",
            TILE,
            vec![
                vec2_range("tiles", [2.0, 2.0], 0.1, 32.0, 0.1),
                vec2_range("offset", [0.0, 0.0], -1.0, 1.0, 0.01),
                boolean("mirror", false),
            ],
        ),
    ]
}
