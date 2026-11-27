//! Text content as runs, and text animators: per-character motion and color held by a text
//! style.
//!
//! A run's style changes its layout as CSS inline boxes do; animators never change layout. An
//! animator gives each grapheme cluster a selection in `[0, 1]` and moves, scales,
//! turns, fades or recolors it in proportion, as After Effects' text animators do.

use std::borrow::Cow;

use alpha2_time::MediaTime;
use serde::{Deserialize, Serialize};

use crate::animation::{valid_keyframes, valid_scalar_values};
use crate::{Animated, FontStyle, Paint, Rgba, TextStroke, Vec2};

/// Most runs one text holds.
pub const MAX_TEXT_RUNS: usize = 512;

/// Most classes one run carries.
pub const MAX_RUN_CLASSES: usize = 8;

/// Most bytes of one class name.
pub const MAX_CLASS_NAME_BYTES: usize = 64;

/// Most animators one text style holds.
pub const MAX_TEXT_ANIMATORS: usize = 8;

/// How far a Range selector in index units reaches past either end: `[-N, 2N]` units for
/// `N` of this.
pub const MAX_RANGE_INDEX: f32 = 4_096.0;

/// The range a percent Range selector's start, end and offset take.
pub const RANGE_PERCENT: std::ops::RangeInclusive<f32> = -100.0..=200.0;

/// Largest magnitude of a text animator's offset on each axis, in font sizes.
pub const MAX_ANIMATOR_OFFSET: f32 = 1_024.0;
/// The range an animator's scale takes on each axis.
pub const ANIMATOR_SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.0..=10.0;
/// Widest feather of a text reveal's edge, in font sizes.
pub const MAX_REVEAL_FEATHER: f32 = 1.0;

/// A text: one string, or runs whose texts joined are the text.
///
/// Written as `"Hello"` or `[{ "text": "Hel" }, { "text": "lo", "class": ["key"] }]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum TextContent {
    /// One string; `\n` starts a line.
    Plain(String),
    /// Runs in order; their texts joined are the text.
    Runs(#[cfg_attr(feature = "schema", schemars(length(max = MAX_TEXT_RUNS)))] Vec<TextRun>),
}

// Read by the JSON type rather than as an untagged enum, which would buffer the whole value and
// hide why runs were refused.
impl<'de> Deserialize<'de> for TextContent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Content;

        impl<'de> serde::de::Visitor<'de> for Content {
            type Value = TextContent;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or a sequence of runs")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<TextContent, E> {
                Ok(TextContent::Plain(text.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, text: String) -> Result<TextContent, E> {
                Ok(TextContent::Plain(text))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<TextContent, A::Error> {
                crate::bounded::seq::<_, _, MAX_TEXT_RUNS>(
                    serde::de::value::SeqAccessDeserializer::new(seq),
                )
                .map(TextContent::Runs)
            }
        }

        deserializer.deserialize_any(Content)
    }
}

impl Default for TextContent {
    fn default() -> Self {
        Self::Plain(String::new())
    }
}

impl From<String> for TextContent {
    fn from(text: String) -> Self {
        Self::Plain(text)
    }
}

impl From<&str> for TextContent {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }
}

/// Compares the whole text, runs joined, with a string.
impl PartialEq<str> for TextContent {
    fn eq(&self, other: &str) -> bool {
        self.text() == other
    }
}

impl PartialEq<&str> for TextContent {
    fn eq(&self, other: &&str) -> bool {
        self.text() == *other
    }
}

