//! Data sets: values an extractor derived from media, recorded in the project and anchored to a
//! source's clock or the timeline's.
//!
//! The engine consumes data in two ways. Values and caption tracks bind to a data set and
//! follow it at render time through the same time map the picture uses; edit operations apply
//! interval data to the document once. No extractor runs while a frame renders.

use crate::{ClipId, DataSetId, MediaFingerprint, NodeId};
use alpha2_media::SourceId;
use alpha2_time::MediaTime;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Most data sets in one project.
pub const MAX_DATA_SETS: usize = 256;
/// Most channels of one `points` or `scalars` data set.
pub const MAX_DATA_CHANNELS: usize = 64;
/// Most values (numbers or lost markers) of one sampled data set.
pub const MAX_DATA_VALUES: usize = 4_000_000;
/// Most words of one `words` data set.
pub const MAX_DATA_WORDS: usize = 200_000;
/// Most intervals of one `intervals` data set.
pub const MAX_DATA_INTERVALS: usize = 200_000;
/// Most bytes of an interval label, a word's text or speaker, or a channel name.
pub const MAX_DATA_LABEL_BYTES: usize = 256;
/// Most bytes of a data set's id.
pub const MAX_DATA_ID_BYTES: usize = 64;
/// Largest magnitude of a normalized coordinate (points, similarity translation).
pub const MAX_DATA_COORDINATE: f32 = 16.0;
/// Largest magnitude of a scalar or an interval score.
pub const MAX_DATA_SCALAR: f32 = 1e9;
/// Largest magnitude of a similarity rotation, in degrees.
pub const MAX_DATA_ROTATION_DEGREES: f32 = 36_000.0;
/// Largest similarity scale; the smallest is just above zero.
pub const MAX_DATA_SCALE: f32 = 1_000.0;

/// The clock a data set's times are on.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataAnchor {
    /// Source time: the presentation times the source's pictures and sound carry once decoded.
    /// Points are normalized to the displayed source picture.
    Source {
        /// The source.
        source_id: SourceId,
    },
    /// Timeline time. Points are normalized to the canvas.
    Timeline,
}

impl DataAnchor {
    /// The anchored source, if any.
    #[must_use]
    pub const fn source(&self) -> Option<&SourceId> {
        match self {
            Self::Source { source_id } => Some(source_id),
            Self::Timeline => None,
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "DataAnchor"))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum DataAnchorWire {
    /// Source time: the presentation times the source's pictures and sound carry once decoded.
    /// Points are normalized to the displayed source picture.
    Source {
        /// The source.
        source_id: SourceId,
    },
    /// Timeline time. Points are normalized to the canvas.
    Timeline {},
}

impl<'de> Deserialize<'de> for DataAnchor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match DataAnchorWire::deserialize(deserializer)? {
            DataAnchorWire::Source { source_id } => Self::Source { source_id },
            DataAnchorWire::Timeline {} => Self::Timeline,
        })
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for DataAnchor {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        DataAnchorWire::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        DataAnchorWire::json_schema(generator)
    }
}

/// How a sampled data set reads between its samples.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DataInterpolation {
    /// The sample at or before the time: the rule of the picture shown at that time.
    #[default]
    Hold,
    /// A straight line between the samples around the time.
    Linear,
}

/// What a data set holds; each kind has a consumer in the engine.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DataKind {
    /// Named channels of normalized `(x, y)` points.
    Points,
    /// Named channels of numbers.
    Scalars,
    /// One translation, rotation and uniform scale per sample.
    Similarity,
    /// Timed words with text.
    Words,
    /// Timed ranges, a zero-length one being an instant.
    Intervals,
}

impl DataKind {
    /// Numbers per channel and sample of a sampled kind; `None` for words and intervals.
    #[must_use]
    pub const fn arity(self) -> Option<usize> {
        match self {
            Self::Points => Some(2),
            Self::Scalars => Some(1),
            Self::Similarity => Some(SIMILARITY_ARITY),
            Self::Words | Self::Intervals => None,
        }
    }

    /// The kind's wire name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Scalars => "scalars",
            Self::Similarity => "similarity",
            Self::Words => "words",
            Self::Intervals => "intervals",
        }
    }
}

