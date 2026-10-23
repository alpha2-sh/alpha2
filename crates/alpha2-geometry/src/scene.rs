//! The vector scene: what a content tree draws at one time and size, as
//! fills in device pixels in drawing order, back first.

use crate::StableHash;
use alpha2_model::FillRule;
use kurbo::{Affine, BezPath};
use std::sync::Arc;

/// A half-open rectangle of device pixels.
#[derive(Copy, Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct IRect {
    /// Left edge.
    pub x0: i32,
    /// Top edge.
    pub y0: i32,
    /// Right edge, exclusive.
    pub x1: i32,
    /// Bottom edge, exclusive.
    pub y1: i32,
}

impl IRect {
    /// A rectangle from its edges.
    #[must_use]
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// Width, zero when empty.
    #[must_use]
    pub fn width(self) -> u32 {
        u32::try_from(self.x1.saturating_sub(self.x0).max(0)).unwrap_or(0)
    }

    /// Height, zero when empty.
    #[must_use]
    pub fn height(self) -> u32 {
        u32::try_from(self.y1.saturating_sub(self.y0).max(0)).unwrap_or(0)
    }

    /// Whether it holds no pixel.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    /// The pixels in both.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// The smallest rectangle holding both; an empty one adds nothing.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// The pixels `path`'s control points reach, a pixel past them for
    /// antialiasing, within `canvas`.
    #[must_use]
    pub fn covering(path: &BezPath, canvas: Self) -> Self {
        let bounds = kurbo::Shape::bounding_box(path);
        if !(bounds.x0.is_finite()
            && bounds.y0.is_finite()
            && bounds.x1.is_finite()
            && bounds.y1.is_finite())
        {
            return Self::default();
        }
        // Clamped to the canvas first, so the conversion cannot overflow.
        #[allow(clippy::cast_possible_truncation)]
        let edge =
            |value: f64, low: i32, high: i32| value.clamp(f64::from(low), f64::from(high)) as i32;
        Self {
            x0: edge((bounds.x0 - 1.0).floor(), canvas.x0, canvas.x1),
            y0: edge((bounds.y0 - 1.0).floor(), canvas.y0, canvas.y1),
            x1: edge((bounds.x1 + 1.0).ceil(), canvas.x0, canvas.x1),
            y1: edge((bounds.y1 + 1.0).ceil(), canvas.y0, canvas.y1),
        }
    }
}

/// What kind of paint covers a fill.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum PaintKind {
    /// One color.
    Solid,
    /// Along the line from `a` to `b`.
    Linear,
    /// Around `a`, offset 1 at radius `b.x` (a fraction of the shorter side).
    Radial,
    /// Around `a`, from angle `b.x` radians clockwise from up.
    Conic,
}

/// One stop of a scene paint: its offset and straight, sRGB-encoded color.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SceneStop {
    /// Position along the gradient, in `0..=1`.
    pub offset: f32,
    /// Straight sRGB color and alpha.
    pub color: [f32; 4],
}

/// The paint of a fill, as the generator pass reads a paint: points in the
/// normalized coordinates of the paint's space, and the transform from
/// device pixels into that space's pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct ScenePaint {
    /// What kind of paint.
    pub kind: PaintKind,
    /// First point: a linear gradient's start, another gradient's centre.
    pub a: [f32; 2],
    /// Second point: a linear gradient's end, a radial gradient's radius
    /// in `b[0]`, a conic gradient's start angle in radians in `b[0]`.
    pub b: [f32; 2],
    /// 0 pad, 1 repeat, 2 reflect.
    pub spread: u32,
    /// One stop for a solid color, two to eight for a gradient.
    pub stops: Vec<SceneStop>,
    /// Device pixels to the paint space's pixels: the inverse of the
    /// transforms of the groups the paint is in.
    pub inverse: Affine,
}

/// One drawing operation of a [`VectorScene`].
#[derive(Clone, Debug, PartialEq)]
pub enum VectorOp {
    /// Draws what follows, until the matching [`VectorOp::PopLayer`], into a
    /// layer composited at `opacity`, within `bounds`.
    PushLayer {
        /// The layer's opacity, in `0..1`.
        opacity: f32,
        /// The device pixels its contents reach.
        bounds: IRect,
    },
    /// Ends the innermost layer.
    PopLayer,
    /// Covers the inside of `path` with `paint`. A stroke is a fill of its
    /// outline with the nonzero rule.
    Fill {
        /// The outline, in device pixels.
        path: Arc<BezPath>,
        /// Which regions are inside.
        rule: FillRule,
        /// What covers it.
        paint: ScenePaint,
        /// Opacity, in `0..=1`.
        opacity: f32,
        /// The device pixels it can cover.
        bounds: IRect,
    },
}

/// Everything a content tree draws at one time and size.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorScene {
    /// The operations, back first.
    pub ops: Vec<VectorOp>,
    /// The device pixels any of them can cover.
    pub bounds: IRect,
    /// The canvas it is drawn on.
    pub width: u32,
    /// The canvas it is drawn on.
    pub height: u32,
    /// A key that changes with any of the above: equal keys draw equal
    /// pictures.
    pub key: u128,
}

impl VectorScene {
    /// A scene of `ops` on a `width` by `height` canvas, its bounds and key
    /// computed from them.
    #[must_use]
    pub fn new(ops: Vec<VectorOp>, width: u32, height: u32) -> Self {
        let mut hash = StableHash::new();
        hash.bytes(b"vector-scene");
        hash.u32(width);
        hash.u32(height);
        let mut bounds = IRect::default();
        for op in &ops {
            match op {
                VectorOp::PushLayer {
                    opacity,
                    bounds: layer,
                } => {
                    hash.u32(1);
                    hash.f32(*opacity);
                    hash_rect(&mut hash, *layer);
                }
                VectorOp::PopLayer => hash.u32(2),
                VectorOp::Fill {
                    path,
                    rule,
                    paint,
                    opacity,
                    bounds: covered,
                } => {
                    hash.u32(3);
                    hash.path(path);
                    hash.u32(match rule {
                        FillRule::Nonzero => 0,
                        FillRule::Evenodd => 1,
                    });
                    hash_paint(&mut hash, paint);
                    hash.f32(*opacity);
                    hash_rect(&mut hash, *covered);
                    bounds = bounds.union(*covered);
                }
            }
        }
        Self {
            ops,
            bounds,
            width,
            height,
            key: hash.finish(),
        }
    }

    /// The fills it draws.
    #[must_use]
    pub fn fill_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op, VectorOp::Fill { .. }))
            .count()
    }
}

fn hash_rect(hash: &mut StableHash, rect: IRect) {
    for edge in [rect.x0, rect.y0, rect.x1, rect.y1] {
        hash.u32(edge.cast_unsigned());
    }
}

fn hash_paint(hash: &mut StableHash, paint: &ScenePaint) {
    hash.u32(paint.kind as u32);
    for value in paint.a.iter().chain(&paint.b) {
        hash.f32(*value);
    }
    hash.u32(paint.spread);
    hash.u64(paint.stops.len() as u64);
    for stop in &paint.stops {
        hash.f32(stop.offset);
        for channel in stop.color {
            hash.f32(channel);
        }
    }
    for coefficient in paint.inverse.as_coeffs() {
        hash.f64(coefficient);
    }
}
