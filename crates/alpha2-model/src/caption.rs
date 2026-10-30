//! Caption tracks: timed text the engine draws in a track's style, each cue
//! in a named style or its speaker's when it has one.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::{
    MAX_TEXT_CONTENT_BYTES, Paint, TextAlign, TextAnimator, TextBackground, TextShadow, TextStroke,
    TextStyle, Vec2,
};
use alpha2_time::TimeRange;
use serde::{Deserialize, Serialize};

/// Most named styles one caption track holds.
pub const MAX_CAPTION_STYLES: usize = 32;

/// Most bytes of a caption style's name or a cue's speaker.
pub const MAX_CAPTION_NAME_BYTES: usize = 64;

/// What a caption track shows: its cues, in its style.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Captions {
    /// The style every cue starts from.
    #[serde(default)]
    pub style: TextStyle,
    /// Named changes to `style`, for cues that name one or whose speaker has
    /// one: at most [`MAX_CAPTION_STYLES`], names 1 to 64 bytes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub styles: BTreeMap<String, TextStylePatch>,
    /// Cues in timeline order, none overlapping another: a track shows one
    /// cue at a time. Simultaneous captions take one track each.
    pub cues: Vec<CaptionCue>,
}

/// Text shown over one timeline interval.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CaptionCue {
    /// When the cue shows. Non-empty and not before zero.
    pub timeline_range: TimeRange,
    /// The text, a string or runs; `\n` starts a line. Non-empty, at most
    /// [`MAX_TEXT_CONTENT_BYTES`] bytes. Run times are from the cue's start
    /// and lie within the cue.
    pub text: crate::TextContent,
    /// Who says it, as the source names them (a WebVTT voice): at most 64
    /// bytes. A style of that name, when the track has one, styles the cue
    /// when the cue names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// The name of one of the track's `styles`, applied over its style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

impl CaptionCue {
    /// A cue showing `text` over `timeline_range`, with no speaker or style.
    #[must_use]
    pub fn new(timeline_range: TimeRange, text: impl Into<crate::TextContent>) -> Self {
        Self {
            timeline_range,
            text: text.into(),
            speaker: None,
            style: None,
        }
    }
}

/// Changes to a [`TextStyle`]: each field set replaces the base's whole
/// (lists are replaced, not merged); a field left out keeps the base's.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextStylePatch {
    /// The family; `null` draws from the fallback order alone.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub font_family: Option<Option<String>>,
    /// See [`TextStyle::font_size`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// See [`TextStyle::fill`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Paint>,
    /// See [`TextStyle::line_height`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f32>,
    /// See [`TextStyle::letter_spacing`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter_spacing: Option<f32>,
    /// See [`TextStyle::font_weight`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_weight: Option<u16>,
    /// See [`TextStyle::align`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<TextAlign>,
    /// See [`TextStyle::max_width`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f32>,
    /// See [`TextStyle::position`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec2>,
    /// See [`TextStyle::anchor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Vec2>,
    /// See [`TextStyle::strokes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strokes: Option<Vec<TextStroke>>,
    /// See [`TextStyle::shadows`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadows: Option<Vec<TextShadow>>,
    /// The box behind each line; `null` removes the base's.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<TextBackground>"))]
    pub background: Option<Option<TextBackground>>,
    /// See [`TextStyle::animators`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animators: Option<Vec<TextAnimator>>,
}

impl TextStylePatch {
    /// `base` with every field this patch sets replaced.
    #[must_use]
    pub fn apply(&self, base: &TextStyle) -> TextStyle {
        let mut style = base.clone();
        if let Some(family) = &self.font_family {
            style.font_family.clone_from(family);
        }
        if let Some(size) = self.font_size {
            style.font_size = size;
        }
        if let Some(fill) = &self.fill {
            style.fill = fill.clone();
        }
        if let Some(height) = self.line_height {
            style.line_height = height;
        }
        if let Some(spacing) = self.letter_spacing {
            style.letter_spacing = spacing;
        }
        if let Some(weight) = self.font_weight {
            style.font_weight = weight;
        }
        if let Some(align) = self.align {
            style.align = align;
        }
        if let Some(width) = self.max_width {
            style.max_width = width;
        }
        if let Some(position) = self.position {
            style.position = position;
        }
        if let Some(anchor) = self.anchor {
            style.anchor = anchor;
        }
        if let Some(strokes) = &self.strokes {
            style.strokes.clone_from(strokes);
        }
        if let Some(shadows) = &self.shadows {
            style.shadows.clone_from(shadows);
        }
        if let Some(background) = self.background {
            style.background = background;
        }
        if let Some(animators) = &self.animators {
            style.animators.clone_from(animators);
        }
        style
    }
}

