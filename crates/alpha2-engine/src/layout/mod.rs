//! Layout generators: box trees laid out by CSS flexbox and grid with the
//! fonts the engine draws with (`alpha2_layout`), then drawn from vector
//! fills and text textures.
//!
//! A layout is resolved where text is prepared ([`crate::TextTextures`]),
//! which holds the fonts, so its text is measured and drawn by the same
//! code.

pub(crate) mod draw;
pub use draw::PreparedLayout;
pub(crate) mod paint;

use alpha2_render::RenderSize;

/// The shorter side of `size` in pixels: what a layout's lengths are
/// fractions of.
pub(crate) fn unit_of(size: RenderSize) -> f32 {
    crate::text::picture(size).unit()
}