impl TextContent {
    /// The whole text: the string, or the runs' texts joined.
    #[must_use]
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Self::Plain(text) => Cow::Borrowed(text),
            Self::Runs(runs) => Cow::Owned(runs.iter().map(|run| run.text.as_str()).collect()),
        }
    }

    /// The runs, none for a plain string.
    #[must_use]
    pub fn runs(&self) -> &[TextRun] {
        match self {
            Self::Plain(_) => &[],
            Self::Runs(runs) => runs,
        }
    }

    /// Bytes of the whole text.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Plain(text) => text.len(),
            Self::Runs(runs) => runs.iter().map(|run| run.text.len()).sum(),
        }
    }

    /// Whether the whole text is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the text is non-empty and within `max_bytes`, has at most [`MAX_TEXT_RUNS`] runs,
    /// and every run has non-empty text, at most [`MAX_RUN_CLASSES`] valid class names, and a
    /// time span that starts at or after zero and before it ends.
    pub(crate) fn is_valid(&self, max_bytes: usize) -> bool {
        !self.is_empty()
            && self.len() <= max_bytes
            && self.runs().len() <= MAX_TEXT_RUNS
            && self.runs().iter().all(TextRun::is_valid)
    }

    /// Whether any run has a style.
    #[must_use]
    pub fn has_run_styles(&self) -> bool {
        self.runs().iter().any(|run| run.style.is_some())
    }

    /// Whether a run's `font_scale` changes over time, so the text lays out again as it does.
    #[must_use]
    pub fn varies(&self) -> bool {
        self.runs().iter().any(|run| {
            run.style
                .as_ref()
                .and_then(|style| style.font_scale.as_ref())
                .is_some_and(Animated::varies)
        })
    }

    pub(crate) fn invalid_run_style(&self, base: FontSelection<'_>) -> Option<usize> {
        let mut selections = Vec::new();
        for (index, run) in self.runs().iter().enumerate() {
            let style = run.style.as_ref();
            if style.is_some_and(|style| !style.is_valid()) {
                return Some(index);
            }
            let selection = TextRunStyle::selection(style, base);
            if !selections.contains(&selection) {
                if selections.len() == crate::MAX_TEXT_FONT_SELECTIONS {
                    return Some(index);
                }
                selections.push(selection);
            }
        }
        None
    }
}

/// A run's changes to its holder's text style, as an inline box's own CSS properties. Each
/// field set replaces the holder's value for the run's characters; a field left out keeps it.
/// A change of family, weight, style, size or letter spacing shapes the run apart and can move
/// line breaks; fill, strokes, background and decorations only paint.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextRunStyle {
    /// The family, as a text style's `font_family`; `null` draws from the fallback order alone.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::caption::double_option"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub font_family: Option<Option<String>>,
    /// The weight in `[1, 1000]`, as a text style's `font_weight`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_weight: Option<u16>,
    /// The face style, as a text style's `font_style`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_style: Option<FontStyle>,
    /// The run's size as a multiple of the text's font size, in `[0.1, 10]`, as CSS
    /// `font-size: <n>em`. Lines are laid out again when it changes; keyframes are in the
    /// holder's time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_scale: Option<Animated<f32>>,
    /// Space after each character, as a fraction of the run's size, in `[-0.5, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter_spacing: Option<f32>,
    /// What paints the run's glyphs; gradient positions are in the text's layout box, as the
    /// text's fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Paint>,
    /// The run's bands, as a text style's `strokes`, widths in fractions of the run's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "crate::bounded::option_seq::<_, _, { crate::MAX_TEXT_STROKES }>")]
    #[cfg_attr(feature = "schema", schemars(length(max = crate::MAX_TEXT_STROKES)))]
    pub strokes: Option<Vec<TextStroke>>,
    /// A box behind the run on each line, its advances wide and its em box tall, as CSS
    /// `background-color` paints an inline box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<Rgba>,
    /// See [`crate::TextStyle::underline`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    /// See [`crate::TextStyle::line_through`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_through: Option<bool>,
}