/// Largest caption font size, as a fraction of the frame's shorter side.
pub const MAX_CAPTION_FONT_SIZE: f32 = 0.25;

/// Why a caption track's cue or style is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CaptionIssue {
    /// The track's style, a style name, a speaker, a cue's range, text or
    /// run times, or the cues' order.
    Invalid,
    /// Cue `cue` names a style the track does not have.
    UnknownStyle { cue: usize, name: String },
    /// Cue `cue`'s style, its patch applied, is out of range.
    CueStyle { cue: usize },
}

impl Captions {
    /// The patch cue `cue` takes: the style it names, else its speaker's
    /// when the track has a style of that name, else none.
    #[must_use]
    pub fn cue_patch(&self, cue: &CaptionCue) -> Option<&TextStylePatch> {
        match &cue.style {
            Some(name) => self.styles.get(name),
            None => cue
                .speaker
                .as_ref()
                .and_then(|speaker| self.styles.get(speaker)),
        }
    }

    /// The style cue `cue` draws in: the track's, with its patch applied.
    #[must_use]
    pub fn cue_style(&self, cue: &CaptionCue) -> Cow<'_, TextStyle> {
        match self.cue_patch(cue) {
            Some(patch) => Cow::Owned(patch.apply(&self.style)),
            None => Cow::Borrowed(&self.style),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), CaptionIssue> {
        let name = |name: &str| !name.is_empty() && name.len() <= MAX_CAPTION_NAME_BYTES;
        let valid = self.style.is_valid(MAX_CAPTION_FONT_SIZE)
            && self.styles.len() <= MAX_CAPTION_STYLES
            && self.styles.keys().all(|key| name(key))
            && self.cues.iter().all(|cue| {
                !cue.timeline_range.is_empty()
                    && cue.timeline_range.start().ticks() >= 0
                    && cue.text.is_valid(MAX_TEXT_CONTENT_BYTES)
                    && cue.speaker.as_deref().is_none_or(name)
                    && run_times_lie_in(cue)
            })
            && self
                .cues
                .windows(2)
                .all(|pair| pair[0].timeline_range.end() <= pair[1].timeline_range.start());
        if !valid {
            return Err(CaptionIssue::Invalid);
        }
        for (index, cue) in self.cues.iter().enumerate() {
            if let Some(style) = &cue.style
                && !self.styles.contains_key(style)
            {
                return Err(CaptionIssue::UnknownStyle {
                    cue: index,
                    name: style.clone(),
                });
            }
            if self.cue_patch(cue).is_some() && !self.cue_style(cue).is_valid(MAX_CAPTION_FONT_SIZE)
            {
                return Err(CaptionIssue::CueStyle { cue: index });
            }
        }
        Ok(())
    }
}

/// Whether every run time of `cue` lies within the cue: from zero to its
/// duration.
fn run_times_lie_in(cue: &CaptionCue) -> bool {
    let duration = cue
        .timeline_range
        .duration()
        .map_or(0, alpha2_time::MediaTime::ticks);
    cue.text.runs().iter().all(|run| {
        run.time
            .is_none_or(|time| time.start.ticks() >= 0 && time.end.ticks() <= duration)
    })
}

/// A field that is left out (no change), `null` (clear) or a value: serde's
/// `Option<Option<T>>` read with `null` kept apart from absent.
#[allow(clippy::option_option, clippy::ref_option)]
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<T: Serialize, S: Serializer>(
        value: &Option<Option<T>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(inner) => inner.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}