impl fmt::Display for DataKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Numbers per similarity sample: `tx`, `ty`, rotation in degrees, scale.
pub const SIMILARITY_ARITY: usize = 4;

/// Samples on one time grid, stored by column.
///
/// `values` holds `times.len() * channels * arity` numbers: sample after sample, channel after
/// channel within a sample, components within a channel. `None` marks a channel lost at that
/// sample, and covers all of its components.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DataSamples {
    /// How the values read between samples.
    pub interpolation: DataInterpolation,
    /// Channel names, unique; empty for `similarity`.
    pub channels: Vec<String>,
    /// Sample times in ticks on the anchor's clock, strictly ascending.
    pub times: Vec<MediaTime>,
    /// The values, as described on the type.
    pub values: Vec<Option<f32>>,
}

impl DataSamples {
    /// The position of `channel`, if the set has it.
    #[must_use]
    pub fn channel(&self, channel: &str) -> Option<usize> {
        self.channels.iter().position(|name| name == channel)
    }
}

/// One timed word.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DataWord {
    /// When the word starts, in ticks on the anchor's clock.
    pub start: MediaTime,
    /// When it ends; after `start`.
    pub end: MediaTime,
    /// The word, non-empty, at most [`MAX_DATA_LABEL_BYTES`] bytes.
    pub text: String,
    /// Who says it, at most [`MAX_DATA_LABEL_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// One timed range; `start == end` is an instant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DataInterval {
    /// When it starts, in ticks on the anchor's clock.
    pub start: MediaTime,
    /// When it ends; not before `start`.
    pub end: MediaTime,
    /// A free label, at most [`MAX_DATA_LABEL_BYTES`] bytes. Edit operations ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A free score, finite, at most [`MAX_DATA_SCALAR`] in magnitude. Edit operations ignore
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

/// The values of a data set, by kind.
#[derive(Clone, Debug, PartialEq)]
pub enum DataValues {
    /// Channels of `(x, y)` points.
    Points(DataSamples),
    /// Channels of numbers.
    Scalars(DataSamples),
    /// `tx`, `ty`, rotation in degrees and scale per sample, with no channels.
    Similarity(DataSamples),
    /// Words sorted by `(start, end)`; they may overlap.
    Words(Vec<DataWord>),
    /// Intervals sorted by `(start, end)`; they may overlap.
    Intervals(Vec<DataInterval>),
}

impl DataValues {
    /// The kind of these values.
    #[must_use]
    pub const fn kind(&self) -> DataKind {
        match self {
            Self::Points(_) => DataKind::Points,
            Self::Scalars(_) => DataKind::Scalars,
            Self::Similarity(_) => DataKind::Similarity,
            Self::Words(_) => DataKind::Words,
            Self::Intervals(_) => DataKind::Intervals,
        }
    }

    /// The samples of a sampled kind.
    #[must_use]
    pub const fn samples(&self) -> Option<&DataSamples> {
        match self {
            Self::Points(samples) | Self::Scalars(samples) | Self::Similarity(samples) => {
                Some(samples)
            }
            Self::Words(_) | Self::Intervals(_) => None,
        }
    }
}

/// Which extractor made a data set. Recorded for hosts; the engine never reads it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MadeBy {
    /// The extractor's reverse-DNS id.
    pub extractor: String,
    /// The extractor's output-contract version.
    pub version: u32,
    /// The parameters it ran with.
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// The plugin that ran, when the extractor was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<MadeByPlugin>,
}

/// The plugin build that made a data set.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MadeByPlugin {
    /// The package's semantic version.
    pub version: String,
    /// SHA-256 of the artifact that ran, as 64 lowercase hexadecimal digits.
    pub artifact_sha256: String,
}

/// Values derived from media, anchored to a source or to the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct DataSet {
    /// Stable identity; at most [`MAX_DATA_ID_BYTES`] bytes.
    pub id: DataSetId,
    /// The clock the times are on.
    pub anchor: DataAnchor,
    /// The values.
    pub values: DataValues,
    /// Which extractor made it.
    pub made_by: Option<MadeBy>,
    /// The original source's fingerprint when the data was made; absent for timeline anchors.
    /// Hosts compare it with the source's current one to report stale data.
    pub made_from: Option<MediaFingerprint>,
}

