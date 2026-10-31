//! Layout generators: box trees laid out by CSS flexbox and grid (`alpha2_layout`), drawn from
//! vector fills and text textures.
//!
//! Layouts resolve in [`crate::TextTextures`], which holds the fonts, so text is measured and
//! drawn by the same code.

pub(crate) mod draw;
pub use draw::PreparedLayout;
pub(crate) mod paint;

use alpha2_render::RenderSize;

/// The shorter side of `size` in pixels, the unit of layout lengths.
pub(crate) fn unit_of(size: RenderSize) -> f32 {
    crate::text::picture(size).unit()
}
