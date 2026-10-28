//! The "Wipes" group: an edge sweeping one picture off the other.

use alpha2_shaders::{
    GRADIENT_WIPE, HEART, HORIZONTAL_CLOSE, HORIZONTAL_OPEN, PINWHEEL, SCREEN_MELT, SQUARES_WIRE,
    STATIC_WIPE, VERTICAL_CLOSE, VERTICAL_OPEN, WIND, WINDOW_BLINDS, WINDOW_SLICE, WIPE_DOWN,
    WIPE_LEFT, WIPE_RIGHT, WIPE_UP, ZOOM_LEFT_WIPE, ZOOM_RIGHT_WIPE,
};

use crate::EffectDef;
use crate::builtins::{
    boolean, choice, degrees, float, float_step, original_transition, transition, vec2_range,
};

#[allow(clippy::too_many_lines)]
pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition("wipe-up", WIPE_UP, "wipeUp.glsl", "Jake Nelson", Vec::new()),
        transition(
            "wipe-down",
            WIPE_DOWN,
            "wipeDown.glsl",
            "Jake Nelson",
            Vec::new(),
        ),
        transition(
            "wipe-left",
            WIPE_LEFT,
            "wipeLeft.glsl",
            "Jake Nelson",
            Vec::new(),
        ),
        transition(
            "wipe-right",
            WIPE_RIGHT,
            "wipeRight.glsl",
            "Jake Nelson",
            Vec::new(),
        ),
        transition(
            "window-blinds",
            WINDOW_BLINDS,
            "windowblinds.glsl",
            "Fabien Benetou",
            Vec::new(),
        ),
        transition(
            "window-slice",
            WINDOW_SLICE,
            "windowslice.glsl",
            "gre",
            vec![
                float("count", 10.0, 1.0, 100.0),
                float("smoothness", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "wind",
            WIND,
            "wind.glsl",
            "gre",
            vec![float("size", 0.2, 0.0001, 1.0), boolean("reversed", false)],
        ),
        transition(
            "pinwheel",
            PINWHEEL,
            "pinwheel.glsl",
            "Mr Speaker",
            vec![float("speed", 2.0, 0.0, 20.0)],
        ),
        transition("heart", HEART, "heart.glsl", "gre", Vec::new()),
        transition(
            "horizontal-open",
            HORIZONTAL_OPEN,
            "HorizontalOpen.glsl",
            "martiniti",
            Vec::new(),
        ),
        transition(
            "horizontal-close",
            HORIZONTAL_CLOSE,
            "HorizontalClose.glsl",
            "martiniti",
            Vec::new(),
        ),
        transition(
            "vertical-open",
            VERTICAL_OPEN,
            "VerticalOpen.glsl",
            "martiniti",
            Vec::new(),
        ),
        transition(
            "vertical-close",
            VERTICAL_CLOSE,
            "VerticalClose.glsl",
            "martiniti",
            Vec::new(),
        ),
        transition(
            "screen-melt",
            SCREEN_MELT,
            "DoomScreenTransition.glsl",
            "Zeh Fernando",
            vec![
                float_step("bars", 30.0, 2.0, 200.0, 1.0),
                float("amplitude", 2.0, 0.0, 10.0),
                float("noise", 0.1, 0.0, 1.0),
                float("frequency", 0.5, 0.0, 10.0),
                float("drip_scale", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "squares-wire",
            SQUARES_WIRE,
            "squareswire.glsl",
            "gre",
            vec![
                vec2_range("squares", [10.0, 10.0], 1.0, 100.0, 1.0),
                vec2_range("direction", [1.0, -0.5], -1.0, 1.0, 0.1),
                float("smoothness", 1.6, 0.0, 5.0),
            ],
        ),
        transition(
            "static-wipe",
            STATIC_WIPE,
            "static_wipe.glsl",
            "Ben Lucas",
            vec![
                boolean("up_to_down", true),
                float("max_static_span", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "zoom-left-wipe",
            ZOOM_LEFT_WIPE,
            "ZoomLeftWipe.glsl",
            "Handk",
            vec![float("zoom_quickness", 0.5, 0.01, 0.5)],
        ),
        transition(
            "zoom-right-wipe",
            ZOOM_RIGHT_WIPE,
            "ZoomRigthWipe.glsl",
            "Handk",
            vec![float("zoom_quickness", 0.5, 0.01, 0.5)],
        ),
        original_transition(
            "gradient-wipe",
            GRADIENT_WIPE,
            vec![
                choice(
                    "pattern",
                    0,
                    &["linear", "radial", "conic", "diamond", "noise"],
                ),
                degrees("angle", 0.0, -360.0, 360.0),
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("softness", 0.1, 0.0, 1.0),
                float_step("noise_scale", 0.1, 0.01, 1.0, 0.005),
                float_step("seed", 0.0, 0.0, 1000.0, 1.0),
                boolean("reversed", false),
            ],
        ),
    ]
}
