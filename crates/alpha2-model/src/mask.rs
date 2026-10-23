//! Shape masks and track mattes: what limits where a clip is visible.

use crate::{Animated, ClipId, FillRule, ShapeGeometry, ShapeIssue, ShapeProblem};
use serde::{Deserialize, Serialize};

/// Most shape masks on one clip.
pub const MAX_CLIP_MASKS: usize = 8;

/// A shape that limits where a clip is visible.
///
/// Geometry is in the clip's source space, before its transform, so the mask
/// moves with the clip: `(0, 0)` is the source's top-left corner and `(1, 1)`
/// its bottom-right. Distances (`feather`, a rectangle's `corner_radius`, a
/// polystar's radii) are fractions of that space's shorter side, so a mask
/// covers the same part of the picture at any render resolution.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ShapeMask {
    /// The shape's outline, as a shapes generator's geometry.
    pub shape: ShapeGeometry,
    /// How soft the edge is. Non-negative; zero is the outline's
    /// antialiased edge. The coverage is blurred by a Gaussian of standard
    /// deviation `0.3125 * feather`, which spreads its 10 to 90 percent ramp
    /// over `0.8 * feather` across the outline.
    #[serde(default = "zero")]
    pub feather: Animated<f32>,
    /// Whether the mask keeps the outside of the shape instead of its inside.
    #[serde(default)]
    pub invert: bool,
    /// How this mask combines with the ones before it.
    #[serde(default)]
    pub mode: MaskMode,
    /// Which regions of the outline are inside it.
    #[serde(default)]
    pub fill_rule: FillRule,
}

fn zero() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
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
    /// Checks the outline and feather; `at` names the mask in the issue.
    pub(crate) fn validate(&self, at: &str) -> Result<(), ShapeIssue> {
        self.shape.validate(&format!("{at}.shape"))?;
        if crate::animation::valid_scalar_values(&self.feather, |value| value >= 0.0) {
            Ok(())
        } else {
            Err(ShapeIssue {
                at: format!("{at}.feather"),
                problem: ShapeProblem::OutOfRange,
            })
        }
    }
}

/// Checks each of `masks`, naming the first problem by its index.
pub(crate) fn validate_masks(masks: &[ShapeMask]) -> Result<(), ShapeIssue> {
    for (index, mask) in masks.iter().enumerate() {
        mask.validate(&format!("masks[{index}]"))?;
    }
    Ok(())
}
