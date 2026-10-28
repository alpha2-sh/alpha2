//! File-backed WGSL bodies for the built-in transition catalog.
//! Each transition is a `.wgsl` asset under `shaders/runtime/transitions` so
//! shader authors can edit, review, and attribute shader-language source
//! without embedding it in Rust. The bodies are not standalone modules: the
//! effect registry concatenates them onto a generated per-effect preamble.
//!
//! A port of a gl-transitions shader keeps its coordinates: it defines
//! `gl_transition` over a bottom-left uv and samples with `gl_from_color` and
//! `gl_to_color`. [`GL_TRANSITION_ENTRY`] adapts that to alpha2's top-left uv,
//! so a direction or position parameter means what it means upstream, and
//! "up" is up. A transition Alpha2 wrote itself defines `transition` over the
//! top-left uv directly. Both have [`TRANSITION_HELPERS`] before them.

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

/// Alpha, boundary, hash and noise helpers shared by the built-in transition
/// bodies.
pub const TRANSITION_HELPERS: &str = include_str!("../../shaders/runtime/transitions/helpers.wgsl");
/// The entry point that runs a gl-transitions port in its bottom-left uv,
/// and the samplers in those coordinates.
pub const GL_TRANSITION_ENTRY: &str =
    include_str!("../../shaders/runtime/transitions/gl-entry.wgsl");