impl DataSet {
    /// The kind of the values.
    #[must_use]
    pub const fn kind(&self) -> DataKind {
        self.values.kind()
    }
}

// The schema of a `DataSet`: one flat object per kind with only that kind's fields.
/// Values derived from media, anchored to a source's clock or the timeline's.
#[cfg(feature = "schema")]
#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
#[schemars(rename = "DataSet")]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DataSetSchema {
    /// Named channels of normalized `(x, y)` points, each pair `null` when lost.
    Points {
        /// Stable identity, 1 to 64 bytes.
        id: DataSetId,
        /// The clock the times are on.
        anchor: DataAnchor,
        /// How the values read between samples.
        interpolation: DataInterpolation,
        /// Channel names, unique, 1 to 256 bytes each.
        channels: Vec<String>,
        /// Sample times in ticks on the anchor's clock, strictly ascending.
        times: Vec<MediaTime>,
        /// `times.len() * channels.len() * 2` numbers, sample after sample, channel after channel.
        values: Vec<Option<f32>>,
        /// Which extractor made it.
        #[serde(default)]
        made_by: Option<MadeBy>,
        /// The original source's fingerprint when it was made.
        #[serde(default)]
        made_from: Option<MediaFingerprint>,
    },
    /// Named channels of numbers, each `null` when lost.
    Scalars {
        /// Stable identity, 1 to 64 bytes.
        id: DataSetId,
        /// The clock the times are on.
        anchor: DataAnchor,
        /// How the values read between samples.
        interpolation: DataInterpolation,
        /// Channel names, unique, 1 to 256 bytes each.
        channels: Vec<String>,
        /// Sample times in ticks on the anchor's clock, strictly ascending.
        times: Vec<MediaTime>,
        /// `times.len() * channels.len()` numbers, sample after sample, channel after channel.
        values: Vec<Option<f32>>,
        /// Which extractor made it.
        #[serde(default)]
        made_by: Option<MadeBy>,
        /// The original source's fingerprint when it was made.
        #[serde(default)]
        made_from: Option<MediaFingerprint>,
    },
    /// Translation (normalized), rotation (degrees) and uniform scale about the picture's
    /// center, per sample; four `null`s when lost.
    Similarity {
        /// Stable identity, 1 to 64 bytes.
        id: DataSetId,
        /// The clock the times are on.
        anchor: DataAnchor,
        /// How the values read between samples.
        interpolation: DataInterpolation,
        /// Sample times in ticks on the anchor's clock, strictly ascending.
        times: Vec<MediaTime>,
        /// `times.len() * 4` numbers: `tx`, `ty`, rotation, scale per sample.
        values: Vec<Option<f32>>,
        /// Which extractor made it.
        #[serde(default)]
        made_by: Option<MadeBy>,
        /// The original source's fingerprint when it was made.
        #[serde(default)]
        made_from: Option<MediaFingerprint>,
    },
    /// Timed words.
    Words {
        /// Stable identity, 1 to 64 bytes.
        id: DataSetId,
        /// The clock the times are on.
        anchor: DataAnchor,
        /// Words sorted by `(start, end)`.
        words: Vec<DataWord>,
        /// Which extractor made it.
        #[serde(default)]
        made_by: Option<MadeBy>,
        /// The original source's fingerprint when it was made.
        #[serde(default)]
        made_from: Option<MediaFingerprint>,
    },
    /// Timed ranges.
    Intervals {
        /// Stable identity, 1 to 64 bytes.
        id: DataSetId,
        /// The clock the times are on.
        anchor: DataAnchor,
        /// Intervals sorted by `(start, end)`.
        intervals: Vec<DataInterval>,
        /// Which extractor made it.
        #[serde(default)]
        made_by: Option<MadeBy>,
        /// The original source's fingerprint when it was made.
        #[serde(default)]
        made_from: Option<MediaFingerprint>,
    },
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for DataSet {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        DataSetSchema::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        DataSetSchema::json_schema(generator)
    }
}