impl TextRunStyle {
    /// `base` with every face, spacing, paint and decoration field this style sets replaced.
    /// The size stays the base's: `font_scale` multiplies it where the run is laid out, and
    /// `background_color` has no text-wide counterpart.
    #[must_use]
    pub fn apply(&self, base: &crate::TextStyle) -> crate::TextStyle {
        let mut style = base.clone();
        if let Some(family) = &self.font_family {
            style.font_family.clone_from(family);
        }
        if let Some(weight) = self.font_weight {
            style.font_weight = weight;
        }
        if let Some(font_style) = self.font_style {
            style.font_style = font_style;
        }
        if let Some(spacing) = self.letter_spacing {
            style.letter_spacing = spacing;
        }
        if let Some(fill) = &self.fill {
            style.fill = fill.clone();
        }
        if let Some(strokes) = &self.strokes {
            style.strokes.clone_from(strokes);
        }
        if let Some(underline) = self.underline {
            style.underline = underline;
        }
        if let Some(line_through) = self.line_through {
            style.line_through = line_through;
        }
        style
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.font_family.as_ref().is_none_or(|family| {
            family.as_ref().is_none_or(|family| {
                !family.is_empty() && family.len() <= crate::MAX_FONT_FAMILY_BYTES
            })
        }) && self
            .font_weight
            .is_none_or(|weight| crate::FONT_WEIGHT_RANGE.contains(&weight))
            && self.font_scale.as_ref().is_none_or(|scale| {
                scale.data().is_none()
                    && valid_scalar_values(scale, |value| crate::FONT_SCALE_RANGE.contains(&value))
            })
            && self
                .letter_spacing
                .is_none_or(|spacing| crate::LETTER_SPACING_RANGE.contains(&spacing))
            && self.fill.as_ref().is_none_or(Paint::is_valid)
            && self
                .strokes
                .as_deref()
                .is_none_or(crate::text::valid_strokes)
            && self.background_color.is_none_or(Rgba::is_valid)
    }

    /// Whether the run takes only fields a layout text's run may: no keyframed font scale.
    #[must_use]
    pub const fn is_static(&self) -> bool {
        !matches!(
            self.font_scale,
            Some(Animated::Keyframes { .. } | Animated::Data(_))
        )
    }

    fn selection(
        style: Option<&Self>,
        base: FontSelection<'_>,
    ) -> (Option<String>, u16, FontStyle) {
        let family = match style.and_then(|style| style.font_family.as_ref()) {
            Some(family) => family.as_deref(),
            None => base.family,
        };
        (
            family.map(str::to_ascii_lowercase),
            style
                .and_then(|style| style.font_weight)
                .unwrap_or(base.weight),
            style
                .and_then(|style| style.font_style)
                .unwrap_or(base.style),
        )
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct FontSelection<'a> {
    pub(crate) family: Option<&'a str>,
    pub(crate) weight: u16,
    pub(crate) style: FontStyle,
}

impl<'a> FontSelection<'a> {
    pub(crate) fn of(style: &'a crate::TextStyle) -> Self {
        Self {
            family: style.font_family.as_deref(),
            weight: style.font_weight,
            style: style.font_style,
        }
    }
}

/// A piece of a text with its own style, that animators' Class and Timing selectors can pick.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextRun {
    /// Non-empty; `\n` starts a line. A grapheme cluster spanning two runs belongs to the run
    /// holding its first byte.
    pub text: String,
    /// Names Class selectors match, each 1 to 64 bytes of `A-Z`, `a-z`, `0-9`, `_` and `-`; at
    /// most [`MAX_RUN_CLASSES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "crate::bounded::seq::<_, _, { crate::MAX_RUN_CLASSES }>")]
    #[cfg_attr(feature = "schema", schemars(length(max = crate::MAX_RUN_CLASSES)))]
    pub class: Vec<String>,
    /// When the run is said, from the holder's start (a caption cue's start, or a text
    /// generator's picture time zero).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<RunTime>,
    /// Changes to the holder's style for this run's characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<TextRunStyle>,
}

impl TextRun {
    fn is_valid(&self) -> bool {
        !self.text.is_empty()
            && self.class.len() <= MAX_RUN_CLASSES
            && self.class.iter().all(|name| valid_class_name(name))
            && self
                .time
                .is_none_or(|time| time.start.ticks() >= 0 && time.start < time.end)
    }
}

/// Whether `name` is a class name a run or a Class selector may carry.
#[must_use]
pub fn valid_class_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CLASS_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// A run's time span.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RunTime {
    /// When it starts.
    pub start: MediaTime,
    /// When it ends; after `start`.
    pub end: MediaTime,
}

