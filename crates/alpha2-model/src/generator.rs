//! Sources the engine draws itself: fills, gradients, shapes, and text.

use crate::{
    GeneratorIssue, LayoutNode, MAX_TEXT_CONTENT_BYTES, MAX_TEXT_GENERATOR_FONT_SIZE, ShapeItem,
    TextStyle, Vec2,
};
use serde::{Deserialize, Serialize};

/// Most stops in one gradient.
pub const MAX_GRADIENT_STOPS: usize = 8;

/// A straight (not premultiplied), sRGB-encoded color with alpha, each
/// component in `0..=1`.
///
/// Written as the object, or as CSS hex: `"#rgb"`, `"#rgba"`, `"#rrggbb"` or
/// `"#rrggbbaa"`, any case, each digit pair a component times 255. It is
/// written back as the object: a component need not be a multiple of 1/255.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema",
    schemars(rename = "Rgba", transform = crate::schema::with_hex_form)
)]
#[serde(remote = "Self", deny_unknown_fields)]
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

    /// The color a CSS hex string names, if it is one.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#')?.as_bytes();
        let nibble = |index: usize| char::from(digits[index]).to_digit(16);
        let component = |value: u32| u8::try_from(value).ok().map(|byte| f32::from(byte) / 255.0);
        let channels = match digits.len() {
            3 | 4 => (0..digits.len())
                .map(|index| component(nibble(index)? * 17))
                .collect::<Option<Vec<_>>>()?,
            6 | 8 => (0..digits.len() / 2)
                .map(|index| component(nibble(index * 2)? * 16 + nibble(index * 2 + 1)?))
                .collect::<Option<Vec<_>>>()?,
            _ => return None,
        };
        Some(Self::new(
            channels[0],
            channels[1],
            channels[2],
            channels.get(3).copied().unwrap_or(1.0),
        ))
    }
}

/// Reads a hex string or the object.
struct RgbaVisitor;

impl<'de> serde::de::Visitor<'de> for RgbaVisitor {
    type Value = Rgba;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a color: the {red, green, blue, alpha} object or a CSS hex string")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Rgba, E> {
        Rgba::from_hex(value).ok_or_else(|| {
            E::custom(format!(
                "invalid color \"{value}\": write #rgb, #rgba, #rrggbb, or #rrggbbaa"
            ))
        })
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Rgba, A::Error> {
        Rgba::deserialize(serde::de::value::MapAccessDeserializer::new(map))
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RgbaVisitor)
    }
}

impl Serialize for Rgba {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}

/// The picture of a generated source.
///
/// A generated picture has the canvas's size and aspect, like a source that
/// fills the frame at scale 1. Positions are in its normalized space, `(0, 0)`
/// top-left to `(1, 1)` bottom-right; lengths (radii, stroke widths) are
/// fractions of its shorter side, so the picture is the same at any render
/// resolution. A fill or text does not change over time (a clip animates it
/// with its transform, effects, and masks); shapes and layouts may.
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
    /// Boxes laid out by CSS flexbox around their text and shapes. Its
    /// picture changes over time when any value in the tree is keyframed,
    /// in the picture's time. The engine lays it out with the fonts it
    /// draws with. See [`LayoutNode`].
    Layout {
        /// The root box: the whole picture.
        root: Box<LayoutNode>,
    },
}

/// A color or gradient.
///
/// Gradients interpolate between stops premultiplied, in linear light: the
/// engine's working space, so a gradient between two colors passes through
/// the physical mix of them, and a stop fading to transparent does not darken
/// its neighbour.
///
/// A color (hex or object) is read as a solid paint of it; a paint is
/// written back tagged.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema",
    schemars(rename = "Paint", transform = crate::schema::with_color_form)
)]
#[serde(
    remote = "Self",
    tag = "type",
    rename_all = "snake_case",
    deny_unknown_fields
)]
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

impl<'de> Deserialize<'de> for Paint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        // Which form it is shows only in whether it has a tag.
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("type").is_some() {
            Self::deserialize(value).map_err(D::Error::custom)
        } else {
            <Rgba as Deserialize>::deserialize(value)
                .map(|color| Self::Solid { color })
                .map_err(D::Error::custom)
        }
    }
}

impl Serialize for Paint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
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

    /// Checks every field. A shapes or layout generator's problem names
    /// where in its tree it lies; another generator's is
    /// [`GeneratorIssue::Invalid`]. A layout's pixel font sizes are not
    /// held to the limit here: that takes the canvas ([`Self::validate_on`]).
    ///
    /// # Errors
    /// Returns the first problem, if any.
    pub fn validate(&self) -> Result<(), GeneratorIssue> {
        self.validate_canvas(None)
    }

    /// [`Self::validate`] for a generator drawn on a `width` by `height`
    /// canvas, which a layout's pixel lengths are pixels of.
    ///
    /// # Errors
    /// Returns the first problem, if any.
    pub fn validate_on(&self, width: u32, height: u32) -> Result<(), GeneratorIssue> {
        self.validate_canvas(Some((width, height)))
    }

    fn validate_canvas(&self, canvas: Option<(u32, u32)>) -> Result<(), GeneratorIssue> {
        let valid = match self {
            Self::Fill { paint } => paint.is_valid(),
            Self::Shapes { items } => {
                return crate::shapes::validate_shape_items(items).map_err(GeneratorIssue::Shape);
            }
            Self::Layout { root } => return crate::layout::validate_layout(root, canvas),
            Self::Text { text, style } => {
                !text.is_empty()
                    && text.len() <= MAX_TEXT_CONTENT_BYTES
                    && style.is_valid(MAX_TEXT_GENERATOR_FONT_SIZE)
            }
        };
        valid.then_some(()).ok_or(GeneratorIssue::Invalid)
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