/// Every field any kind has, read without buffering, then checked against the kind.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DataSetFlat {
    id: DataSetId,
    anchor: DataAnchor,
    kind: DataKind,
    #[serde(default)]
    interpolation: Option<DataInterpolation>,
    #[serde(default)]
    channels: Option<Vec<String>>,
    #[serde(default)]
    times: Option<Vec<MediaTime>>,
    #[serde(default)]
    values: Option<Vec<Option<f32>>>,
    #[serde(default)]
    words: Option<Vec<DataWord>>,
    #[serde(default)]
    intervals: Option<Vec<DataInterval>>,
    #[serde(default)]
    made_by: Option<MadeBy>,
    #[serde(default)]
    made_from: Option<MediaFingerprint>,
}

impl TryFrom<DataSetFlat> for DataSet {
    type Error = String;

    fn try_from(flat: DataSetFlat) -> Result<Self, Self::Error> {
        let kind = flat.kind;
        let stray = |field: &str| format!("a {kind} data set has no field `{field}`");
        let missing = |field: &str| format!("a {kind} data set needs `{field}`");
        let values = match kind {
            DataKind::Points | DataKind::Scalars | DataKind::Similarity => {
                if flat.words.is_some() {
                    return Err(stray("words"));
                }
                if flat.intervals.is_some() {
                    return Err(stray("intervals"));
                }
                let channels = if kind == DataKind::Similarity {
                    if flat.channels.is_some() {
                        return Err(stray("channels"));
                    }
                    Vec::new()
                } else {
                    flat.channels.ok_or_else(|| missing("channels"))?
                };
                let samples = DataSamples {
                    interpolation: flat.interpolation.ok_or_else(|| missing("interpolation"))?,
                    channels,
                    times: flat.times.ok_or_else(|| missing("times"))?,
                    values: flat.values.ok_or_else(|| missing("values"))?,
                };
                match kind {
                    DataKind::Points => DataValues::Points(samples),
                    DataKind::Scalars => DataValues::Scalars(samples),
                    _ => DataValues::Similarity(samples),
                }
            }
            DataKind::Words | DataKind::Intervals => {
                for (field, present) in [
                    ("interpolation", flat.interpolation.is_some()),
                    ("channels", flat.channels.is_some()),
                    ("times", flat.times.is_some()),
                    ("values", flat.values.is_some()),
                ] {
                    if present {
                        return Err(stray(field));
                    }
                }
                if kind == DataKind::Words {
                    if flat.intervals.is_some() {
                        return Err(stray("intervals"));
                    }
                    DataValues::Words(flat.words.ok_or_else(|| missing("words"))?)
                } else {
                    if flat.words.is_some() {
                        return Err(stray("words"));
                    }
                    DataValues::Intervals(flat.intervals.ok_or_else(|| missing("intervals"))?)
                }
            }
        };
        Ok(Self {
            id: flat.id,
            anchor: flat.anchor,
            values,
            made_by: flat.made_by,
            made_from: flat.made_from,
        })
    }
}

impl<'de> Deserialize<'de> for DataSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let flat = DataSetFlat::deserialize(deserializer)?;
        Self::try_from(flat).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize)]
struct DataSetRef<'a> {
    id: &'a DataSetId,
    anchor: &'a DataAnchor,
    kind: DataKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    interpolation: Option<DataInterpolation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channels: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    times: Option<&'a [MediaTime]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<&'a [Option<f32>]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    words: Option<&'a [DataWord]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intervals: Option<&'a [DataInterval]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    made_by: Option<&'a MadeBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    made_from: Option<&'a MediaFingerprint>,
}

impl Serialize for DataSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut wire = DataSetRef {
            id: &self.id,
            anchor: &self.anchor,
            kind: self.kind(),
            interpolation: None,
            channels: None,
            times: None,
            values: None,
            words: None,
            intervals: None,
            made_by: self.made_by.as_ref(),
            made_from: self.made_from.as_ref(),
        };
        match &self.values {
            DataValues::Points(samples)
            | DataValues::Scalars(samples)
            | DataValues::Similarity(samples) => {
                wire.interpolation = Some(samples.interpolation);
                if self.kind() != DataKind::Similarity {
                    wire.channels = Some(&samples.channels);
                }
                wire.times = Some(&samples.times);
                wire.values = Some(&samples.values);
            }
            DataValues::Words(words) => wire.words = Some(words),
            DataValues::Intervals(intervals) => wire.intervals = Some(intervals),
        }
        wire.serialize(serializer)
    }
}

