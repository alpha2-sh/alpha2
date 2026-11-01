//! Deterministic vector geometry for alpha2 shapes.
//!
//! A shape content tree evaluated at one time ([`EvaluatedItem`]) becomes a [`VectorScene`] of
//! fills and stroke outlines in device pixels, in drawing order, with their group layers.
//! [`raster::rasterize`] turns a path into tile coverage, and queries (bounds, hit tests,
//! lengths, trims) share the same code.
//!
//! Results are bit-identical on every target: geometry is `f64`, trigonometry uses `libm`, arc
//! lengths and flattening use only `+ - * /` and `sqrt`, and coverage accumulates in scalar `f32`
//! in a fixed order. If another crate enables kurbo's `std`, stroke outlines and scene keys may
//! differ in the last bits, so shared hash goldens compare coverage only.

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

/// The canvas size in device pixels. Normalized `(1, 1)` maps to `(width, height)`, and
/// normalized lengths are fractions of the shorter side.
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

    /// The pixel length of a normalized length (a fraction of the shorter side).
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
