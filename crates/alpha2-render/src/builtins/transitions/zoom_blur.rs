//! The "Zoom & Blur" group: zooms, spins and blurs between the pictures.

use alpha2_shaders::{
    DEFOCUS_BLUR, DREAMY_ZOOM, LINEAR_BLUR, MOSAIC_TILES, REVOLVE, ROTATE_SCALE_FADE,
    ROTATE_SCALE_VANISH, SCALE_IN, SIMPLE_ZOOM_OUT, WHIP_PAN, ZOOM_BLUR, ZOOM_IN_CIRCLES,
    ZOOM_IN_OUT,
};

use crate::EffectDef;
use crate::builtins::{
    boolean, choice, color, degrees, float, float_step, original_transition, transition, vec2_range,
};

// A table of data, one entry per transition.
#[allow(clippy::too_many_lines)]
pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition(
            "defocus-blur",
            DEFOCUS_BLUR,
            "DefocusBlur.glsl",
            "Sergey Kosarevsky",
            vec![float("blur_size", 0.02, 0.0, 0.2)],
        ),
        transition(
            "dreamy-zoom",
            DREAMY_ZOOM,
            "DreamyZoom.glsl",
            "Zeh Fernando",
            vec![
                degrees("rotation", 13.5, -180.0, 180.0),
                float("scale", 1.2, 0.1, 5.0),
            ],
        ),
        transition(
            "revolve",
            REVOLVE,
            "Revolve_Left.glsl",
            "bread",
            vec![
                vec2_range("center", [0.46, 0.52], 0.0, 1.0, 0.01),
                float("direction", -1.0, -1.0, 1.0),
                // Upstream's 1.95 and 2.85 radians.
                degrees("max_rotation", 111.727, 0.0, 360.0),
                degrees("swirl", 163.293, 0.0, 720.0),
                float("peak_zoom", 2.22, 0.1, 5.0),
                float("barrel", 0.38, 0.0, 2.0),
                float("motion_blur", 1.0, 0.0, 3.0),
                float("switch_start", 0.3, 0.0, 1.0),
                float("switch_end", 0.5, 0.0, 1.0),
                float("shadow", 0.16, 0.0, 1.0),
            ],
        ),
        transition(
            "linear-blur",
            LINEAR_BLUR,
            "LinearBlur.glsl",
            "gre",
            vec![float("intensity", 0.1, 0.0, 1.0)],
        ),
        transition(
            "mosaic-tiles",
            MOSAIC_TILES,
            "Mosaic.glsl",
            "Xaychru",
            vec![
                float_step("end_x", 2.0, -5.0, 5.0, 1.0),
                float_step("end_y", -1.0, -5.0, 5.0, 1.0),
            ],
        ),
        transition(
            "rotate-scale-fade",
            ROTATE_SCALE_FADE,
            "rotate_scale_fade.glsl",
            "Fernando Kuteken",
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("rotations", 1.0, -5.0, 5.0),
                float("scale", 8.0, 0.1, 20.0),
                color("back_color", [0.15, 0.15, 0.15, 1.0]),
            ],
        ),
        transition(
            "rotate-scale-vanish",
            ROTATE_SCALE_VANISH,
            "RotateScaleVanish.glsl",
            "Mark Craig",
            vec![
                boolean("fade_in_second", true),
                boolean("reverse_effect", false),
                boolean("reverse_rotation", false),
                color("back_color", [0.0, 0.0, 0.0, 1.0]),
            ],
        ),
        transition(
            "scale-in",
            SCALE_IN,
            "scale-in.glsl",
            "haiyoucuv",
            Vec::new(),
        ),
        transition(
            "simple-zoom-out",
            SIMPLE_ZOOM_OUT,
            "SimpleZoomOut.glsl",
            "Tianshuo",
            vec![
                float("zoom_quickness", 0.8, 0.2, 1.0),
                boolean("fade", true),
            ],
        ),
        transition(
            "zoom-in-circles",
            ZOOM_IN_CIRCLES,
            "ZoomInCircles.glsl",
            "dycm8009",
            Vec::new(),
        ),
        transition(
            "zoom-in-out",
            ZOOM_IN_OUT,
            "zoomInOut.glsl",
            "OllyOllyOlly",
            Vec::new(),
        ),
        original_transition(
            "zoom-blur",
            ZOOM_BLUR,
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("strength", 0.4, 0.0, 1.0),
            ],
        ),
        original_transition(
            "whip-pan",
            WHIP_PAN,
            vec![
                choice("direction", 0, &["left", "right", "up", "down"]),
                float("blur", 0.5, 0.0, 1.0),
            ],
        ),
    ]
}