/// A value that follows a channel of a data set at render time: the `data` value mode.
///
/// A source-anchored set is read through a reader (`via`) whose time map gives source time, so
/// the value follows the picture through any speed, reverse, loop or freeze.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DataBinding {
    /// The data set, a `points` or `scalars` one.
    pub data: DataSetId,
    /// The channel followed.
    pub channel: String,
    /// The reader whose time map gives source time: required for a source-anchored set,
    /// forbidden for a timeline-anchored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<DataVia>,
    /// Where a point is read (points with `via` only, where it is required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<DataSpace>,
    /// `[x, y]` added after the space mapping, in that space's normalized units (points only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<[f32; 2]>,
    /// A linear map of the value (scalars only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap: Option<DataRemap>,
}

/// The reader whose clock maps time onto a source-anchored data set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DataVia {
    /// A timeline clip showing the anchor's source; its picture time map gives source time.
    Clip(ClipId),
    /// A `media` node of the same composition reading the anchor's source; its start plus the
    /// composition time gives source time.
    Node(NodeId),
}

/// Where a bound point is read.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DataSpace {
    /// The normalized source picture, unchanged: effect params and the clip's own masks.
    Picture,
    /// Normalized canvas coordinates through the `via` clip's presentation and evaluated
    /// visual: another clip's position.
    Canvas,
}

/// A linear map from `input` to `output`, clamped to `output`.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DataRemap {
    /// The input values mapped to `output[0]` and `output[1]`; distinct and finite.
    pub input: [f32; 2],
    /// The output range; finite.
    pub output: [f32; 2],
}

impl DataRemap {
    /// Maps `value`, clamped to the output range.
    #[must_use]
    pub fn apply(&self, value: f32) -> f32 {
        let [in0, in1] = self.input.map(f64::from);
        let [out0, out1] = self.output.map(f64::from);
        let mapped = out0 + (f64::from(value) - in0) * (out1 - out0) / (in1 - in0);
        let (low, high) = if out0 <= out1 {
            (out0, out1)
        } else {
            (out1, out0)
        };
        #[allow(clippy::cast_possible_truncation)]
        let clamped = mapped.clamp(low, high) as f32;
        clamped
    }

    // Equal inputs give no slope; any two distinct values do.
    #[allow(clippy::float_cmp)]
    fn is_valid(&self) -> bool {
        self.input
            .iter()
            .chain(&self.output)
            .all(|value| value.is_finite())
            && self.input[0] != self.input[1]
    }
}

/// A `similarity` data set applied to a clip's own picture before its transform.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CorrectionRef {
    /// The data set, a `similarity` one anchored to the clip's source.
    pub data: DataSetId,
}

/// Caption cues built from a `words` data set.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CaptionsFromWords {
    /// The data set, a `words` one.
    pub data: DataSetId,
    /// For a source-anchored set, the track whose clips showing the source give the words their
    /// timeline times; forbidden for a timeline-anchored set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<crate::TrackId>,
    /// Most characters of one cue, at least 1.
    pub max_chars: u32,
}