/// One animator: what it selects, and what it does to what it selects.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextAnimator {
    /// Gives each character its selection, in `[0, 1]`.
    pub selector: TextSelector,
    /// Applied to each character in proportion to its selection.
    #[serde(default)]
    pub properties: TextAnimatorProperties,
}

/// How an animator selects characters.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TextSelector {
    /// A range over the text's units, shaped at its ends, like After Effects' range selector.
    Range {
        /// What the range counts: characters (the default), non-space characters, words, or lines.
        #[serde(default)]
        based_on: TextUnit,
        /// Whether start, end and offset are percents of the units (the default) or unit indices.
        #[serde(default)]
        units: RangeUnits,
        /// Where the range starts: `[-100, 200]` percent, or `[-N, 2N]` units for `N` of
        /// [`MAX_RANGE_INDEX`]. Default 0.
        #[serde(default = "zero")]
        start: Animated<f32>,
        /// Where it ends, in the same range. Default 100.
        #[serde(default = "hundred")]
        end: Animated<f32>,
        /// Added to start and end, in the same range. Default 0.
        #[serde(default = "zero")]
        offset: Animated<f32>,
        /// How selection rises and falls across the range.
        #[serde(default)]
        shape: RangeShape,
        /// The selection's scale, in `[0, 1]`. Default 1.
        #[serde(default = "one")]
        amount: Animated<f32>,
    },
    /// Characters of timed runs, by the run's time against the current time.
    Timing {
        /// Which runs it selects: those not yet said, being said, or said.
        state: RunState,
        /// How long the state takes to fade in and out; zero switches.
        #[serde(default)]
        ramp: MediaTime,
        /// Splits each run's span over its characters in order, sweeping the run character by
        /// character.
        #[serde(default)]
        sweep: bool,
    },
    /// Characters of runs that carry a class.
    Class {
        /// The class name.
        class: String,
    },
}

const fn zero() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
}

const fn hundred() -> Animated<f32> {
    Animated::Constant { value: 100.0 }
}

const fn one() -> Animated<f32> {
    Animated::Constant { value: 1.0 }
}

/// The units a Range selector counts.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TextUnit {
    /// Grapheme clusters, spaces included.
    #[default]
    Characters,
    /// Grapheme clusters that are not white space.
    CharactersExcludingSpaces,
    /// UAX #29 words that hold a letter or digit; a CJK ideograph is a word.
    Words,
    /// Laid-out lines.
    Lines,
}

/// What a Range selector's start, end and offset count in.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RangeUnits {
    /// Percents of the units.
    #[default]
    Percent,
    /// Unit indices.
    Index,
}

/// How selection rises and falls across a Range selector's range.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RangeShape {
    /// Fully selected inside the range, partly at fractional ends.
    #[default]
    Square,
    /// Rising from the start to the end.
    RampUp,
    /// Falling from the start to the end.
    RampDown,
    /// Rising to the middle, falling after it.
    Triangle,
    /// A half circle over the range.
    Round,
    /// The triangle eased by smoothstep.
    Smooth,
}

/// Which runs a Timing selector selects.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Runs whose time has not started.
    Upcoming,
    /// Runs whose time holds the current time.
    Active,
    /// Runs whose time has ended.
    Spoken,
}

/// What an animator does to each character, in proportion to its selection. A property left
/// out does nothing.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextAnimatorProperties {
    /// Moves the character, in font sizes, +y down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<Animated<Vec2>>,
    /// Scales the character about its centre; 1 is none. Each axis in `[0, 10]`. The scales of
    /// several animators multiply, and the product is at most 10 on each axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Animated<Vec2>>,
    /// Turns the character about its centre, clockwise degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<Animated<f32>>,
    /// The opacity a fully selected character takes, in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<Animated<f32>>,
    /// The fill color a fully selected character takes; a gradient fill is mixed toward it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Animated<Rgba>>,
    /// The color every stroke of a fully selected character takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<Animated<Rgba>>,
    /// Space added after the character, in font sizes, in `[-0.5, 1]`, as letter spacing adds
    /// it. Lines are not re-broken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking: Option<Animated<f32>>,
    /// Whether the character's advance follows this animator's horizontal scale, pushing the
    /// characters after it along its line, which is re-aligned; lines are not re-broken, and the
    /// text wraps with room for the largest scale this can reach. Needs `scale`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub advance_follows_scale: bool,
    /// Wipes the character's unit (its group, as `animator_grouping` gives) in along a straight
    /// edge, cutting its fill, strokes and color glyphs alike; a fully selected unit shows
    /// `progress` of itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal: Option<TextReveal>,
}

