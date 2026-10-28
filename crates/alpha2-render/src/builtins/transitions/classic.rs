//! The "Classic" group: cross-fades, pushes and simple reveals.

use alpha2_shaders::{
    ANGULAR, BOUNCE, CIRCLE, CIRCLE_OPEN, COVER, CROSS_WARP, DIRECTIONAL, DIRECTIONAL_EASING,
    DIRECTIONAL_SCALED, DIRECTIONAL_WIPE, DREAMY, FADE, FOLD, HSV_FADE, LEFT_RIGHT, MULTIPLY_BLEND,
    PIXELIZE, RADIAL, ROLLS, ROTATE, SLIDES, SPLIT_SLIDE_IN_HORIZONTAL,
    SPLIT_SLIDE_IN_OUT_HORIZONTAL, SPLIT_SLIDE_IN_OUT_VERTICAL, SPLIT_SLIDE_IN_VERTICAL,
    SPLIT_SLIDE_OUT_HORIZONTAL, SPLIT_SLIDE_OUT_VERTICAL, TOP_BOTTOM, UNCOVER, X_AXIS_TRANSLATION,
};

use crate::EffectDef;
use crate::builtins::{
    boolean, choice, color, degrees, float, float_step, original_transition, transition, vec2_range,
};

// One table of data, in catalog order.
#[allow(clippy::too_many_lines)]
pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition("fade", FADE, "fade.glsl", "gre", Vec::new()),
        transition(
            "directional",
            DIRECTIONAL,
            "Directional.glsl",
            "Gaëtan Renaudeau",
            vec![vec2_range("direction", [0.0, 1.0], -1.0, 1.0, 0.1)],
        ),
        transition(
            "circle-open",
            CIRCLE_OPEN,
            "circleopen.glsl",
            "gre",
            vec![float("smoothness", 0.3, 0.0, 1.0), boolean("opening", true)],
        ),
        transition(
            "angular",
            ANGULAR,
            "angular.glsl",
            "Fernando Kuteken",
            vec![degrees("starting_angle", 90.0, -360.0, 360.0)],
        ),
        transition(
            "pixelize",
            PIXELIZE,
            "pixelize.glsl",
            "gre",
            vec![
                vec2_range("squares_min", [20.0, 20.0], 1.0, 100.0, 1.0),
                float("steps", 50.0, 0.0, 200.0),
            ],
        ),
        transition(
            "directional-wipe",
            DIRECTIONAL_WIPE,
            "directionalwipe.glsl",
            "gre",
            vec![
                vec2_range("direction", [1.0, -1.0], -1.0, 1.0, 0.1),
                float("smoothness", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "circle",
            CIRCLE,
            "circle.glsl",
            "Fernando Kuteken",
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                color("back_color", [0.1, 0.1, 0.1, 1.0]),
            ],
        ),
        transition(
            "cross-warp",
            CROSS_WARP,
            "crosswarp.glsl",
            "Eke Péter",
            Vec::new(),
        ),
        transition("dreamy", DREAMY, "Dreamy.glsl", "mikolalysenko", Vec::new()),
        transition(
            "radial",
            RADIAL,
            "Radial.glsl",
            "Xaychru",
            vec![float("smoothness", 1.0, 0.0001, 2.0)],
        ),
        transition(
            "bounce",
            BOUNCE,
            "Bounce.glsl",
            "Adrian Purser",
            vec![
                color("shadow_color", [0.0, 0.0, 0.0, 0.6]),
                float("shadow_height", 0.075, 0.001, 0.5),
                float_step("bounces", 3.0, 0.0, 10.0, 0.5),
            ],
        ),
        transition(
            "directional-scaled",
            DIRECTIONAL_SCALED,
            "DirectionalScaled.glsl",
            "Thibaut Foussard",
            vec![
                vec2_range("direction", [0.0, 1.0], -1.0, 1.0, 0.1),
                float("scale", 0.7, 0.1, 2.0),
            ],
        ),
        transition(
            "slides",
            SLIDES,
            "Slides.glsl",
            "Mark Craig",
            vec![
                choice(
                    "origin",
                    0,
                    &[
                        "top",
                        "right",
                        "bottom",
                        "left",
                        "top_right",
                        "bottom_right",
                        "bottom_left",
                        "top_left",
                        "center",
                    ],
                ),
                boolean("slide_in", false),
            ],
        ),
        transition("fold", FOLD, "Fold.glsl", "nwoeanhinnogaehr", Vec::new()),
        transition(
            "directional-easing",
            DIRECTIONAL_EASING,
            "directional-easing.glsl",
            "Max Plotnikov",
            vec![vec2_range("direction", [0.0, 1.0], -1.0, 1.0, 0.1)],
        ),
        transition(
            "hsv-fade",
            HSV_FADE,
            "HSVfade.glsl",
            "nwoeanhinnogaehr",
            Vec::new(),
        ),
        transition(
            "left-right",
            LEFT_RIGHT,
            "LeftRight.glsl",
            "zhmy",
            vec![color("back_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        transition(
            "top-bottom",
            TOP_BOTTOM,
            "TopBottom.glsl",
            "zhmy",
            vec![color("back_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        transition(
            "multiply-blend",
            MULTIPLY_BLEND,
            "multiply_blend.glsl",
            "Fernando Kuteken",
            Vec::new(),
        ),
        transition(
            "rolls",
            ROLLS,
            "Rolls.glsl",
            "Mark Craig",
            vec![
                choice(
                    "corner",
                    0,
                    &["bottom_right", "bottom_left", "top_left", "top_right"],
                ),
                boolean("rot_down", false),
            ],
        ),
        split_slide(
            "split-slide-in-horizontal",
            SPLIT_SLIDE_IN_HORIZONTAL,
            "splitSlideInHorizontal.glsl",
        ),
        split_slide(
            "split-slide-in-vertical",
            SPLIT_SLIDE_IN_VERTICAL,
            "splitSlideInVertical.glsl",
        ),
        split_slide(
            "split-slide-out-horizontal",
            SPLIT_SLIDE_OUT_HORIZONTAL,
            "splitSlideOutHorizontal.glsl",
        ),
        split_slide(
            "split-slide-out-vertical",
            SPLIT_SLIDE_OUT_VERTICAL,
            "splitSlideOutVertical.glsl",
        ),
        split_slide(
            "split-slide-in-out-horizontal",
            SPLIT_SLIDE_IN_OUT_HORIZONTAL,
            "splitSlideInOutHorizontal.glsl",
        ),
        split_slide(
            "split-slide-in-out-vertical",
            SPLIT_SLIDE_IN_OUT_VERTICAL,
            "splitSlideInOutVertical.glsl",
        ),
        transition(
            "rotate",
            ROTATE,
            "rotateTransition.glsl",
            "haiyoucuv",
            Vec::new(),
        ),
        transition(
            "x-axis-translation",
            X_AXIS_TRANSLATION,
            "x_axis_translation.glsl",
            "lizhongjian",
            Vec::new(),
        ),
        original_transition(
            "cover",
            COVER,
            vec![choice("direction", 0, &["left", "right", "up", "down"])],
        ),
        original_transition(
            "uncover",
            UNCOVER,
            vec![choice("direction", 0, &["left", "right", "up", "down"])],
        ),
    ]
}

/// One of the six split slides, all by one author.
fn split_slide(name: &str, body: &str, source: &str) -> EffectDef {
    transition(
        name,
        body,
        source,
        "OllyOllyOlly",
        vec![boolean("reversed", false)],
    )
}
