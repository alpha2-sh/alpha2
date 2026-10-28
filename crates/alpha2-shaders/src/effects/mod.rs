//! File-backed WGSL bodies for the built-in effects and the internal passes
//! that draw them, one module per catalog group. [`EFFECT_HELPERS`] is
//! prepended to every one of them.

mod blur;
mod color;
mod distort;
mod keying;
mod stylize;

pub use blur::*;
pub use color::*;
pub use distort::*;
pub use keying::*;
pub use stylize::*;

/// Color, noise and coordinate helpers shared by the built-in effect bodies.
pub const EFFECT_HELPERS: &str = include_str!("../../shaders/runtime/effects/helpers.wgsl");
/// Body of a public effect the render graph expands into internal passes.
pub const EXPANDED_EFFECT: &str = include_str!("../../shaders/runtime/effects/expanded.wgsl");
