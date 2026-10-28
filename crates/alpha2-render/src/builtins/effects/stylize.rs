//! The "Stylize" group: looks laid over a picture.

use alpha2_shaders::{
    CRT, EDGE_DETECT, EMBOSS, EXPANDED_EFFECT, FILM_GRAIN, GLITCH_EFFECT, HALFTONE, LETTERBOX,
    LIGHT_LEAK, OIL_PAINT, OUTLINE, RGB_SPLIT, VHS, VIGNETTE,
};

use crate::EffectDef;
use crate::builtins::{
    DROP_SHADOW_ID, boolean, choice, color, degrees, float, float_step, glow_definition,
    simple_effect, vec2_range,
};

pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        simple_effect(
            "vignette",
            VIGNETTE,
            vec![
                float("amount", 0.45, 0.0, 1.0),
                float("radius", 0.72, 0.0, 1.5),
                float("softness", 0.45, 0.01, 1.0),
            ],
        ),
        glow_definition(),
        simple_effect(
            "film-grain",
            FILM_GRAIN,
            vec![
                float("amount", 0.18, 0.0, 1.0),
                float("size", 1.4, 0.5, 6.0),
            ],
        ),
        simple_effect(
            "rgb-split",
            RGB_SPLIT,
            vec![
                choice("mode", 0, &["linear", "radial"]),
                float_step("amount", 0.005, 0.0, 0.05, 0.0005),
                degrees("angle", 0.0, -360.0, 360.0),
            ],
        ),
        // `speed` is new patterns a second; `color_split` a fraction of the height.
        simple_effect(
            "glitch",
            GLITCH_EFFECT,
            vec![
                float("amount", 0.5, 0.0, 1.0),
                float_step("block_size", 0.05, 0.005, 0.5, 0.005),
                float_step("speed", 10.0, 0.0, 60.0, 1.0),
                float_step("color_split", 0.01, 0.0, 0.05, 0.0005),
                float_step("seed", 0.0, 0.0, 1000.0, 1.0),
            ],
        ),
        crt_definition(),
        vhs_definition(),
        // `cells` across the width, as pixelate's `blocks`.
        simple_effect(
            "halftone",
            HALFTONE,
            vec![
                choice("mode", 0, &["mono", "color"]),
                float_step("cells", 80.0, 10.0, 400.0, 1.0),
                degrees("angle", 45.0, -90.0, 90.0),
            ],
        ),
        simple_effect(
            "emboss",
            EMBOSS,
            vec![
                degrees("angle", 135.0, -360.0, 360.0),
                float_step("distance", 0.0015, 0.0, 0.01, 0.0001),
                float("strength", 1.0, 0.0, 5.0),
                boolean("keep_color", false),
            ],
        ),
        simple_effect(
            "edge-detect",
            EDGE_DETECT,
            vec![
                float_step("width", 0.001, 0.0002, 0.01, 0.0001),
                float("strength", 1.0, 0.0, 5.0),
                boolean("invert", false),
                boolean("colored", false),
            ],
        ),
        drop_shadow_definition(),
        simple_effect(
            "outline",
            OUTLINE,
            vec![
                color("color", [1.0, 1.0, 1.0, 1.0]),
                float_step("width", 0.004, 0.0, 0.03, 0.0005),
            ],
        ),
        simple_effect(
            "light-leak",
            LIGHT_LEAK,
            vec![
                color("color", [1.0, 0.55, 0.2, 1.0]),
                float("intensity", 0.6, 0.0, 2.0),
                vec2_range("position", [0.15, 0.2], -0.5, 1.5, 0.01),
                float("size", 0.6, 0.05, 2.0),
                float_step("speed", 0.2, 0.0, 5.0, 0.01),
                float_step("seed", 0.0, 0.0, 1000.0, 1.0),
            ],
        ),
        letterbox_definition(),
        simple_effect(
            "oil-paint",
            OIL_PAINT,
            vec![float_step("radius", 0.004, 0.0, 0.015, 0.0005)],
        ),
    ]
}

/// A curved screen with scanlines, an aperture mask and darkened corners.
///
/// The mask's stripes are one output pixel wide on purpose, not a fraction
/// of the height as other distances are: a preview drawn at half size
/// shows them at another density than the export.
fn crt_definition() -> EffectDef {
    simple_effect(
        "crt",
        CRT,
        vec![
            float("curvature", 0.1, 0.0, 0.5),
            float("scanlines", 0.35, 0.0, 1.0),
            float_step("line_count", 240.0, 50.0, 1080.0, 1.0),
            float("mask", 0.2, 0.0, 1.0),
            float("vignette", 0.3, 0.0, 1.0),
        ],
    )
}

/// Worn tape: wobbling rows, a rolling tracking band, chroma bleed and
/// grain, moving at `speed`.
///
/// The bleed's reach and the grain are in output pixels on purpose, as a
/// tape's horizontal bandwidth is, not fractions of the height: a preview
/// drawn at half size bleeds twice as far relative to the picture.
fn vhs_definition() -> EffectDef {
    simple_effect(
        "vhs",
        VHS,
        vec![
            float("amount", 0.5, 0.0, 1.0),
            float("noise", 0.15, 0.0, 1.0),
            float("bleed", 0.5, 0.0, 1.0),
            float_step("speed", 1.0, 0.0, 10.0, 0.05),
            float_step("seed", 0.0, 0.0, 1000.0, 1.0),
        ],
    )
}

/// Returns the data-only definition of the built-in drop shadow.
///
/// The source's alpha, moved `distance` (a fraction of the height) along
/// `angle` (counter-clockwise, -45 down and to the right), in `color` at
/// `opacity`, blurred like the Gaussian blur with a `softness` radius and
/// drawn under the source, or alone with `shadow_only`. It is drawn on the
/// canvas before the clip's transform, so a rotated clip turns its shadow
/// with it. A render graph draws it as internal passes; its own body is
/// never drawn.
pub(crate) fn drop_shadow_definition() -> EffectDef {
    EffectDef {
        id: DROP_SHADOW_ID.into(),
        ..simple_effect(
            "drop-shadow",
            EXPANDED_EFFECT,
            vec![
                color("color", [0.0, 0.0, 0.0, 1.0]),
                float("opacity", 0.6, 0.0, 1.0),
                degrees("angle", -45.0, -360.0, 360.0),
                float_step("distance", 0.01, 0.0, 0.2, 0.0005),
                float_step("softness", 0.01, 0.0, 0.2, 0.0005),
                boolean("shadow_only", false),
            ],
        )
    }
}

/// Opaque bars around a window of `aspect`, width over height.
///
/// It runs before the clip's transform, so a clip scaled down shrinks its
/// bars with it; bars over the whole program belong on the top-level
/// composition.
fn letterbox_definition() -> EffectDef {
    simple_effect(
        "letterbox",
        LETTERBOX,
        vec![
            float_step("aspect", 2.39, 1.0, 4.0, 0.01),
            color("color", [0.0, 0.0, 0.0, 1.0]),
        ],
    )
}