/// What is wrong with a data set.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DataProblem {
    /// The id is longer than [`MAX_DATA_ID_BYTES`].
    IdTooLong,
    /// The anchor names no source of the project.
    UnknownSource(SourceId),
    /// The anchor names a source that has no media clock (a generated or composition source).
    SourceWithoutMedia(SourceId),
    /// More than [`MAX_DATA_CHANNELS`] channels, or none.
    ChannelCount,
    /// A channel name is empty, too long or repeated.
    ChannelName,
    /// More than [`MAX_DATA_VALUES`] values.
    TooManyValues,
    /// `values` does not hold exactly `times * channels * arity` entries.
    ValueCount,
    /// Times are not strictly ascending.
    TimesNotAscending,
    /// A time is negative.
    NegativeTime,
    /// A time is past the source fingerprint's duration.
    PastSourceEnd,
    /// A value is not finite, outside its kind's bounds, or a lost marker covers only part of a
    /// channel or sample.
    InvalidValue {
        /// The value's position in `values`.
        index: usize,
    },
    /// More than [`MAX_DATA_WORDS`] words or [`MAX_DATA_INTERVALS`] intervals.
    TooManyItems,
    /// A word or interval is out of order, has its end before its start (or at it, for a
    /// word), empty word text, a score out of bounds, or a text too long.
    InvalidItem {
        /// The item's position.
        index: usize,
    },
    /// `made_by` has an empty extractor id or a malformed plugin hash.
    InvalidMadeBy,
    /// `made_from` has a malformed part (by field name), or is present on a timeline anchor.
    InvalidMadeFrom(&'static str),
}

impl fmt::Display for DataProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdTooLong => write!(formatter, "has an id over {MAX_DATA_ID_BYTES} bytes"),
            Self::UnknownSource(source) => {
                write!(formatter, "is anchored to unknown source {source}")
            }
            Self::SourceWithoutMedia(source) => {
                write!(
                    formatter,
                    "is anchored to source {source}, which has no media"
                )
            }
            Self::ChannelCount => {
                write!(formatter, "needs 1 to {MAX_DATA_CHANNELS} channels")
            }
            Self::ChannelName => {
                formatter.write_str("has an empty, overlong or repeated channel name")
            }
            Self::TooManyValues => write!(formatter, "has more than {MAX_DATA_VALUES} values"),
            Self::ValueCount => {
                formatter.write_str("does not have times * channels * arity values")
            }
            Self::TimesNotAscending => {
                formatter.write_str("has times that are not strictly ascending")
            }
            Self::NegativeTime => formatter.write_str("has a negative time"),
            Self::PastSourceEnd => formatter.write_str("has a time past its source's duration"),
            Self::InvalidValue { index } => {
                write!(formatter, "has an invalid value at values[{index}]")
            }
            Self::TooManyItems => write!(
                formatter,
                "has more than {MAX_DATA_WORDS} words or {MAX_DATA_INTERVALS} intervals"
            ),
            Self::InvalidItem { index } => write!(formatter, "has an invalid item at [{index}]"),
            Self::InvalidMadeBy => formatter.write_str("has an invalid made_by"),
            Self::InvalidMadeFrom(part) => write!(formatter, "has an invalid made_from.{part}"),
        }
    }
}

/// What is wrong with a [`DataBinding`], a [`CorrectionRef`] or a [`CaptionsFromWords`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BindingProblem {
    /// It names no data set of the project.
    UnknownData,
    /// The data set's kind does not fit the place's type.
    WrongKind(DataKind),
    /// The value's type takes no data (an effect parameter that is not a Float or Vec2).
    TypeRefusesData,
    /// The place's clock refuses this anchor: a composition-time place and a timeline anchor,
    /// or a picture-time place and any anchor.
    ClockRefusesAnchor,
    /// The channel is not one of the data set's.
    UnknownChannel,
    /// A source-anchored set needs `via`.
    ViaRequired,
    /// A timeline-anchored set takes no `via`.
    ViaForbidden,
    /// `via` names a clip at a composition-time place, or a node at a timeline-time place.
    ViaWrongReader,
    /// `via` names no timeline clip.
    UnknownViaClip,
    /// `via` names no node of the composition, or one that is not a `media` node.
    InvalidViaNode,
    /// The reader does not show the data set's source.
    ViaShowsOtherSource,
    /// A point read through `via` needs `space`.
    SpaceRequired,
    /// `space`, `offset` or `remap` where it does not apply, or `canvas` through a node.
    FieldNotApplicable(&'static str),
    /// `remap` has a non-finite value or equal inputs; `offset` a non-finite component.
    InvalidRemap,
    /// Following clips in canvas space forms a cycle.
    Cycle,
    /// A correction's data set is not anchored to the clip's own source.
    NotOwnSource,
    /// A caption track's `track` is missing for a source anchor, given for a timeline anchor,
    /// or names no visual or audio track; or `max_chars` is zero.
    InvalidCaptionTrack,
}

