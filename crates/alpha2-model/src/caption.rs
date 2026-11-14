//! Caption tracks: timed text drawn in a track's style, each cue in a named style or its
//! speaker's.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::{
    FontSizeFit, LineClamp, MAX_TEXT_CONTENT_BYTES, Paint, TextAlign, TextAnimator, TextBackground,
    TextShadow, TextStroke, TextStyle, Vec2,
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
    /// Named changes to `style` for cues that name one or whose speaker has one. At most
    /// [`MAX_CAPTION_STYLES`], names 1 to 64 bytes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub styles: BTreeMap<String, TextStylePatch>,
    /// Cues in timeline order, none overlapping; a track shows one cue at a time. Empty when
    /// `from_words` gives the cues.
    #[serde(default)]
    pub cues: Vec<CaptionCue>,
    /// Cues built at evaluation from a `words` data set, instead of `cues`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_words: Option<crate::CaptionsFromWords>,
}

/// Text shown over one timeline interval.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CaptionCue {
    /// When the cue shows. Non-empty and not before zero.
    pub timeline_range: TimeRange,
    /// The text, a string or runs; `\n` starts a line. Non-empty, at most
    /// [`MAX_TEXT_CONTENT_BYTES`] bytes. Run times are from the cue's start and lie within it.
    pub text: crate::TextContent,
    /// Who says it, as the source names them (a WebVTT voice), at most 64 bytes. A track style
    /// of that name applies when the cue names no style.
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

/// Changes to a [`TextStyle`]. Each field set replaces the base's whole value (lists are not
/// merged); a field left out keeps the base's.
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
    /// See [`TextStyle::max_height`]; `null` removes the base's.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<f32>"))]
    pub max_height: Option<Option<f32>>,
    /// See [`TextStyle::font_size_fit`]; `null` removes the base's.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<FontSizeFit>"))]
    pub font_size_fit: Option<Option<FontSizeFit>>,
    /// See [`TextStyle::line_clamp`]; `null` removes the base's.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<LineClamp>"))]
    pub line_clamp: Option<Option<LineClamp>>,
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
        if let Some(height) = self.max_height {
            style.max_height = height;
        }
        if let Some(fit) = self.font_size_fit {
            style.font_size_fit = fit;
        }
        if let Some(clamp) = &self.line_clamp {
            style.line_clamp.clone_from(clamp);
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
    /// The track's style, a style name, a speaker, a cue's range, text or run times, or the
    /// cues' order.
    Invalid,
    /// Cue `cue` names a style the track does not have.
    UnknownStyle { cue: usize, name: String },
    /// Cue `cue`'s style, its patch applied, is out of range.
    CueStyle { cue: usize },
}

impl Captions {
    /// The patch cue `cue` takes: the style it names, else its speaker's if the track has one,
    /// else none.
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
        let valid = (self.from_words.is_none() || self.cues.is_empty())
            && self.style.is_valid(MAX_CAPTION_FONT_SIZE)
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

/// Whether every run time of `cue` lies within the cue's duration.
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

/// A field that is absent (no change), `null` (clear) or a value, read as `Option<Option<T>>`
/// with `null` kept apart from absent.
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

/// What joins word `left` to word `right` in a cue: nothing between two characters of scripts
/// written without word spaces, else one space.
fn word_separator(left: &str, right: &str) -> &'static str {
    match (left.chars().next_back(), right.chars().next()) {
        (Some(last), Some(first)) if unspaced(last) && unspaced(first) => "",
        _ => " ",
    }
}

/// Whether `character` is of line breaking class ID, CJ or SA (UAX #14), as far as scripts
/// written without spaces between words go. Hangul is spaced and not included.
fn unspaced(character: char) -> bool {
    matches!(
        u32::from(character),
        0x0E00..=0x0EFF // Thai, Lao
            | 0x1000..=0x109F // Myanmar
            | 0x1780..=0x17FF // Khmer
            | 0x19E0..=0x19FF // Khmer symbols
            | 0x2E80..=0x2FDF // CJK and Kangxi radicals
            | 0x3000..=0x303F // CJK symbols and punctuation
            | 0x3040..=0x30FF // Hiragana, Katakana
            | 0x3100..=0x312F // Bopomofo
            | 0x31F0..=0x31FF // Katakana phonetic extensions
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK unified ideographs
            | 0xA9E0..=0xA9FF // Myanmar extended B
            | 0xAA60..=0xAA7F // Myanmar extended A
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFF00..=0xFFEF // Halfwidth and fullwidth forms
            | 0x20000..=0x3FFFF // CJK extensions B and later
    )
}

