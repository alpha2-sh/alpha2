//! Bodies of the built-in transitions in the "3D" catalog group.

/// Rotating cube transition body, with a floor reflection.
pub const CUBE: &str = include_str!("../../shaders/runtime/transitions/cube.wgsl");
/// Doors opening onto the incoming picture transition body.
pub const DOORWAY: &str = include_str!("../../shaders/runtime/transitions/doorway.wgsl");
/// Two cards swapping places transition body.
pub const SWAP: &str = include_str!("../../shaders/runtime/transitions/swap.wgsl");
/// Grid of flipping tiles transition body.
pub const GRID_FLIP: &str = include_str!("../../shaders/runtime/transitions/grid-flip.wgsl");
/// Diagonal wave of flipping tiles transition body.
pub const TILES_WAVE: &str = include_str!("../../shaders/runtime/transitions/tiles-wave.wgsl");
/// Book page turn transition body.
pub const BOOK_FLIP: &str = include_str!("../../shaders/runtime/transitions/book-flip.wgsl");
/// Flat card flip transition body.
pub const SIMPLE_FLIP: &str = include_str!("../../shaders/runtime/transitions/simple-flip.wgsl");
/// Page curling away transition body.
pub const PAGE_PEEL: &str = include_str!("../../shaders/runtime/transitions/page-peel.wgsl");