impl fmt::Display for BindingProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownData => formatter.write_str("names an unknown data set"),
            Self::WrongKind(kind) => write!(formatter, "cannot follow {kind} data"),
            Self::TypeRefusesData => formatter.write_str("is a value whose type takes no data"),
            Self::ClockRefusesAnchor => {
                formatter.write_str("is keyed on a clock that refuses this anchor")
            }
            Self::UnknownChannel => formatter.write_str("names an unknown channel"),
            Self::ViaRequired => formatter.write_str("needs `via` for a source-anchored data set"),
            Self::ViaForbidden => {
                formatter.write_str("takes no `via` for a timeline-anchored data set")
            }
            Self::ViaWrongReader => {
                formatter.write_str("names a reader of the wrong kind for its clock")
            }
            Self::UnknownViaClip => formatter.write_str("reads through an unknown clip"),
            Self::InvalidViaNode => formatter
                .write_str("reads through a node that is not a media node of its composition"),
            Self::ViaShowsOtherSource => {
                formatter.write_str("reads through a reader of another source")
            }
            Self::SpaceRequired => {
                formatter.write_str("needs `space` for a point read through `via`")
            }
            Self::FieldNotApplicable(field) => {
                write!(formatter, "has `{field}` where it does not apply")
            }
            Self::InvalidRemap => formatter
                .write_str("has a non-finite offset or remap, or a remap with equal inputs"),
            Self::Cycle => formatter.write_str("follows clips in canvas space in a cycle"),
            Self::NotOwnSource => {
                formatter.write_str("corrects with data anchored to another source")
            }
            Self::InvalidCaptionTrack => {
                formatter.write_str("has an invalid words track or max_chars")
            }
        }
    }
}

pub(crate) fn valid_label(text: &str) -> bool {
    text.len() <= MAX_DATA_LABEL_BYTES
}

