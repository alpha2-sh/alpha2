//! WGSL bodies for the built-in transitions, appended to a generated per-effect preamble.
//!
//! gl-transitions ports keep upstream coordinates: they define `gl_transition` over a bottom-left
//! uv and sample with `gl_from_color` and `gl_to_color`, and [`GL_TRANSITION_ENTRY`] adapts them
//! so parameters mean what they mean upstream. Native transitions define `transition` over the
//! top-left uv. Both get [`TRANSITION_HELPERS`] first.

mod classic;
mod glitch;
mod shapes;
mod stylized;
mod three_d;
mod wipes;
mod zoom_blur;

pub use classic::*;
pub use glitch::*;
pub use shapes::*;
pub use stylized::*;
pub use three_d::*;
pub use wipes::*;
pub use zoom_blur::*;

/// Alpha, boundary, hash and noise helpers shared by the transition bodies.
pub const TRANSITION_HELPERS: &str = include_str!("../../shaders/runtime/transitions/helpers.wgsl");
/// The entry point and samplers that run a gl-transitions port in its bottom-left uv.
pub const GL_TRANSITION_ENTRY: &str =
    include_str!("../../shaders/runtime/transitions/gl-entry.wgsl");
