use crate::{
    AudioProperties, Captions, ClipBlendMode, ClipComposition, ClipEffect, ClipId, Easing, Matte,
    MediaKind, ShapeMask, TimeMap, TrackAudio, TrackId, VisualProperties,
};
use alpha2_media::SourceId;
use alpha2_time::TimeRange;
use serde::{Deserialize, Serialize};

/// Timeline tracks ordered from back to front for visual tracks.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Timeline {
    /// Tracks in composition order.
    pub tracks: Vec<Track>,
}

impl Timeline {
    /// The smallest interval holding everything the timeline shows: every
    /// clip, and every caption cue, which a caption track holds instead of
    /// clips. `None` for a timeline that shows nothing. What a whole-project
    /// render or export covers.
    #[must_use]
    pub fn extent(&self) -> Option<TimeRange> {
        let mut ranges = self.tracks.iter().flat_map(|track| {
            track.clips.iter().map(|clip| clip.timeline_range).chain(
                track
                    .captions
                    .iter()
                    .flat_map(|captions| captions.cues.iter().map(|cue| cue.timeline_range)),
            )
        });
        let first = ranges.next()?;
        let (start, end) = ranges.fold((first.start(), first.end()), |(start, end), range| {
            (start.min(range.start()), end.max(range.end()))
        });
        TimeRange::new(start, end).ok()
    }

    /// Whether any track solos its bus, which silences every bus that does
    /// not ([`TrackAudio::is_silenced`]).
    #[must_use]
    pub fn any_solo(&self) -> bool {
        self.tracks.iter().any(|track| track.audio.solo)
    }

    /// The clips that are another clip's matte. Such a clip is drawn only
    /// into the mattes that name it, never composited itself, wherever on
    /// the timeline either clip is.
    #[must_use]
    pub fn matte_sources(&self) -> std::collections::HashSet<&ClipId> {
        self.tracks
            .iter()
            .flat_map(|track| &track.clips)
            .filter_map(|clip| clip.matte.as_ref().map(|matte| &matte.source))
            .collect()
    }
}

/// A homogeneous visual or audio timeline lane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Track {
    /// Stable track identity.
    pub id: TrackId,
    /// User-visible track name.
    pub name: String,
    /// Controls source compatibility and evaluation behavior.
    pub kind: TrackKind,
    /// Clips on this track. Ordering is authoring order; evaluation sorts by time.
    /// A caption track has none.
    #[serde(default)]
    pub clips: Vec<Clip>,
    /// Explicit transitions between overlapping clips on this track.
    #[serde(default)]
    pub transitions: Vec<ClipTransition>,
    /// Bus controls applied to this track's mixed audio.
    #[serde(default)]
    pub audio: TrackAudio,
    /// The cues and style of a caption track; present exactly for that kind.
    #[serde(default)]
    pub captions: Option<Captions>,
}

impl Track {
    /// Creates an empty track with default bus controls.
    #[must_use]
    pub fn new(id: TrackId, name: impl Into<String>, kind: TrackKind) -> Self {
        Self {
            id,
            name: name.into(),
            kind,
            clips: Vec::new(),
            transitions: Vec::new(),
            audio: TrackAudio::default(),
            captions: (kind == TrackKind::Caption).then(Captions::default),
        }
    }
}

/// The media domain accepted by a track.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TrackKind {
    /// A composited visual track.
    Video,
    /// A mixed audio track.
    Audio,
    /// Timed text, drawn over the tracks before it and under the ones after.
    Caption,
}

impl TrackKind {
    pub(crate) const fn accepts(self, source: MediaKind) -> bool {
        matches!(
            (self, source),
            (
                Self::Video,
                MediaKind::Video | MediaKind::Image | MediaKind::Generated | MediaKind::Composition
            ) | (Self::Audio, MediaKind::Audio)
        )
    }
}

/// A reference to a source over independent timeline and source ranges.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Clip {
    /// Stable clip identity.
    pub id: ClipId,
    /// Source consumed by this clip.
    pub source_id: SourceId,
    /// Destination interval on the project timeline.
    pub timeline_range: TimeRange,
    /// Interval read from the source timeline.
    pub source_range: TimeRange,
    /// Resolution-independent visual presentation controls.
    #[serde(default)]
    pub visual: VisualProperties,
    /// The composition graph that builds the clip's picture from its source,
    /// with the clip's time mapping onto it. Without one the clip shows its
    /// source as it is. Visual clips only.
    #[serde(default)]
    pub composition: Option<ClipComposition>,
    /// Artistic operation used when this clip is composited over earlier layers.
    #[serde(default)]
    pub blend_mode: ClipBlendMode,
    /// Shapes that limit where the clip is visible, applied in order after
    /// its composition and before its transform. At most
    /// [`crate::MAX_CLIP_MASKS`]; visual clips only.
    #[serde(default)]
    pub masks: Vec<ShapeMask>,
    /// Another visual clip whose picture sets where this clip is visible.
    #[serde(default)]
    pub matte: Option<Matte>,
    /// How timeline time maps onto the source range.
    #[serde(default)]
    pub time_map: TimeMap,
    /// Presentation controls applied to this clip's audio.
    #[serde(default)]
    pub audio: AudioProperties,
    /// Whether a runtime may keep and reuse the clip's rendered picture.
    /// Never changes what renders: a kept picture is the one a live render
    /// draws.
    #[serde(default, skip_serializing_if = "CacheIntent::is_auto")]
    pub cache: CacheIntent,
}

/// Whether a runtime may keep and reuse a clip's rendered picture.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CacheIntent {
    /// The runtime decides, keeping pictures whose inputs it can identify.
    #[default]
    Auto,
    /// Keep it ahead of other pictures, and bake its range when a host bakes
    /// render caches.
    Pin,
    /// Always render it live: nothing it draws is read from or kept in a
    /// cache.
    Bypass,
}

impl CacheIntent {
    /// Whether it is [`CacheIntent::Auto`].
    #[must_use]
    pub const fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

impl Clip {
    /// Creates a clip with default visual, blend, and audio controls and no
    /// composition.
    #[must_use]
    pub fn new(
        id: ClipId,
        source_id: SourceId,
        timeline_range: TimeRange,
        source_range: TimeRange,
    ) -> Self {
        Self {
            id,
            source_id,
            timeline_range,
            source_range,
            visual: VisualProperties::default(),
            composition: None,
            blend_mode: ClipBlendMode::default(),
            masks: Vec::new(),
            matte: None,
            time_map: TimeMap::Forward,
            audio: AudioProperties::default(),
            cache: CacheIntent::Auto,
        }
    }
}

/// A two-input transition over the overlap of two visual clips.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ClipTransition {
    /// Clip sampled as transition progress zero.
    pub from_clip: ClipId,
    /// Clip sampled as transition progress one.
    pub to_clip: ClipId,
    /// Half-open timeline interval over which both clips must be active.
    pub timeline_range: TimeRange,
    /// Timing curve applied to the transition's linear progress. The eased
    /// progress is clamped to `0..=1`, the domain transition shaders define.
    pub easing: Easing,
    /// Versioned transition shader and named parameter values.
    pub effect: ClipEffect,
}