/// A straight edge that shows a unit up to it, as a linear wipe: across the unit's box (its
/// advances by its em boxes, outset by its strokes) in the direction `angle_degrees`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextReveal {
    /// How much of a fully selected unit shows, in `[0, 1]`: 0 hides it, 1 shows it whole. A unit
    /// selected by `w` shows `1 + w (progress - 1)`. Default 0.
    #[serde(default = "zero")]
    pub progress: Animated<f32>,
    /// The direction the edge travels, clockwise degrees with y down: 0 reveals left to right,
    /// 90 top to bottom. Default 0.
    #[serde(default = "zero")]
    pub angle_degrees: Animated<f32>,
    /// The edge's softness in font sizes, in `[0, MAX_REVEAL_FEATHER]`; 0 is a one-pixel
    /// antialiased step. Default 0.
    #[serde(default = "zero")]
    pub feather: Animated<f32>,
}

impl TextReveal {
    fn is_valid(&self) -> bool {
        valid_scalar_values(&self.progress, |value| (0.0..=1.0).contains(&value))
            && valid_scalar_values(&self.angle_degrees, |_| true)
            && valid_scalar_values(&self.feather, |value| {
                (0.0..=MAX_REVEAL_FEATHER).contains(&value)
            })
    }
}

/// The units whose clusters scale and turn about one pivot, as After Effects' Anchor Point
/// Grouping.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnimatorGrouping {
    /// Each grapheme cluster about its own pivot.
    #[default]
    Character,
    /// Each UAX #29 word holding a letter or digit; a cluster in no word is its own group.
    Word,
    /// Each line.
    Line,
    /// The whole text.
    All,
}

impl TextAnimatorProperties {
    /// Whether no property is set: the animator does nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.offset.is_none()
            && self.scale.is_none()
            && self.rotation_degrees.is_none()
            && self.opacity.is_none()
            && self.fill.is_none()
            && self.stroke.is_none()
            && self.tracking.is_none()
            && self.reveal.is_none()
    }

    fn is_valid(&self) -> bool {
        let vec2 = |animation: &Animated<Vec2>, check: &dyn Fn(f32) -> bool| match animation {
            Animated::Constant { value } => check(value.x) && check(value.y),
            Animated::Keyframes { keyframes } => {
                valid_keyframes(keyframes)
                    && keyframes
                        .iter()
                        .all(|keyframe| check(keyframe.value.x) && check(keyframe.value.y))
            }
            Animated::Data(_) => true,
        };
        let color = |animation: &Animated<Rgba>| match animation {
            Animated::Constant { value } => value.is_valid(),
            Animated::Keyframes { keyframes } => {
                valid_keyframes(keyframes) && keyframes.iter().all(|key| key.value.is_valid())
            }
            Animated::Data(_) => false,
        };
        self.offset
            .as_ref()
            .is_none_or(|offset| vec2(offset, &|value| value.abs() <= MAX_ANIMATOR_OFFSET))
            && self.scale.as_ref().is_none_or(|scale| {
                vec2(scale, &|value| {
                    value.is_finite() && ANIMATOR_SCALE_RANGE.contains(&value)
                })
            })
            && self
                .rotation_degrees
                .as_ref()
                .is_none_or(|rotation| valid_scalar_values(rotation, |_| true))
            && self.opacity.as_ref().is_none_or(|opacity| {
                valid_scalar_values(opacity, |value| (0.0..=1.0).contains(&value))
            })
            && self.fill.as_ref().is_none_or(color)
            && self.stroke.as_ref().is_none_or(color)
            && self.tracking.as_ref().is_none_or(|tracking| {
                valid_scalar_values(tracking, |value| {
                    crate::LETTER_SPACING_RANGE.contains(&value)
                })
            })
            && (!self.advance_follows_scale || self.scale.is_some())
            && self.reveal.as_ref().is_none_or(TextReveal::is_valid)
    }
}

