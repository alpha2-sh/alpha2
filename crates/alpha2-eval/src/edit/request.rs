//! What an edit operation asks for, what it reports, and why it refuses.

use alpha2_model::{
    ClipId, CompositionId, DataAnchor, DataKind, DataSetId, ModelError, NodeId, TrackId,
};
use alpha2_time::{MediaTime, TimeRange};
use serde::{Deserialize, Serialize};
use std::fmt;

/// One edit operation, applied to a project once.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditRequest {
    /// Removes timeline ranges from the target clips, with ripple.
    RemoveRanges {
        /// An `intervals` data set or an anchored list.
        ranges: Ranges,
        /// The clips the ranges come from and apply to.
        target: Target,
        /// What closes the gaps; `timeline` by default.
        #[serde(default)]
        ripple: Ripple,
        /// Keeps only the ranges, removing their complement within the target clips' extent.
        #[serde(default)]
        invert: bool,
        /// Ticks kept at each edge of every removed range; a negative value widens each range,
        /// which never starts before time 0.
        #[serde(default)]
        padding: MediaTime,
        /// Removed ranges shorter than this, after padding, are kept.
        #[serde(default)]
        min_duration: MediaTime,
    },
    /// Splits every target clip spanning a mapped time, except where a cut would change what
    /// the clip shows; those cuts are listed in [`EditReport::skipped`].
    SplitAt {
        /// An `intervals` data set's starts or an anchored list of ticks.
        times: Times,
        /// The clips split.
        target: Target,
    },
    /// Replaces bindings with plain values.
    Bake {
        /// What is baked.
        targets: Vec<BakeTarget>,
    },
}

/// Ranges an operation reads.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Ranges {
    /// An `intervals` data set of the project.
    Data(DataRef),
    /// A list on an anchor's clock.
    List(RangeList),
}

/// A data set an operation reads.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataRef {
    /// Its id.
    pub data: DataSetId,
}

/// Ranges given in the request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RangeList {
    /// The ranges as `{start, end}` ticks, each `start <= end`.
    pub list: Vec<RawRange>,
    /// The clock they are on; required.
    #[serde(default)]
    pub anchor: Option<DataAnchor>,
}

/// A range as written, before its order is checked.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawRange {
    /// Its start, in ticks.
    pub start: MediaTime,
    /// Its end, in ticks.
    pub end: MediaTime,
}

/// Times an operation reads.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Times {
    /// The starts of an `intervals` data set's items.
    Data(DataRef),
    /// A list of ticks on an anchor's clock.
    List(TimeList),
}

/// Times given in the request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeList {
    /// The times, in ticks.
    pub list: Vec<MediaTime>,
    /// The clock they are on; required.
    #[serde(default)]
    pub anchor: Option<DataAnchor>,
}

/// The clips an operation works on.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    /// Every clip of a track.
    Track(TrackId),
    /// These clips.
    Clips(Vec<ClipId>),
}

/// What closes the gaps a removal leaves.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ripple {
    /// Nothing closes them; the target clips lose the ranges and gaps stay.
    None,
    /// Every clip on the target clips' tracks loses the ranges and closes up; other tracks stay.
    Track,
    /// The ranges leave every track, so picture, sound and captions stay in sync.
    #[default]
    Timeline,
}

/// One value or caption track to bake.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum BakeTarget {
    /// A clip's property, such as `visual.position`, `masks[0].feather` or `audio.gain`.
    Clip {
        /// The clip.
        clip: ClipId,
        /// The property path.
        property: String,
    },
    /// A composition node's property, such as `effect.params.radius`.
    Node {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
        /// The property path.
        property: String,
    },
    /// A track's property: `captions`, `audio.gain` or `audio.pan`.
    Track {
        /// The track.
        track: TrackId,
        /// The property path.
        property: String,
    },
}

/// A kept stretch of timeline that moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Moved {
    /// Where it was.
    pub from: TimeRange,
    /// Where it is.
    pub to: TimeRange,
}

/// A clip an operation created by cutting another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Created {
    /// The new clip.
    pub id: ClipId,
    /// The clip it was cut from, which keeps its id on its first part.
    pub from: ClipId,
}

/// Why `split_at` left a cut out.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The cut, or a loop part boundary it makes, falls inside one of the clip's transitions.
    InTransition,
    /// The cut falls inside one of the clip's fades.
    InFade,
    /// The clip is another clip's matte, and that clip is not cut alike.
    MatteSource,
    /// A binding reads through the clip (`via`), and its owner is not cut alike.
    ReadThrough,
}

/// A cut `split_at` left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// The clip.
    pub clip: ClipId,
    /// The cut, in timeline time.
    pub time: MediaTime,
    /// Why.
    pub reason: SkipReason,
}

/// What an operation changed, so a caller can show and select it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EditReport {
    /// Timeline ranges removed, in the old timeline's time.
    pub removed: Vec<TimeRange>,
    /// Kept stretches that moved.
    pub moved: Vec<Moved>,
    /// Clips the operation created, with the clip each was cut from.
    pub created: Vec<Created>,
    /// Cuts `split_at` left out, in clip and time order; omitted when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
}

