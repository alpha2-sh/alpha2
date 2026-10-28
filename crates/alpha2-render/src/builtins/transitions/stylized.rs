//! The "Stylized" group: looks laid over a cross-fade.

use core::f32::consts::TAU;

use alpha2_shaders::{
    ADVANCED_MOSAIC, BLOCK_DISSOLVE, BURN, BURN_NOISE, BUTTERFLY_WAVE_SCRAWLER, CIRCLE_CROP,
    COLOR_PHASE, COLOUR_DISTANCE, CRAZY_PARAMETRIC_FUN, CROSSHATCH, DIRECTIONAL_WARP, EDGE,
    FADE_COLOR, FADE_GRAYSCALE, FILM_BURN, FLY_EYE, HEXAGONALIZE, KALEIDOSCOPE, LUMA_BURN,
    LUMINANCE_MELT, MORPH, OVEREXPOSURE, PERLIN, POLKA_DOTS_CURTAIN, POWER_KALEIDO, RANDOM_NOISE,
    RANDOM_SQUARES, RIPPLE, SIMPLE_ZOOM, SQUEEZE, SWIRL, UNDULATING_BURN_OUT, WATER_DROP,
};

use crate::EffectDef;
use crate::builtins::{
    boolean, choice, color, float, float_step, thresholds, transition, vec2_range,
};

