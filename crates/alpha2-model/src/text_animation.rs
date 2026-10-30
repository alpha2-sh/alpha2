//! Text content as runs, and text animators: per-character motion and color
//! a text style holds.
//!
//! A text is one string or runs whose texts joined are the string. Runs never
//! change layout: the joined text is shaped and broken into lines as the
//! plain string is. An animator gives each character (grapheme cluster) a
//! selection in `[0, 1]` and moves, scales, turns, fades or recolors it in
//! proportion, as After Effects' text animators do; Range, Timing and Class
//! selectors cover tracking-in titles, karaoke and keyword highlights alike.

use std::borrow::Cow;

use alpha2_time::MediaTime;
use serde::{Deserialize, Serialize};

use crate::animation::{valid_keyframes, valid_scalar_values};
use crate::{Animated, Rgba, Vec2};

/// Most runs one text holds.
pub const MAX_TEXT_RUNS: usize = 512;

/// Most classes one run carries.
pub const MAX_RUN_CLASSES: usize = 8;

/// Most bytes of one class name.
pub const MAX_CLASS_NAME_BYTES: usize = 64;

/// Most animators one text style holds.
pub const MAX_TEXT_ANIMATORS: usize = 8;

/// The farthest a Range selector in index units reaches past either end, as
/// a count of units: `[-N, 2N]` for `N` of this.
pub const MAX_RANGE_INDEX: f32 = 4_096.0;

/// The range a percent Range selector's start, end and offset take.
pub const RANGE_PERCENT: std::ops::RangeInclusive<f32> = -100.0..=200.0;

/// The range an animator's scale takes on each axis.
pub const ANIMATOR_SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.0..=10.0;

/// A text: one string, or runs whose texts joined are the text.
///
/// Written as `"Hello"` or `[{ "text": "Hel" }, { "text": "lo", "class": ["key"] }]`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum TextContent {
    /// One string; `\n` starts a line.
    Plain(String),
    /// Runs in order; their texts joined are the text.
    Runs(Vec<TextRun>),
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

    /// Whether the text is non-empty and within `max_bytes`, and every run
    /// is valid: non-empty text, at most [`MAX_RUN_CLASSES`] valid class
    /// names, a time span that starts before it ends and not before zero.
    /// There are at most [`MAX_TEXT_RUNS`] runs.
    pub(crate) fn is_valid(&self, max_bytes: usize) -> bool {
        !self.is_empty()
            && self.len() <= max_bytes
            && self.runs().len() <= MAX_TEXT_RUNS
            && self.runs().iter().all(TextRun::is_valid)
    }
}

/// A piece of a text that animators' Class and Timing selectors can pick.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextRun {
    /// Non-empty; `\n` starts a line as in plain text. A grapheme cluster
    /// that spans two runs belongs to the run holding its first byte.
    pub text: String,
    /// Names Class selectors match: each 1 to 64 bytes of `A-Z`, `a-z`,
    /// `0-9`, `_` and `-`; at most [`MAX_RUN_CLASSES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class: Vec<String>,
    /// When the run is said, from the start of what holds it: a caption
    /// cue's start, or a text generator's picture time zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<RunTime>,
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
    /// A range over the text's units, shaped at its ends: After Effects'
    /// range selector.
    Range {
        /// What the range counts: characters (the default), characters that
        /// are not spaces, words, or lines.
        #[serde(default)]
        based_on: TextUnit,
        /// Whether start, end and offset are percents of the units (the
        /// default) or unit indices.
        #[serde(default)]
        units: RangeUnits,
        /// Where the range starts: in `[-100, 200]` percent, or `[-N, 2N]`
        /// units for `N` of [`MAX_RANGE_INDEX`]. Default 0.
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
    /// Characters of timed runs, by where the run's time lies against the
    /// current time.
    Timing {
        /// Which runs it selects: those not yet said, being said, or said.
        state: RunState,
        /// How long the state takes to fade in and out; zero switches.
        #[serde(default)]
        ramp: MediaTime,
        /// Splits each run's span over its characters in order, so the run
        /// is swept character by character.
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

/// What an animator does to each character, in proportion to its selection.
/// A property left out does nothing.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextAnimatorProperties {
    /// Moves the character, in font sizes, +y down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<Animated<Vec2>>,
    /// Scales the character about its centre; 1 is none. Each axis in
    /// `[0, 10]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Animated<Vec2>>,
    /// Turns the character about its centre, clockwise degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<Animated<f32>>,
    /// The opacity a fully selected character takes, in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<Animated<f32>>,
    /// The fill color a fully selected character takes; a gradient fill is
    /// mixed toward it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Animated<Rgba>>,
    /// The color every stroke of a fully selected character takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<Animated<Rgba>>,
    /// Space added after the character, in font sizes, in `[-0.5, 1]`, as
    /// letter spacing adds it; lines are not broken again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking: Option<Animated<f32>>,
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
        };
        let color = |animation: &Animated<Rgba>| match animation {
            Animated::Constant { value } => value.is_valid(),
            Animated::Keyframes { keyframes } => {
                valid_keyframes(keyframes) && keyframes.iter().all(|key| key.value.is_valid())
            }
        };
        self.offset
            .as_ref()
            .is_none_or(|offset| vec2(offset, &|value| value.is_finite()))
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

/// Whether `animators` are at most [`MAX_TEXT_ANIMATORS`] valid animators,
/// and, with `range_only`, use Range selectors alone.
pub(crate) fn valid_animators(animators: &[TextAnimator], range_only: bool) -> bool {
    animators.len() <= MAX_TEXT_ANIMATORS
        && animators.iter().all(|animator| {
            animator.selector.is_valid()
                && animator.properties.is_valid()
                && (!range_only || matches!(animator.selector, TextSelector::Range { .. }))
        })
}

/// Whether any value of `animators` has keyframes.
pub(crate) fn animators_have_keyframes(animators: &[TextAnimator]) -> bool {
    let mut keyed = false;
    for_each_keyed_animator(animators, "", &mut |_, _| keyed = true);
    keyed
}

/// Calls `visit` with each keyed value of `animators` under `at`, for the
/// lint's keyframe walk.
pub(crate) fn for_each_keyed_animator(
    animators: &[TextAnimator],
    at: &str,
    visit: &mut impl FnMut(&str, &[MediaTime]),
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
    }
}
