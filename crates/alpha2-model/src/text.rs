//! Styled text: what caption tracks and text generators draw.

use crate::{Paint, Rgba, Vec2};
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

/// Most strokes around one text.
pub const MAX_TEXT_STROKES: usize = 4;

/// Widest all of a text's strokes reach together, as a fraction of the font
/// size: the sum of their widths.
pub const MAX_TEXT_STROKE_EXTENT: f32 = 0.5;

/// Most shadows under one text.
pub const MAX_TEXT_SHADOWS: usize = 4;

/// Farthest a text shadow lies from its text on each axis, as a fraction of
/// the font size.
pub const MAX_TEXT_SHADOW_OFFSET: f32 = 1.0;

/// Widest text shadow blur, as a fraction of the font size.
pub const MAX_TEXT_SHADOW_BLUR: f32 = 1.0;

/// Largest text background padding, as a fraction of the font size.
pub const MAX_TEXT_PADDING: f32 = 2.0;

/// How text looks and where it lies: a caption track's cues, or a text
/// [`crate::Generator`].
///
/// Sizes are resolution-independent: the font size is a fraction of the
/// canvas's shorter side, as every other length is, the wrapping width a
/// fraction of the canvas width, and the strokes, shadows and background
/// padding fractions of the font size, so a look follows the font size. The
/// engine draws with the fonts its host registered: those of `font_family`
/// first, then the rest in their fallback order.
///
/// The look is painted bottom to top as CSS paints text: the background's
/// boxes, the shadows (the last listed lowest), the strokes (the outermost
/// lowest), the fill, then color glyphs.
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
    /// What paints the glyphs; a color reads as a solid paint. A gradient's
    /// positions are in the text's layout box, `(0, 0)` its top-left and
    /// `(1, 1)` its bottom-right, and a radial gradient's radius is a
    /// fraction of the box's shorter side. Default solid white.
    #[serde(default = "white_paint")]
    pub fill: Paint,
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
    /// Bands around the glyphs, innermost first: each lies outside the one
    /// before it. At most [`MAX_TEXT_STROKES`], their widths summing to at
    /// most [`MAX_TEXT_STROKE_EXTENT`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strokes: Vec<TextStroke>,
    /// Shadows of the glyphs and their strokes, the first on top, as CSS
    /// `text-shadow` lists them. At most [`MAX_TEXT_SHADOWS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<TextShadow>,
    /// A box behind each line.
    #[serde(default)]
    pub background: Option<TextBackground>,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: default_font_size(),
            fill: white_paint(),
            line_height: default_line_height(),
            letter_spacing: 0.0,
            font_weight: default_font_weight(),
            align: TextAlign::default(),
            max_width: default_max_width(),
            position: default_position(),
            anchor: default_anchor(),
            strokes: Vec::new(),
            shadows: Vec::new(),
            background: None,
        }
    }
}

const fn default_font_size() -> f32 {
    0.05
}

const fn white_paint() -> Paint {
    Paint::Solid {
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
    }
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

/// A band around the glyphs, and around the strokes listed before it,
/// drawn behind them.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextStroke {
    /// Band color.
    pub color: Rgba,
    /// Band width as a fraction of the font size, positive; a text's
    /// strokes sum to at most [`MAX_TEXT_STROKE_EXTENT`].
    pub width: f32,
}

/// A shadow of a text's glyphs and strokes, behind them.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextShadow {
    /// Shadow color.
    pub color: Rgba,
    /// Offset in font sizes, +x right, +y down; each component in
    /// `[-1, 1]` ([`MAX_TEXT_SHADOW_OFFSET`]).
    #[serde(default)]
    pub offset: Vec2,
    /// Blur radius in font sizes, as CSS `text-shadow` takes it: the
    /// Gaussian's standard deviation is half of it. In `[0, 1]`
    /// ([`MAX_TEXT_SHADOW_BLUR`]).
    #[serde(default)]
    pub blur: f32,
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
            && self.fill.is_valid()
            && LINE_HEIGHT_RANGE.contains(&self.line_height)
            && LETTER_SPACING_RANGE.contains(&self.letter_spacing)
            && FONT_WEIGHT_RANGE.contains(&self.font_weight)
            && self.max_width > 0.0
            && self.max_width <= 1.0
            && finite(self.position)
            && finite(self.anchor)
            && valid_strokes(&self.strokes)
            && valid_shadows(&self.shadows)
            && self.background.is_none_or(|background| {
                background.color.is_valid()
                    && (0.0..=MAX_TEXT_PADDING).contains(&background.padding)
            })
    }
}

/// Whether `strokes` are at most [`MAX_TEXT_STROKES`] valid bands that reach
/// at most [`MAX_TEXT_STROKE_EXTENT`] together.
pub(crate) fn valid_strokes(strokes: &[TextStroke]) -> bool {
    strokes.len() <= MAX_TEXT_STROKES
        && strokes
            .iter()
            .all(|stroke| stroke.color.is_valid() && stroke.width.is_finite() && stroke.width > 0.0)
        && strokes.iter().map(|stroke| stroke.width).sum::<f32>() <= MAX_TEXT_STROKE_EXTENT
}

/// Whether `shadows` are at most [`MAX_TEXT_SHADOWS`] shadows in range.
pub(crate) fn valid_shadows(shadows: &[TextShadow]) -> bool {
    let offset = |value: f32| value.is_finite() && value.abs() <= MAX_TEXT_SHADOW_OFFSET;
    shadows.len() <= MAX_TEXT_SHADOWS
        && shadows.iter().all(|shadow| {
            shadow.color.is_valid()
                && offset(shadow.offset.x)
                && offset(shadow.offset.y)
                && (0.0..=MAX_TEXT_SHADOW_BLUR).contains(&shadow.blur)
        })
}
