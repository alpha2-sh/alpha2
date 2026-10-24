//! Deterministic vector geometry for alpha2 shapes.
//!
//! A shapes generator's content tree, evaluated at one time
//! ([`EvaluatedItem`]), becomes a [`VectorScene`]: fills and stroke outlines
//! in device pixels, in drawing order, with the group layers that fade them.
//! [`raster::rasterize`] turns one of its paths into tile coverage. Queries
//! the editor needs (bounds, hit tests, lengths, trims) use the same code.
//!
//! Every result is the same bits on every target. Geometry is `f64`,
//! trigonometry is `libm`'s, arc lengths and flattening use only `+ - * /`
//! and `sqrt`, and coverage is accumulated in scalar `f32` in a fixed order.
//! Stroke expansion is kurbo's, and its transcendental calls are `libm`'s
//! too, unless another crate in the build turns on kurbo's `std`: then
//! outlines and scene keys can differ in the last bits, and the shared hash
//! goldens compare coverage only.

pub mod affine;
pub mod arclen;
mod far;
mod hash;
#[doc(hidden)]
pub mod parity;
pub mod query;
pub mod raster;
mod scene;
pub mod shapes;
pub mod spatial;
mod stroke;
mod tree;
pub mod trim;

#[cfg(test)]
mod tests;

pub use far::{MAX_DEVICE, project_far};
pub use hash::StableHash;
pub use kurbo;
pub use scene::{IRect, PaintKind, ScenePaint, SceneStop, VectorOp, VectorScene};
pub use stroke::{StrokeStyle, stroke_outline};
pub use tree::{
    EvaluatedGeometry, EvaluatedItem, EvaluatedPath, EvaluatedSubpath, EvaluatedTransform,
    EvaluatedVertex, ItemPath, build_scene, build_scene_in, build_scene_with_items, item_paths,
    scene_paint,
};

/// The size, in device pixels, of the picture a content tree is drawn on:
/// normalized `(1, 1)` is `(width, height)`, and lengths are fractions of
/// the shorter side.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct CanvasSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl CanvasSize {
    /// A canvas of `width` by `height` pixels.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// The scale from normalized coordinates to pixels, per axis.
    #[must_use]
    pub fn scale(self) -> kurbo::Vec2 {
        kurbo::Vec2::new(f64::from(self.width), f64::from(self.height))
    }

    /// The pixel length of a normalized length: a fraction of the shorter
    /// side.
    #[must_use]
    pub fn unit(self) -> f64 {
        f64::from(self.width.min(self.height))
    }

    /// The pixel point of a normalized point.
    #[must_use]
    pub fn point(self, x: f64, y: f64) -> kurbo::Point {
        kurbo::Point::new(x * f64::from(self.width), y * f64::from(self.height))
    }
}
