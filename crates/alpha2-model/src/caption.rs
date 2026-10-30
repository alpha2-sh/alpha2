//! Caption tracks: timed text the engine draws in a track's style.

use crate::{MAX_TEXT_CONTENT_BYTES, TextStyle};
use alpha2_time::TimeRange;
use serde::{Deserialize, Serialize};

/// What a caption track shows: its cues, in one style.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Captions {
    /// How every cue of the track looks.
    #[serde(default)]
    pub style: TextStyle,
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
    /// [`MAX_TEXT_CONTENT_BYTES`] bytes. Run times are from the cue's start.
    pub text: crate::TextContent,
}

/// Largest caption font size, as a fraction of the frame's shorter side.
pub const MAX_CAPTION_FONT_SIZE: f32 = 0.25;

impl Captions {
    pub(crate) fn is_valid(&self) -> bool {
        self.style.is_valid(MAX_CAPTION_FONT_SIZE)
            && self.cues.iter().all(|cue| {
                !cue.timeline_range.is_empty()
                    && cue.timeline_range.start().ticks() >= 0
                    && cue.text.is_valid(MAX_TEXT_CONTENT_BYTES)
            })
            && self
                .cues
                .windows(2)
                .all(|pair| pair[0].timeline_range.end() <= pair[1].timeline_range.start())
    }
}
