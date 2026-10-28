//! The "3D" group: pictures moved as if in space.

use alpha2_shaders::{
    BOOK_FLIP, CUBE, DOORWAY, GRID_FLIP, PAGE_PEEL, SIMPLE_FLIP, SWAP, TILES_WAVE,
};

use crate::EffectDef;
use crate::builtins::{
    boolean, color, degrees, float, float_step, original_transition, transition, vec2_range,
};

pub(in crate::builtins) fn definitions() -> Vec<EffectDef> {
    vec![
        transition(
            "cube",
            CUBE,
            "cube.glsl",
            "gre",
            vec![
                float("persp", 0.7, 0.0, 1.0),
                float("unzoom", 0.3, 0.0, 1.0),
                float("reflection", 0.4, 0.0, 1.0),
                float_step("floating", 3.0, 0.0, 100.0, 1.0),
                color("back_color", [0.0, 0.0, 0.0, 1.0]),
            ],
        ),
        transition(
            "doorway",
            DOORWAY,
            "doorway.glsl",
            "gre",
            vec![
                float("reflection", 0.4, 0.0, 1.0),
                float("perspective", 0.4, 0.0, 2.0),
                float("depth", 3.0, 1.0, 10.0),
                color("back_color", [0.0, 0.0, 0.0, 1.0]),
            ],
        ),
        transition(
            "swap",
            SWAP,
            "swap.glsl",
            "gre",
            vec![
                float("reflection", 0.4, 0.0, 1.0),
                float("perspective", 0.2, 0.0, 0.3),
                float("depth", 3.0, 1.0, 3.0),
                color("back_color", [0.0, 0.0, 0.0, 1.0]),
            ],
        ),
        transition(
            "grid-flip",
            GRID_FLIP,
            "GridFlip.glsl",
            "TimDonselaar",
            vec![
                vec2_range("size", [4.0, 4.0], 1.0, 20.0, 1.0),
                float("pause", 0.1, 0.01, 0.45),
                float("divider_width", 0.05, 0.0, 0.5),
                color("bg_color", [0.0, 0.0, 0.0, 1.0]),
                float("randomness", 0.1, 0.0, 1.0),
            ],
        ),
        transition(
            "tiles-wave",
            TILES_WAVE,
            "TilesWave.glsl",
            "numb3r23",
            vec![
                vec2_range("tile_count", [8.0, 8.0], 1.0, 50.0, 1.0),
                boolean("flip_x", true),
                boolean("flip_y", false),
            ],
        ),
        transition("book-flip", BOOK_FLIP, "BookFlip.glsl", "hong", Vec::new()),
        transition(
            "simple-flip",
            SIMPLE_FLIP,
            "SimpleFlip.glsl",
            "nwoeanhinnogaehr",
            vec![color("back_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        original_transition(
            "page-peel",
            PAGE_PEEL,
            vec![
                degrees("angle", 135.0, -360.0, 360.0),
                float_step("radius", 0.1, 0.01, 0.5, 0.005),
                float("back_shade", 0.6, 0.0, 1.0),
            ],
        ),
    ]
}