// One entry per transition, in catalog order.
#[allow(clippy::too_many_lines)]
pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition(
            "color-phase",
            COLOR_PHASE,
            "colorphase.glsl",
            "gre",
            vec![
                thresholds("from_step", [0.0, 0.2, 0.4, 0.0]),
                thresholds("to_step", [0.6, 0.8, 1.0, 1.0]),
            ],
        ),
        transition(
            "fade-color",
            FADE_COLOR,
            "fadecolor.glsl",
            "gre",
            vec![
                color("color", [0.0, 0.0, 0.0, 1.0]),
                float("color_phase", 0.4, 0.0, 1.0),
            ],
        ),
        transition(
            "fade-grayscale",
            FADE_GRAYSCALE,
            "fadegrayscale.glsl",
            "gre",
            vec![float("intensity", 0.3, 0.0, 1.0)],
        ),
        transition(
            "morph",
            MORPH,
            "morph.glsl",
            "paniq",
            vec![float("strength", 0.1, 0.0, 1.0)],
        ),
        transition(
            "ripple",
            RIPPLE,
            "ripple.glsl",
            "gre",
            vec![
                float("amplitude", 100.0, 0.0, 200.0),
                float("speed", 50.0, 0.0, 100.0),
            ],
        ),
        transition(
            "squeeze",
            SQUEEZE,
            "squeeze.glsl",
            "gre",
            vec![float("color_separation", 0.04, 0.0, 0.5)],
        ),
        transition(
            "simple-zoom",
            SIMPLE_ZOOM,
            "SimpleZoom.glsl",
            "0gust1",
            vec![float("zoom_quickness", 0.8, 0.2, 1.0)],
        ),
        transition(
            "circle-crop",
            CIRCLE_CROP,
            "CircleCrop.glsl",
            "fkuteken",
            vec![color("back_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        transition(
            "colour-distance",
            COLOUR_DISTANCE,
            "ColourDistance.glsl",
            "P-Seebauer",
            vec![float("power", 5.0, 0.0, 20.0)],
        ),
        transition(
            "polka-dots-curtain",
            POLKA_DOTS_CURTAIN,
            "PolkaDotsCurtain.glsl",
            "bobylito",
            vec![
                float("dots", 20.0, 1.0, 100.0),
                vec2_range("center", [0.0, 0.0], 0.0, 1.0, 0.01),
            ],
        ),
        transition(
            "directional-warp",
            DIRECTIONAL_WARP,
            "directionalwarp.glsl",
            "pschroen",
            vec![
                float("smoothness", 0.1, 0.0001, 1.0),
                vec2_range("direction", [-1.0, 1.0], -1.0, 1.0, 0.1),
            ],
        ),
        transition(
            "block-dissolve",
            BLOCK_DISSOLVE,
            "BlockDissolve.glsl",
            "nwoeanhinnogaehr",
            vec![float_step("block_size", 0.02, 0.002, 0.5, 0.001)],
        ),
        transition(
            "random-squares",
            RANDOM_SQUARES,
            "randomsquares.glsl",
            "gre",
            vec![
                vec2_range("size", [10.0, 10.0], 1.0, 100.0, 1.0),
                float("smoothness", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "burn-noise",
            BURN_NOISE,
            "burn0.glsl",
            "liubailin2020@gmail.com",
            vec![color("burn_color", [1.0, 0.5, 0.0, 1.0])],
        ),
        transition(
            "perlin",
            PERLIN,
            "perlin.glsl",
            "Rich Harris",
            vec![
                float("scale", 4.0, 0.5, 50.0),
                float("smoothness", 0.01, 0.0, 0.5),
                float("seed", 12.9898, 0.0, 100.0),
            ],
        ),
        transition(
            "film-burn",
            FILM_BURN,
            "FilmBurn.glsl",
            "Anastasia Dunbar",
            vec![float("seed", 2.31, 0.0, 100.0)],
        ),
        transition(
            "overexposure",
            OVEREXPOSURE,
            "Overexposure.glsl",
            "Ben Zhang",
            vec![float("strength", 0.6, 0.0, 2.0)],
        ),
        transition(
            "advanced-mosaic",
            ADVANCED_MOSAIC,
            "AdvancedMosaic.glsl",
            "Sergey Kosarevsky",
            vec![float_step("pixel_size", 50.0, 1.0, 200.0, 1.0)],
        ),
        transition(
            "burn",
            BURN,
            "burn.glsl",
            "gre",
            vec![color("color", [0.9, 0.4, 0.2, 1.0])],
        ),
        transition(
            "crosshatch",
            CROSSHATCH,
            "crosshatch.glsl",
            "pthrasher",
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("threshold", 3.0, 0.1, 10.0),
                float("fade_edge", 0.1, 0.0, 0.5),
            ],
        ),
        transition(
            "butterfly-wave-scrawler",
            BUTTERFLY_WAVE_SCRAWLER,
            "ButterflyWaveScrawler.glsl",
            "mandubian",
            vec![
                float("amplitude", 1.0, 0.0, 3.0),
                float_step("waves", 30.0, 0.0, 100.0, 1.0),
                float("color_separation", 0.3, 0.0, 1.0),
            ],
        ),
        transition(
            "crazy-parametric-fun",
            CRAZY_PARAMETRIC_FUN,
            "CrazyParametricFun.glsl",
            "mandubian",
            vec![
                float("a", 4.0, 0.1, 10.0),
                float("b", 1.0, 0.1, 10.0),
                float_step("amplitude", 120.0, 0.0, 500.0, 1.0),
                float("smoothness", 0.1, 0.01, 1.0),
            ],
        ),
        transition(
            "edge",
            EDGE,
            "EdgeTransition.glsl",
            "Woohyun Kim",
            vec![
                float_step("edge_thickness", 0.001, 0.0, 0.01, 0.0001),
                float("edge_brightness", 8.0, 0.0, 20.0),
            ],
        ),
        transition(
            "fly-eye",
            FLY_EYE,
            "flyeye.glsl",
            "gre",
            vec![
                float("size", 0.04, 0.0, 0.5),
                float("zoom", 50.0, 0.0, 200.0),
                float("color_separation", 0.3, 0.0, 1.0),
            ],
        ),
        transition(
            "hexagonalize",
            HEXAGONALIZE,
            "hexagonalize.glsl",
            "Fernando Kuteken",
            vec![
                float("steps", 50.0, 0.0, 200.0),
                float("horizontal_hexagons", 20.0, 1.0, 100.0),
            ],
        ),
        transition(
            "kaleidoscope",
            KALEIDOSCOPE,
            "kaleidoscope.glsl",
            "nwoeanhinnogaehr",
            vec![
                float("speed", 1.0, 0.0, 10.0),
                float("angle", 1.0, -TAU, TAU),
                float("power", 1.5, 0.1, 10.0),
            ],
        ),
        transition(
            "luma-burn",
            LUMA_BURN,
            "dissolve.glsl",
            "hjm1fb",
            vec![
                float("line_width", 0.1, 0.0, 1.0),
                color("spread_color", [1.0, 0.0, 0.0, 1.0]),
                color("hot_color", [0.9, 0.9, 0.2, 1.0]),
                float("power", 5.0, 0.1, 10.0),
                float("intensity", 1.0, 0.0, 5.0),
            ],
        ),
        transition(
            "luminance-melt",
            LUMINANCE_MELT,
            "luminance_melt.glsl",
            "0gust1",
            vec![
                choice("direction", 1, &["up", "down"]),
                float("l_threshold", 0.8, 0.0, 1.0),
                boolean("above", false),
            ],
        ),
        transition(
            "power-kaleido",
            POWER_KALEIDO,
            "powerKaleido.glsl",
            "Boundless",
            vec![
                float("scale", 2.0, 0.1, 10.0),
                float("z", 1.5, 0.1, 10.0),
                float("speed", 5.0, 0.0, 20.0),
            ],
        ),
        transition(
            "random-noise",
            RANDOM_NOISE,
            "randomNoisex.glsl",
            "towrabbit",
            Vec::new(),
        ),
        transition(
            "swirl",
            SWIRL,
            "Swirl.glsl",
            "Sergey Kosarevsky",
            Vec::new(),
        ),
        transition(
            "undulating-burn-out",
            UNDULATING_BURN_OUT,
            "undulatingBurnOut.glsl",
            "pthrasher",
            vec![
                float("smoothness", 0.03, 0.0, 0.5),
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                color("color", [0.0, 0.0, 0.0, 1.0]),
            ],
        ),
        transition(
            "water-drop",
            WATER_DROP,
            "WaterDrop.glsl",
            "Pawe\u{142} P\u{142}\u{f3}ciennik",
            vec![
                float("amplitude", 30.0, 0.0, 100.0),
                float("speed", 30.0, 0.0, 100.0),
            ],
        ),
    ]
}
