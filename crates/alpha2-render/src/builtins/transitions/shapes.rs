//! The "Shapes" group: the incoming picture revealed through a growing shape.

use alpha2_shaders::{
    BOW_TIE, BOW_TIE_HORIZONTAL, BOW_TIE_VERTICAL, BOX, CHESSBOARD, FRAGMENT, POLAR_FUNCTION,
    RECTANGLE, RECTANGLE_CROP, SHAPE_WIPE, STAR_WIPE,
};

use crate::EffectDef;
use crate::builtins::{
    boolean, choice, color, degrees, float, float_step, original_transition, transition, vec2_range,
};

pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition(
            "box",
            BOX,
            "Box.glsl",
            "lql",
            vec![
                boolean("rect_in", true),
                // Corners in the picture as shown: left_top is the top-left one.
                choice(
                    "location",
                    0,
                    &[
                        "center",
                        "left_top",
                        "left_bottom",
                        "right_top",
                        "right_bottom",
                    ],
                ),
            ],
        ),
        transition(
            "chessboard",
            CHESSBOARD,
            "chessboard.glsl",
            "lql",
            vec![float_step("grid_num", 10.0, 1.0, 50.0, 1.0)],
        ),
        transition(
            "bow-tie",
            BOW_TIE,
            "BowTieWithParameter.glsl",
            "KMojek",
            vec![float("adjust", 0.5, 0.0, 1.0), boolean("reversed", false)],
        ),
        transition(
            "bow-tie-horizontal",
            BOW_TIE_HORIZONTAL,
            "BowTieHorizontal.glsl",
            "huynx",
            Vec::new(),
        ),
        transition(
            "bow-tie-vertical",
            BOW_TIE_VERTICAL,
            "BowTieVertical.glsl",
            "huynx",
            Vec::new(),
        ),
        transition("fragment", FRAGMENT, "fragment.glsl", "lbl", Vec::new()),
        transition(
            "polar-function",
            POLAR_FUNCTION,
            "polar_function.glsl",
            "Fernando Kuteken",
            vec![float_step("segments", 5.0, 1.0, 20.0, 1.0)],
        ),
        transition(
            "rectangle",
            RECTANGLE,
            "Rectangle.glsl",
            "martiniti",
            vec![color("bg_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        transition(
            "rectangle-crop",
            RECTANGLE_CROP,
            "RectangleCrop.glsl",
            "martiniti",
            vec![color("bg_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        transition(
            "star-wipe",
            STAR_WIPE,
            "StarWipe.glsl",
            "Ben Lucas",
            vec![
                float("border_thickness", 0.01, 0.0, 0.1),
                // A fraction of the 72 degrees between two points.
                float("star_rotation", 0.75, -1.0, 1.0),
                color("border_color", [1.0, 1.0, 1.0, 1.0]),
                vec2_range("star_center", [0.5, 0.5], 0.0, 1.0, 0.01),
            ],
        ),
        original_transition(
            "shape-wipe",
            SHAPE_WIPE,
            vec![
                choice(
                    "shape",
                    0,
                    &["circle", "ellipse", "diamond", "rectangle", "star", "cross"],
                ),
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                float("aspect", 1.0, 0.1, 10.0),
                degrees("rotation", 0.0, -360.0, 360.0),
                float("softness", 0.0, 0.0, 0.5),
                boolean("closing", false),
            ],
        ),
    ]
}
