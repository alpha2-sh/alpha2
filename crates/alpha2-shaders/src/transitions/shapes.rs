//! Bodies of the built-in transitions in the "Shapes" catalog group.

/// Box opening from the center or a corner, or closing onto it, transition body.
pub const BOX: &str = include_str!("../../shaders/runtime/transitions/box.wgsl");
/// Checkerboard wipe transition body.
pub const CHESSBOARD: &str = include_str!("../../shaders/runtime/transitions/chessboard.wgsl");
/// Bow-tie wipe with an adjustable tip transition body.
pub const BOW_TIE: &str = include_str!("../../shaders/runtime/transitions/bow-tie.wgsl");
/// Horizontal bow-tie wipe transition body.
pub const BOW_TIE_HORIZONTAL: &str =
    include_str!("../../shaders/runtime/transitions/bow-tie-horizontal.wgsl");
/// Vertical bow-tie wipe transition body.
pub const BOW_TIE_VERTICAL: &str =
    include_str!("../../shaders/runtime/transitions/bow-tie-vertical.wgsl");
/// Voronoi shards flying away transition body.
pub const FRAGMENT: &str = include_str!("../../shaders/runtime/transitions/fragment.wgsl");
/// Flower-shaped iris transition body.
pub const POLAR_FUNCTION: &str =
    include_str!("../../shaders/runtime/transitions/polar-function.wgsl");
/// Box of a background color growing and shrinking transition body.
pub const RECTANGLE: &str = include_str!("../../shaders/runtime/transitions/rectangle.wgsl");
/// Box crop through a background color transition body.
pub const RECTANGLE_CROP: &str =
    include_str!("../../shaders/runtime/transitions/rectangle-crop.wgsl");
/// Bordered star wipe transition body.
pub const STAR_WIPE: &str = include_str!("../../shaders/runtime/transitions/star-wipe.wgsl");
/// Iris wipe through one of several shapes transition body.
pub const SHAPE_WIPE: &str = include_str!("../../shaders/runtime/transitions/shape-wipe.wgsl");