impl TextSelector {
    fn is_valid(&self) -> bool {
        match self {
            Self::Range {
                units,
                start,
                end,
                offset,
                amount,
                ..
            } => {
                let range = match units {
                    RangeUnits::Percent => RANGE_PERCENT,
                    RangeUnits::Index => -MAX_RANGE_INDEX..=2.0 * MAX_RANGE_INDEX,
                };
                [start, end, offset]
                    .into_iter()
                    .all(|value| valid_scalar_values(value, |value| range.contains(&value)))
                    && valid_scalar_values(amount, |value| (0.0..=1.0).contains(&value))
            }
            Self::Timing { ramp, .. } => ramp.ticks() >= 0,
            Self::Class { class } => valid_class_name(class),
        }
    }
}

/// Whether `animators` are at most [`MAX_TEXT_ANIMATORS`] valid animators, using only Range
/// selectors when `range_only`.
pub(crate) fn valid_animators(animators: &[TextAnimator], range_only: bool) -> bool {
    animators.len() <= MAX_TEXT_ANIMATORS
        && animators.iter().all(|animator| {
            animator.selector.is_valid()
                && animator.properties.is_valid()
                && (!range_only || matches!(animator.selector, TextSelector::Range { .. }))
        })
}

/// Whether any value of `animators` has keyframes or data.
pub(crate) fn animators_have_keyframes(animators: &[TextAnimator]) -> bool {
    let mut keyed = false;
    for_each_keyed_animator(animators, "", &mut |_, _| keyed = true);
    keyed
}

/// Calls `visit` with each keyed value of `animators` under `at`, for the lint's keyframe
/// walk.
pub(crate) fn for_each_keyed_animator(
    animators: &[TextAnimator],
    at: &str,
    visit: &mut impl FnMut(&str, crate::animation::Keyed<'_>),
) {
    use crate::animation::visit_keyed;
    for (index, animator) in animators.iter().enumerate() {
        let at = format!("{at}[{index}]");
        if let TextSelector::Range {
            start,
            end,
            offset,
            amount,
            ..
        } = &animator.selector
        {
            visit_keyed(&format!("{at}.selector.start"), start, visit);
            visit_keyed(&format!("{at}.selector.end"), end, visit);
            visit_keyed(&format!("{at}.selector.offset"), offset, visit);
            visit_keyed(&format!("{at}.selector.amount"), amount, visit);
        }
        let properties = &animator.properties;
        let at = format!("{at}.properties");
        if let Some(value) = &properties.offset {
            visit_keyed(&format!("{at}.offset"), value, visit);
        }
        if let Some(value) = &properties.scale {
            visit_keyed(&format!("{at}.scale"), value, visit);
        }
        if let Some(value) = &properties.rotation_degrees {
            visit_keyed(&format!("{at}.rotation_degrees"), value, visit);
        }
        if let Some(value) = &properties.opacity {
            visit_keyed(&format!("{at}.opacity"), value, visit);
        }
        if let Some(value) = &properties.fill {
            visit_keyed(&format!("{at}.fill"), value, visit);
        }
        if let Some(value) = &properties.stroke {
            visit_keyed(&format!("{at}.stroke"), value, visit);
        }
        if let Some(value) = &properties.tracking {
            visit_keyed(&format!("{at}.tracking"), value, visit);
        }
        if let Some(reveal) = &properties.reveal {
            visit_keyed(&format!("{at}.reveal.progress"), &reveal.progress, visit);
            visit_keyed(
                &format!("{at}.reveal.angle_degrees"),
                &reveal.angle_degrees,
                visit,
            );
            visit_keyed(&format!("{at}.reveal.feather"), &reveal.feather, visit);
        }
    }
}
