//! The "Keying" group: effects that make parts of a picture transparent.

use alpha2_shaders::{ALPHA_ADJUST, LUMA_KEY, MATTE_CHOKER, SET_MATTE, SPILL_SUPPRESS};

use crate::builtins::{boolean, choice, chroma_key_definition, float, float_step, simple_effect};
use crate::{EffectDef, InputSpec};

pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        chroma_key_definition(),
        simple_effect(
            "luma-key",
            LUMA_KEY,
            vec![
                choice("key_out", 0, &["dark", "bright"]),
                float("threshold", 0.1, 0.0, 1.0),
                float("softness", 0.1, 0.0, 1.0),
            ],
        ),
        simple_effect(
            "spill-suppress",
            SPILL_SUPPRESS,
            vec![
                choice("screen", 0, &["green", "blue"]),
                float("amount", 1.0, 0.0, 1.0),
            ],
        ),
        simple_effect(
            "alpha-adjust",
            ALPHA_ADJUST,
            vec![
                float("black", 0.0, 0.0, 1.0),
                float("white", 1.0, 0.0, 1.0),
                float("gamma", 1.0, 0.1, 10.0),
                boolean("invert", false),
            ],
        ),
        // A positive choke erodes the matte, a negative one dilates it; the radius is in
        // output heights.
        simple_effect(
            "matte-choker",
            MATTE_CHOKER,
            vec![
                float("choke", 0.5, -1.0, 1.0),
                float_step("radius", 0.003, 0.0, 0.03, 0.0005),
            ],
        ),
        set_matte_definition(),
    ]
}

/// The source kept where its `matte` input covers it, by the matte's alpha
/// or its luminance, as a track matte keeps it.
fn set_matte_definition() -> EffectDef {
    EffectDef {
        inputs: vec![InputSpec {
            name: "matte".into(),
        }],
        ..simple_effect(
            "set-matte",
            SET_MATTE,
            vec![
                choice("mode", 0, &["alpha", "luma"]),
                boolean("invert", false),
            ],
        )
    }
}