/// The longest gap between two words of one cue, in ticks (0.6 s).
pub const WORD_PAUSE_TICKS: i64 = 72_000;

/// Words grouped into cues that end at `max_chars` characters, a sentence's end, a pause longer
/// than [`WORD_PAUSE_TICKS`] or a change of speaker. Each cue has one run per word (with the
/// separator that follows it), timed from the cue's start, so Timing selectors follow the speech.
///
/// Words are joined by a space, except where both neighbouring characters belong to scripts
/// written without spaces between words: the Unicode line breaking classes ID, CJ and SA (UAX
/// #14), which cover Han, kana, CJK punctuation, fullwidth forms, Thai, Lao, Khmer and Myanmar.
///
/// `words` are in start order, each ending after it starts with non-empty trimmed text. Where
/// words overlap, each cue ends where the next starts.
#[must_use]
pub fn group_words(words: &[crate::DataWord], max_chars: u32) -> Vec<CaptionCue> {
    // Each cue being built: its start, end, speaker, words with their times and its length.
    type Words = (i64, i64, Option<String>, Vec<(String, i64, i64)>, usize);
    let mut groups: Vec<Words> = Vec::new();
    let mut current: Option<Words> = None;
    for word in words {
        let (start, end) = (word.start.ticks(), word.end.ticks());
        let text = word.text.trim().to_owned();
        let characters = text.chars().count();
        if let Some((_, cue_end, speaker, held, held_length)) = &mut current {
            let last = held.last().map_or("", |(word, ..)| word.as_str());
            let length = *held_length + word_separator(last, &text).len() + characters;
            let sentence_end = last.ends_with([
                '.', '?', '!', '\u{3002}', '\u{ff01}', '\u{ff0e}', '\u{ff1f}',
            ]);
            if length <= max_chars as usize
                && start - *cue_end <= WORD_PAUSE_TICKS
                && !sentence_end
                && *speaker == word.speaker
            {
                held.push((text, start, end));
                *cue_end = (*cue_end).max(end);
                *held_length = length;
                continue;
            }
            groups.extend(current.take());
        }
        current = Some((
            start,
            end,
            word.speaker.clone(),
            vec![(text, start, end)],
            characters,
        ));
    }
    groups.extend(current);
    let mut cues = groups
        .into_iter()
        .filter_map(|(start, end, speaker, held, _)| {
            let separators = held
                .windows(2)
                .map(|pair| word_separator(&pair[0].0, &pair[1].0))
                .chain([""])
                .collect::<Vec<_>>();
            let runs = held
                .into_iter()
                .zip(separators)
                .map(|((mut text, from, to), separator)| {
                    text.push_str(separator);
                    crate::TextRun {
                        text,
                        class: Vec::new(),
                        time: Some(crate::RunTime {
                            start: alpha2_time::MediaTime(from - start),
                            end: alpha2_time::MediaTime(to - start),
                        }),
                    }
                })
                .collect();
            let range =
                TimeRange::new(alpha2_time::MediaTime(start), alpha2_time::MediaTime(end)).ok()?;
            Some(CaptionCue {
                speaker,
                ..CaptionCue::new(range, crate::TextContent::Runs(runs))
            })
        })
        .collect::<Vec<_>>();
    for index in 1..cues.len() {
        let next = cues[index].timeline_range.start();
        let previous = &mut cues[index - 1];
        if previous.timeline_range.end() > next && previous.timeline_range.start() < next {
            previous.timeline_range = TimeRange::new(previous.timeline_range.start(), next)
                .unwrap_or(previous.timeline_range);
            clip_runs(previous);
        }
    }
    cues
}

/// Clamps a cue's run times to its range after the range was shortened. A run that would start
/// at or after the end gets no time.
pub fn clip_runs(cue: &mut CaptionCue) {
    let duration = cue
        .timeline_range
        .duration()
        .map_or(0, alpha2_time::MediaTime::ticks);
    if let crate::TextContent::Runs(runs) = &mut cue.text {
        for run in runs {
            run.time = run.time.and_then(|time| {
                let end = time.end.ticks().min(duration);
                (time.start.ticks() < end).then_some(crate::RunTime {
                    start: time.start,
                    end: alpha2_time::MediaTime(end),
                })
            });
        }
    }
}