fn valid_sha256(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `value` is inside its kind's bounds; `component` is the position within a sample's
/// numbers for a similarity set.
#[must_use]
pub fn value_in_bounds(kind: DataKind, component: usize, value: f32) -> bool {
    value.is_finite()
        && match kind {
            DataKind::Points => value.abs() <= MAX_DATA_COORDINATE,
            DataKind::Similarity => match component {
                0 | 1 => value.abs() <= MAX_DATA_COORDINATE,
                2 => value.abs() <= MAX_DATA_ROTATION_DEGREES,
                _ => value > 0.0 && value <= MAX_DATA_SCALE,
            },
            DataKind::Scalars | DataKind::Words | DataKind::Intervals => {
                value.abs() <= MAX_DATA_SCALAR
            }
        }
}

/// Checks a word's own fields (order against its neighbours is checked by the caller).
#[must_use]
pub fn valid_word(word: &DataWord) -> bool {
    word.start.ticks() >= 0
        && word.start < word.end
        && !word.text.is_empty()
        && valid_label(&word.text)
        && word.speaker.as_deref().is_none_or(valid_label)
}

/// Checks an interval's own fields (order against its neighbours is checked by the caller).
#[must_use]
pub fn valid_interval(interval: &DataInterval) -> bool {
    interval.start.ticks() >= 0
        && interval.start <= interval.end
        && interval.label.as_deref().is_none_or(valid_label)
        && interval
            .score
            .is_none_or(|score| value_in_bounds(DataKind::Scalars, 0, score))
}

impl DataSet {
    /// Checks the set against the model's rules. `source_duration` is the anchored source's
    /// fingerprint duration in ticks, when known.
    pub(crate) fn check(&self, source_duration: Option<i64>) -> Result<(), DataProblem> {
        if self.id.as_str().len() > MAX_DATA_ID_BYTES {
            return Err(DataProblem::IdTooLong);
        }
        if let Some(made_by) = &self.made_by {
            let plugin_ok = made_by.plugin.as_ref().is_none_or(|plugin| {
                !plugin.version.is_empty() && valid_sha256(&plugin.artifact_sha256)
            });
            if made_by.extractor.is_empty() || !plugin_ok {
                return Err(DataProblem::InvalidMadeBy);
            }
        }
        if let Some(made_from) = &self.made_from {
            if self.anchor == DataAnchor::Timeline {
                return Err(DataProblem::InvalidMadeFrom("anchor"));
            }
            if let Some(part) = made_from.malformed_part() {
                return Err(DataProblem::InvalidMadeFrom(part));
            }
        }
        let time_ok = |time: MediaTime| -> Result<(), DataProblem> {
            if time.ticks() < 0 {
                return Err(DataProblem::NegativeTime);
            }
            if source_duration.is_some_and(|end| time.ticks() > end) {
                return Err(DataProblem::PastSourceEnd);
            }
            Ok(())
        };
        let kind = self.kind();
        match &self.values {
            DataValues::Points(samples)
            | DataValues::Scalars(samples)
            | DataValues::Similarity(samples) => {
                check_samples(kind, samples, &time_ok)?;
            }
            DataValues::Words(words) => {
                if words.len() > MAX_DATA_WORDS {
                    return Err(DataProblem::TooManyItems);
                }
                for (index, word) in words.iter().enumerate() {
                    let ordered = index == 0 || {
                        let before = &words[index - 1];
                        (before.start, before.end) <= (word.start, word.end)
                    };
                    if !valid_word(word) || !ordered {
                        return Err(DataProblem::InvalidItem { index });
                    }
                    time_ok(word.start)?;
                    time_ok(word.end)?;
                }
            }
            DataValues::Intervals(intervals) => {
                if intervals.len() > MAX_DATA_INTERVALS {
                    return Err(DataProblem::TooManyItems);
                }
                for (index, interval) in intervals.iter().enumerate() {
                    let ordered = index == 0 || {
                        let before = &intervals[index - 1];
                        (before.start, before.end) <= (interval.start, interval.end)
                    };
                    if !valid_interval(interval) || !ordered {
                        return Err(DataProblem::InvalidItem { index });
                    }
                    time_ok(interval.start)?;
                    time_ok(interval.end)?;
                }
            }
        }
        Ok(())
    }
}

fn check_samples(
    kind: DataKind,
    samples: &DataSamples,
    time_ok: &dyn Fn(MediaTime) -> Result<(), DataProblem>,
) -> Result<(), DataProblem> {
    let arity = kind.arity().unwrap_or(1);
    let channels = if kind == DataKind::Similarity {
        1
    } else {
        if samples.channels.is_empty() || samples.channels.len() > MAX_DATA_CHANNELS {
            return Err(DataProblem::ChannelCount);
        }
        for (index, name) in samples.channels.iter().enumerate() {
            if name.is_empty() || !valid_label(name) || samples.channels[..index].contains(name) {
                return Err(DataProblem::ChannelName);
            }
        }
        samples.channels.len()
    };
    if samples.values.len() > MAX_DATA_VALUES {
        return Err(DataProblem::TooManyValues);
    }
    let expected = samples
        .times
        .len()
        .checked_mul(channels * arity)
        .ok_or(DataProblem::ValueCount)?;
    if samples.values.len() != expected {
        return Err(DataProblem::ValueCount);
    }
    for (index, time) in samples.times.iter().enumerate() {
        if index > 0 && samples.times[index - 1] >= *time {
            return Err(DataProblem::TimesNotAscending);
        }
        time_ok(*time)?;
    }
    // A lost marker covers a whole channel of a sample: all of its components or none.
    for (group, values) in samples.values.chunks_exact(arity).enumerate() {
        let lost = values[0].is_none();
        for (component, value) in values.iter().enumerate() {
            let index = group * arity + component;
            let ok = match value {
                None => lost,
                Some(value) => !lost && value_in_bounds(kind, component, *value),
            };
            if !ok {
                return Err(DataProblem::InvalidValue { index });
            }
        }
    }
    Ok(())
}

impl DataBinding {
    /// Checks the fields that need no project: finite offset and a usable remap.
    pub(crate) fn fields_valid(&self) -> bool {
        self.offset
            .is_none_or(|offset| offset.iter().all(|value| value.is_finite()))
            && self.remap.is_none_or(|remap| remap.is_valid())
    }
}
