//! The "Glitch" group: digital breakup and signal noise.

use alpha2_shaders::{
    DROP_ZONE_FLICKER, GLITCH, GLITCH_DISPLACE, OLD_TV_LOST_SIGNAL, PARAMETRIC_GLITCH, STATIC_FADE,
    STRIP_DATAMOSH_GLITCH, TV_STATIC,
};

use crate::EffectDef;
use crate::builtins::{float, float_step, original_transition, transition};

pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition(
            "strip-datamosh-glitch",
            STRIP_DATAMOSH_GLITCH,
            "StripDatamoshGlitch.glsl",
            "bread",
            vec![
                float("strength", 1.0, 0.0, 2.0),
                float_step("horizontal_bars", 42.0, 1.0, 200.0, 1.0),
                float_step("vertical_slits", 18.0, 1.0, 100.0, 1.0),
                float("tear", 0.18, 0.0, 1.0),
                float("chroma", 0.032, 0.0, 0.2),
                float("residue", 0.62, 0.0, 1.0),
                float("noise_amount", 0.16, 0.0, 1.0),
                float("scan_amount", 0.13, 0.0, 1.0),
                float("flash_amount", 0.2, 0.0, 1.0),
            ],
        ),
        transition(
            "drop-zone-flicker",
            DROP_ZONE_FLICKER,
            "Drop_Zone_Flicker.glsl",
            "bread",
            vec![
                float_step("frame_rate", 24.0, 1.0, 60.0, 1.0),
                float("rgb_offset", 0.014, 0.0, 0.1),
                float("block_amount", 0.72, 0.0, 1.0),
                float("ghost_amount", 0.62, 0.0, 1.0),
                float("red_cyan", 0.58, 0.0, 1.0),
                float("scanline", 0.075, 0.0, 0.5),
            ],
        ),
        transition(
            "glitch-displace",
            GLITCH_DISPLACE,
            "GlitchDisplace.glsl",
            "Matt DesLauriers",
            Vec::new(),
        ),
        transition(
            "old-tv-lost-signal",
            OLD_TV_LOST_SIGNAL,
            "old_tv_lost_signal.glsl",
            "mernking gitlab: Godswork",
            Vec::new(),
        ),
        transition(
            "static-fade",
            STATIC_FADE,
            "StaticFade.glsl",
            "Ben Lucas",
            vec![
                float_step("n_noise_pixels", 200.0, 1.0, 1000.0, 1.0),
                float("static_luminosity", 0.8, 0.0, 1.0),
            ],
        ),
        transition(
            "tv-static",
            TV_STATIC,
            "TVStatic.glsl",
            "Brandon Anzaldi",
            vec![float("offset", 0.05, 0.0, 0.5)],
        ),
        transition(
            "parametric-glitch",
            PARAMETRIC_GLITCH,
            "parametric_glitch.glsl",
            "Yoni Maltsman @friendlyspinach",
            vec![
                float("amp_x", 1.0, 0.0, 10.0),
                float("amp_y", 1.0, 0.0, 10.0),
            ],
        ),
        original_transition(
            "glitch",
            GLITCH,
            vec![
                float("amount", 1.0, 0.0, 1.0),
                float_step("block_size", 0.05, 0.005, 0.5, 0.005),
                float_step("seed", 0.0, 0.0, 1000.0, 1.0),
            ],
        ),
    ]
}
