//! Sources the engine draws itself: fills, gradients, shapes, and text.

use crate::{
    MAX_TEXT_CONTENT_BYTES, MAX_TEXT_GENERATOR_FONT_SIZE, ShapeIssue, ShapeItem, TextStyle, Vec2,
};
use serde::{Deserialize, Serialize};

/// Most stops in one gradient.
pub const MAX_GRADIENT_STOPS: usize = 8;

/// A straight (not premultiplied), sRGB-encoded color with alpha, each
/// component in `0..=1`.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Rgba {
    /// Red component.
    pub red: f32,
    /// Green component.
    pub green: f32,
    /// Blue component.
    pub blue: f32,
    /// Coverage.
    pub alpha: f32,
}

impl Rgba {
    /// Creates a color from sRGB-encoded components and alpha.
    #[must_use]
    pub const fn new(red: f32, green: f32, blue: f32, alpha: f32) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    pub(crate) fn is_valid(self) -> bool {
        [self.red, self.green, self.blue, self.alpha]
            .iter()
            .all(|value| (0.0..=1.0).contains(value))
    }
}

/// The picture of a generated source.
///
/// A generated picture has the canvas's size and aspect, like a source that
/// fills the frame at scale 1. Positions are in its normalized space, `(0, 0)`
/// top-left to `(1, 1)` bottom-right; lengths (radii, stroke widths) are
/// fractions of its shorter side, so the picture is the same at any render
/// resolution. A fill or text does not change over time (a clip animates it
/// with its transform, effects, and masks); shapes may.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Generator {
    /// Paint over the whole picture.
    Fill {
        /// What covers the picture.
        paint: Paint,
    },
    /// Shapes drawn from an ordered content tree, as an After Effects shape
    /// layer or a Lottie shape layer draws them. Unlike the other
    /// generators, its picture changes over time: every value in the tree
    /// may be keyframed, in the picture's time.
    Shapes {
        /// The content tree, front first. See [`ShapeItem`].
        items: Vec<ShapeItem>,
    },
    /// Text on a transparent picture, laid out and placed by its style as a
    /// caption track's cues are: its size and wrapping width fractions of
    /// the canvas, its anchor at `style.position`. The engine draws it with
    /// the fonts its host registered.
    Text {
        /// The text; `\n` starts a line. Non-empty, at most
        /// [`MAX_TEXT_CONTENT_BYTES`] bytes.
        text: String,
        /// How it looks and where it lies. The font size is at most
        /// [`MAX_TEXT_GENERATOR_FONT_SIZE`].
        #[serde(default)]
        style: TextStyle,
    },
}

/// A color or gradient.
///
/// Gradients interpolate between stops premultiplied, in linear light: the
/// engine's working space, so a gradient between two colors passes through
/// the physical mix of them, and a stop fading to transparent does not darken
/// its neighbour.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Paint {
    /// One color.
    Solid {
        /// The color.
        color: Rgba,
    },
    /// Colors along the line from `start` to `end`, constant across it.
    LinearGradient {
        /// Where offset 0 lies.
        start: Vec2,
        /// Where offset 1 lies. Distinct from `start`.
        end: Vec2,
        /// Two to [`MAX_GRADIENT_STOPS`] stops.
        stops: Vec<GradientStop>,
        /// What lies beyond the first and last offsets.
        #[serde(default)]
        spread: Spread,
    },
    /// Colors along the radius of a circle.
    RadialGradient {
        /// Where offset 0 lies.
        center: Vec2,
        /// The distance of offset 1 from the centre, as a fraction of the
        /// picture's shorter side. Positive.
        radius: f32,
        /// Two to [`MAX_GRADIENT_STOPS`] stops.
        stops: Vec<GradientStop>,
        /// What lies beyond the first and last offsets.
        #[serde(default)]
        spread: Spread,
    },
    /// Colors around a point: offsets 0 to 1 are one clockwise turn.
    ConicGradient {
        /// The point the gradient turns around.
        center: Vec2,
        /// Where offset 0 lies, in degrees clockwise from straight up.
        #[serde(default)]
        angle_degrees: f32,
        /// Two to [`MAX_GRADIENT_STOPS`] stops.
        stops: Vec<GradientStop>,
    },
}

/// One color of a gradient.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GradientStop {
    /// Position along the gradient, in `0..=1`, not before the previous stop.
    pub offset: f32,
    /// Color at that position.
    pub color: Rgba,
}

/// What a linear or radial gradient shows outside its first and last stops.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Spread {
    /// The end colors continue.
    #[default]
    Pad,
    /// The gradient starts again.
    Repeat,
    /// The gradient runs back and forth.
    Reflect,
}

impl Generator {
    /// Whether every field is in range, as a project needs it to be.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Checks every field. A shapes generator's problem names where in its
    /// tree it lies; another generator's is `None`.
    ///
    /// # Errors
    /// Returns the first problem, if any.
    pub fn validate(&self) -> Result<(), Option<ShapeIssue>> {
        let valid = match self {
            Self::Fill { paint } => paint.is_valid(),
            Self::Shapes { items } => {
                return crate::shapes::validate_shape_items(items).map_err(Some);
            }
            Self::Text { text, style } => {
                !text.is_empty()
                    && text.len() <= MAX_TEXT_CONTENT_BYTES
                    && style.is_valid(MAX_TEXT_GENERATOR_FONT_SIZE)
            }
        };
        valid.then_some(()).ok_or(None)
    }
}

impl Paint {
    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Solid { color } => color.is_valid(),
            Self::LinearGradient {
                start, end, stops, ..
            } => finite(*start) && finite(*end) && start != end && valid_stops(stops),
            Self::RadialGradient {
                center,
                radius,
                stops,
                ..
            } => finite(*center) && radius.is_finite() && *radius > 0.0 && valid_stops(stops),
            Self::ConicGradient {
                center,
                angle_degrees,
                stops,
            } => finite(*center) && angle_degrees.is_finite() && valid_stops(stops),
        }
    }
}

fn finite(value: Vec2) -> bool {
    value.x.is_finite() && value.y.is_finite()
}

fn valid_stops(stops: &[GradientStop]) -> bool {
    (2..=MAX_GRADIENT_STOPS).contains(&stops.len())
        && stops
            .iter()
            .all(|stop| (0.0..=1.0).contains(&stop.offset) && stop.color.is_valid())
        && stops
            .windows(2)
            .all(|pair| pair[0].offset <= pair[1].offset)
}
