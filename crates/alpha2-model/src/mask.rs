//! Shape masks and track mattes: what limits where a clip is visible.

use crate::{Animated, ClipId, Vec2};
use serde::{Deserialize, Serialize};

/// Most shape masks on one clip.
pub const MAX_CLIP_MASKS: usize = 8;

/// A shape that limits where a clip is visible.
///
/// Geometry is in the clip's source space, before its transform, so the mask
/// moves with the clip: `(0, 0)` is the source's top-left corner and `(1, 1)`
/// its bottom-right. Distances (`feather`, a rectangle's `corner_radius`) are
/// fractions of that space's shorter side, so a mask covers the same part of
/// the picture at any render resolution.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ShapeMask {
    /// The shape's outline.
    pub shape: MaskShape,
    /// Centre of the shape.
    pub center: Animated<Vec2>,
    /// Full width and height of the shape, before rotation. Non-negative.
    pub size: Animated<Vec2>,
    /// Clockwise rotation about the centre, in degrees.
    #[serde(default = "zero")]
    pub rotation_degrees: Animated<f32>,
    /// Width of the soft edge, centred on the outline. Non-negative; zero is
    /// a one-pixel antialiased edge.
    #[serde(default = "zero")]
    pub feather: Animated<f32>,
    /// Whether the mask keeps the outside of the shape instead of its inside.
    #[serde(default)]
    pub invert: bool,
    /// How this mask combines with the ones before it.
    #[serde(default)]
    pub mode: MaskMode,
}

fn zero() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
}

/// The outline of a [`ShapeMask`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", from = "MaskShapeWire")]
pub enum MaskShape {
    /// A rectangle, optionally with rounded corners.
    Rectangle {
        /// Corner radius, as a fraction of the shorter side of source
        /// space. Non-negative; a radius past half the rectangle is capped.
        corner_radius: Animated<f32>,
    },
    /// The ellipse inscribed in the size.
    Ellipse,
}

/// The wire form of [`MaskShape`]: an empty struct variant rejects extra
/// fields where serde's unit variant would ignore them.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "MaskShape"))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MaskShapeWire {
    /// A rectangle, optionally with rounded corners.
    Rectangle {
        /// Corner radius, as a fraction of the shorter side of source
        /// space. Non-negative; a radius past half the rectangle is capped.
        #[serde(default = "zero")]
        corner_radius: Animated<f32>,
    },
    /// The ellipse inscribed in the size.
    Ellipse {},
}

impl From<MaskShapeWire> for MaskShape {
    fn from(wire: MaskShapeWire) -> Self {
        match wire {
            MaskShapeWire::Rectangle { corner_radius } => Self::Rectangle { corner_radius },
            MaskShapeWire::Ellipse {} => Self::Ellipse,
        }
    }
}

/// How a mask combines with the coverage of the masks before it.
///
/// Coverage starts empty when the first mask adds, and full otherwise, so a
/// clip whose only mask subtracts is visible outside the shape.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MaskMode {
    /// Union: `c + m - c * m`.
    #[default]
    Add,
    /// Difference: `c * (1 - m)`.
    Subtract,
    /// Intersection: `c * m`.
    Intersect,
}

/// Another visual clip whose picture sets where this clip is visible.
///
/// The matte clip is drawn, with its own transform and effects, only into the
/// matte; a clip that is any clip's matte is never composited itself. Where
/// the matte clip is not active, its coverage is zero.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Matte {
    /// The clip whose picture is the matte.
    pub source: ClipId,
    /// What of that picture is the coverage.
    pub mode: MatteMode,
}

/// Which property of the matte clip's picture is coverage.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MatteMode {
    /// Its alpha.
    Alpha,
    /// One minus its alpha.
    AlphaInverted,
    /// Its luminance over black: BT.709 weights on linear light, times alpha.
    Luma,
    /// One minus that luminance.
    LumaInverted,
}

impl ShapeMask {
    pub(crate) fn is_valid(&self) -> bool {
        let non_negative = |animation: &Animated<f32>| {
            crate::animation::valid_scalar_values(animation, |value| value >= 0.0)
        };
        let sizes = crate::animation::valid_vec2_values(&self.size, |value| {
            value.x >= 0.0 && value.y >= 0.0
        });
        let shape = match &self.shape {
            MaskShape::Rectangle { corner_radius } => non_negative(corner_radius),
            MaskShape::Ellipse => true,
        };
        shape
            && sizes
            && crate::animation::valid_vec2_values(&self.center, |_| true)
            && crate::animation::valid_scalar_values(&self.rotation_degrees, |_| true)
            && non_negative(&self.feather)
    }
}
