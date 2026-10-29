//! Styled text: what caption tracks and text generators draw.

use crate::{Rgba, Vec2};
use serde::{Deserialize, Serialize};

/// Most bytes of text in one caption cue or text generator.
pub const MAX_TEXT_CONTENT_BYTES: usize = 4_096;

/// Most bytes in a text style's font family name.
pub const MAX_FONT_FAMILY_BYTES: usize = 256;

/// Largest font size of a text generator, as a fraction of the canvas's
/// shorter side.
pub const MAX_TEXT_GENERATOR_FONT_SIZE: f32 = 1.0;

/// The line heights text takes, as multiples of the font size.
pub const LINE_HEIGHT_RANGE: std::ops::RangeInclusive<f32> = 0.5..=3.0;

/// The letter spacings text takes, as fractions of the font size: wide
/// enough for display tracking, and no tighter than characters crossing
/// their neighbours.
pub const LETTER_SPACING_RANGE: std::ops::RangeInclusive<f32> = -0.5..=1.0;

/// The font weights text takes, as CSS `font-weight` numbers.
pub const FONT_WEIGHT_RANGE: std::ops::RangeInclusive<u16> = 1..=1000;

/// Widest text outline, as a fraction of the font size.
pub const MAX_TEXT_OUTLINE_WIDTH: f32 = 0.5;

/// Largest text background padding, as a fraction of the font size.
pub const MAX_TEXT_PADDING: f32 = 2.0;

/// How text looks and where it lies: a caption track's cues, or a text
/// [`crate::Generator`].
///
/// Sizes are resolution-independent: the font size is a fraction of the
/// canvas's shorter side, as every other length is, the wrapping width a
/// fraction of the canvas width, and the outline and background padding
/// fractions of the font size. The engine draws with the fonts its host
/// registered: those of `font_family` first, then the rest in their
/// fallback order.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextStyle {
    /// The family the text draws in, as the font's own name table gives it
    /// (typographic family, else family), matched without regard to ASCII
    /// case. Glyphs its fonts lack, and a family no registered font has,
    /// draw from the other fonts in fallback order. `None` draws from the
    /// fallback order alone. Non-empty, at most [`MAX_FONT_FAMILY_BYTES`]
    /// bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size as a fraction of the canvas's shorter side: positive, at
    /// most [`crate::MAX_CAPTION_FONT_SIZE`] on a caption track and
    /// [`MAX_TEXT_GENERATOR_FONT_SIZE`] in a text generator.
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// Text color.
    #[serde(default = "white")]
    pub color: Rgba,
    /// Baseline-to-baseline distance in font sizes, in `[0.5, 3]`. Each
    /// line's em box, 0.8 of a font size above its baseline and 0.2 below,
    /// is centred in its line when the line is taller than an em.
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// Space after each character, as a fraction of the font size, in
    /// `[-0.5, 1]`, as CSS `letter-spacing` adds it: none after a line's
    /// last character, and none after a character of a cursive script
    /// (Arabic, say), whose joins it would break. Not zero, it turns optional
    /// ligatures off.
    #[serde(default)]
    pub letter_spacing: f32,
    /// The weight, in `[1, 1000]`, a family's faces are matched by, as CSS
    /// Fonts 4 matches `font-weight`; a variable face draws at it, clamped
    /// to its `wght` axis. No face is emboldened: a weight the family lacks
    /// draws its nearest face.
    #[serde(default = "default_font_weight")]
    pub font_weight: u16,
    /// How lines align within the text.
    #[serde(default)]
    pub align: TextAlign,
    /// Width text wraps at, as a fraction of the canvas width, in `(0, 1]`.
    #[serde(default = "default_max_width")]
    pub max_width: f32,
    /// Where on the canvas the text's anchor lies, `(0, 0)` top-left to
    /// `(1, 1)` bottom-right.
    #[serde(default = "default_position")]
    pub position: Vec2,
    /// The point of the text, background included, placed at
    /// `position`: `(0.5, 1)` is its bottom centre.
    #[serde(default = "default_anchor")]
    pub anchor: Vec2,
    /// A band around the glyphs.
    #[serde(default)]
    pub outline: Option<TextOutline>,
    /// A box behind each line.
    #[serde(default)]
    pub background: Option<TextBackground>,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: default_font_size(),
            color: white(),
            line_height: default_line_height(),
            letter_spacing: 0.0,
            font_weight: default_font_weight(),
            align: TextAlign::default(),
            max_width: default_max_width(),
            position: default_position(),
            anchor: default_anchor(),
            outline: None,
            background: None,
        }
    }
}

const fn default_font_size() -> f32 {
    0.05
}

const fn white() -> Rgba {
    Rgba::new(1.0, 1.0, 1.0, 1.0)
}

const fn default_line_height() -> f32 {
    1.2
}

const fn default_font_weight() -> u16 {
    400
}

const fn default_max_width() -> f32 {
    0.9
}

const fn default_position() -> Vec2 {
    Vec2::new(0.5, 0.92)
}

const fn default_anchor() -> Vec2 {
    Vec2::new(0.5, 1.0)
}

/// How lines of text align within it.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    /// Left.
    Start,
    /// Centred.
    #[default]
    Center,
    /// Right.
    End,
}

/// A band around the glyphs, drawn behind them.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextOutline {
    /// Band color.
    pub color: Rgba,
    /// Band width as a fraction of the font size, in `(0, 0.5]`.
    pub width: f32,
}

/// A box behind each line of text.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextBackground {
    /// Box color.
    pub color: Rgba,
    /// Space between the text and the box edge, as a fraction of the font
    /// size, in `[0, 2]`.
    #[serde(default)]
    pub padding: f32,
}

impl TextStyle {
    /// Whether every field is in range, the font size at most
    /// `max_font_size`.
    pub(crate) fn is_valid(&self, max_font_size: f32) -> bool {
        let finite = |value: Vec2| value.x.is_finite() && value.y.is_finite();
        self.font_family
            .as_ref()
            .is_none_or(|family| !family.is_empty() && family.len() <= MAX_FONT_FAMILY_BYTES)
            && self.font_size > 0.0
            && self.font_size <= max_font_size
            && self.color.is_valid()
            && LINE_HEIGHT_RANGE.contains(&self.line_height)
            && LETTER_SPACING_RANGE.contains(&self.letter_spacing)
            && FONT_WEIGHT_RANGE.contains(&self.font_weight)
            && self.max_width > 0.0
            && self.max_width <= 1.0
            && finite(self.position)
            && finite(self.anchor)
            && self.outline.is_none_or(|outline| {
                outline.color.is_valid()
                    && outline.width > 0.0
                    && outline.width <= MAX_TEXT_OUTLINE_WIDTH
            })
            && self.background.is_none_or(|background| {
                background.color.is_valid()
                    && (0.0..=MAX_TEXT_PADDING).contains(&background.padding)
            })
    }
}