/// Why an operation refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditError {
    /// It names no data set of the project.
    UnknownData(DataSetId),
    /// The data set's kind does not fit the operation.
    WrongKind {
        /// The data set.
        data: DataSetId,
        /// Its kind.
        kind: DataKind,
    },
    /// It names a clip, track, composition or node the project lacks.
    UnknownTarget(String),
    /// The target names no clip.
    EmptyTarget,
    /// A list without an anchor.
    MissingAnchor,
    /// A range with its start after its end, by position.
    InvalidRange(usize),
    /// A bake target whose value is not bound to data (or a track with no words).
    NothingToBake(String),
    /// A correction has no keyframe equivalent.
    CorrectionNotBakeable(String),
    /// A bake target is a parameter of an effect the model has no range for (a host effect), so
    /// the live value the renderer clamps cannot be written.
    UnboundedParameter(String),
    /// The edit cuts a matte source where a clip it mattes is not cut, or removes it from under
    /// one, so no single part of it can stay that clip's matte.
    MatteSource(ClipId),
    /// The edit cuts a clip where a binding reading through it (`via`) is not cut, or removes it,
    /// so no single part of it can be read through.
    ReadThrough {
        /// The clip read through.
        clip: ClipId,
        /// The binding's owner, such as `clip b`, `track t` or `the master bus`.
        by: String,
    },
    /// A removed range's edge, or a part boundary the removal makes in a loop, falls strictly
    /// inside a transition of the clip it cuts.
    InTransition {
        /// The clip cut.
        clip: ClipId,
        /// Where, in timeline time.
        time: MediaTime,
    },
    /// A removed range's edge falls strictly inside a fade of the clip it cuts.
    InFade {
        /// The clip cut.
        clip: ClipId,
        /// Where, in timeline time.
        time: MediaTime,
    },
    /// The result would hold more clips on a track than a track holds.
    TooManyClips {
        /// The track.
        track: TrackId,
        /// The clips it would hold.
        found: usize,
        /// The most it holds.
        maximum: usize,
    },
    /// A caption bake's cues do not fit a track: more than it holds, or words past the limits of
    /// cues from words ([`alpha2_model::MAX_DERIVED_CUES`], [`alpha2_model::MAX_DATA_WORDS`]).
    TooManyCues {
        /// The track.
        track: TrackId,
        /// The cues it would hold; `None` when the words pass the limits of cues from words.
        found: Option<usize>,
        /// The most it holds.
        maximum: usize,
    },
    /// The edited project failed validation, which is a bug.
    InvalidResult(ModelError),
}

impl fmt::Display for EditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownData(data) => write!(formatter, "unknown data set {data}"),
            Self::WrongKind { data, kind } => write!(
                formatter,
                "data set {data} is {kind} data, which the operation does not take"
            ),
            Self::UnknownTarget(what) => write!(formatter, "unknown {what}"),
            Self::EmptyTarget => formatter.write_str("the target names no clip"),
            Self::MissingAnchor => formatter.write_str("a list needs an anchor"),
            Self::InvalidRange(index) => write!(formatter, "range {index} starts after it ends"),
            Self::NothingToBake(at) => write!(formatter, "{at} follows no data"),
            Self::CorrectionNotBakeable(at) => {
                write!(formatter, "{at} is a correction, which has no keyframes")
            }
            Self::UnboundedParameter(at) => write!(
                formatter,
                "{at} is a parameter of an effect whose range only the renderer knows"
            ),
            Self::MatteSource(clip) => write!(
                formatter,
                "clip {clip} is another clip's matte, and the edit cuts the two apart"
            ),
            Self::ReadThrough { clip, by } => write!(
                formatter,
                "{by} reads data through clip {clip}, and the edit cuts the two apart"
            ),
            Self::InTransition { clip, time } => write!(
                formatter,
                "the edit cuts clip {clip} at {} inside one of its transitions",
                time.ticks()
            ),
            Self::InFade { clip, time } => write!(
                formatter,
                "the edit cuts clip {clip} at {} inside one of its fades",
                time.ticks()
            ),
            Self::TooManyClips {
                track,
                found,
                maximum,
            } => write!(
                formatter,
                "the edit leaves {found} clips on track {track}; a track holds {maximum}"
            ),
            Self::TooManyCues {
                track,
                found: Some(found),
                maximum,
            } => write!(
                formatter,
                "the bake writes {found} cues on track {track}; a track holds {maximum}"
            ),
            Self::TooManyCues {
                track, found: None, ..
            } => write!(
                formatter,
                "the words of track {track} derive more than {} cues or map more than {} words",
                alpha2_model::MAX_DERIVED_CUES,
                alpha2_model::MAX_DATA_WORDS
            ),
            Self::InvalidResult(error) => {
                write!(formatter, "the edited project is invalid: {error}")
            }
        }
    }
}

impl std::error::Error for EditError {}

impl EditError {
    /// The refusal in snake case, as every binding reports it: `unknown_data`, `wrong_kind`,
    /// `unknown_target`, `empty_target`, `missing_anchor`, `invalid_range`, `nothing_to_bake`,
    /// `correction_not_bakeable`, `unbounded_parameter`, `matte_source`, `read_through`,
    /// `in_transition`, `in_fade`, `too_many_clips`, `too_many_cues` or `invalid_result`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UnknownData(_) => "unknown_data",
            Self::WrongKind { .. } => "wrong_kind",
            Self::UnknownTarget(_) => "unknown_target",
            Self::EmptyTarget => "empty_target",
            Self::MissingAnchor => "missing_anchor",
            Self::InvalidRange(_) => "invalid_range",
            Self::NothingToBake(_) => "nothing_to_bake",
            Self::CorrectionNotBakeable(_) => "correction_not_bakeable",
            Self::UnboundedParameter(_) => "unbounded_parameter",
            Self::MatteSource(_) => "matte_source",
            Self::ReadThrough { .. } => "read_through",
            Self::InTransition { .. } => "in_transition",
            Self::InFade { .. } => "in_fade",
            Self::TooManyClips { .. } => "too_many_clips",
            Self::TooManyCues { .. } => "too_many_cues",
            Self::InvalidResult(_) => "invalid_result",
        }
    }
}
