//! What an extractor declares, and what one run asks of it.

use alpha2_media::SourceId;
use alpha2_model::{DataInterpolation, DataKind, DataSetId};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The longest edge of a delivered video frame when the descriptor gives none.
pub const DEFAULT_MAX_SIZE: u32 = 960;
/// Audio frames per delivered block when the descriptor gives none.
pub const DEFAULT_BLOCK_FRAMES: u32 = 1024;
/// The range of `block_frames` a descriptor may declare.
pub const BLOCK_FRAMES_RANGE: std::ops::RangeInclusive<u32> = 64..=65_536;
/// Most outputs one extractor declares.
pub const MAX_EXTRACTOR_OUTPUTS: usize = 32;
/// Most passes one request asks for.
pub const MAX_EXTRACTOR_PASSES: usize = 64;

/// What an extractor declares: its identity, the input it reads and the outputs it emits.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    /// Reverse-DNS id; `alpha2.` is reserved for first-party plugins by convention.
    pub id: String,
    /// The output contract's version.
    pub version: u32,
    /// What the extractor reads.
    pub input: InputSpec,
    /// JSON Schema of the request's `params`, for interfaces and validation by surface helpers.
    #[serde(default = "empty_schema")]
    pub params_schema: serde_json::Value,
    /// The outputs it emits, by name.
    pub outputs: Vec<OutputSpec>,
}

fn empty_schema() -> serde_json::Value {
    serde_json::json!({})
}

/// What an extractor reads.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "media", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputSpec {
    /// Pictures.
    Video {
        /// The inputs it can read.
        from: Vec<InputFrom>,
        /// The pixel format delivered.
        format: VideoFormat,
        /// The longest edge a delivered frame may have, in pixels.
        #[serde(default = "default_max_size")]
        max_size: u32,
    },
    /// Sound.
    Audio {
        /// The inputs it can read.
        from: Vec<InputFrom>,
        /// The sample format delivered.
        format: AudioFormat,
        /// The rate it needs, if fixed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample_rate: Option<u32>,
        /// The channel layout it needs, if fixed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channels: Option<AudioChannels>,
        /// Frames per delivered block, in [`BLOCK_FRAMES_RANGE`].
        #[serde(default = "default_block_frames")]
        block_frames: u32,
    },
}

const fn default_max_size() -> u32 {
    DEFAULT_MAX_SIZE
}

const fn default_block_frames() -> u32 {
    DEFAULT_BLOCK_FRAMES
}

impl InputSpec {
    /// The inputs the extractor can read.
    #[must_use]
    pub fn from(&self) -> &[InputFrom] {
        match self {
            Self::Video { from, .. } | Self::Audio { from, .. } => from,
        }
    }
}

/// Which clock an extractor reads on.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputFrom {
    /// A source, in source time; its outputs are anchored to that source.
    Source,
    /// The rendered timeline, in timeline time; its outputs are anchored to the timeline.
    Timeline,
}

/// How delivered video frames are packed.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoFormat {
    /// Four bytes a pixel: straight-alpha, sRGB-encoded RGBA.
    Rgba8,
    /// One byte a pixel: BT.709 Y' of the sRGB-encoded RGB.
    Luma8,
}

impl VideoFormat {
    /// Bytes a pixel.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 => 4,
            Self::Luma8 => 1,
        }
    }
}

/// How delivered audio is stored.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// Planar 32-bit float, channel after channel.
    F32,
}

/// Which channels delivered audio has.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioChannels {
    /// One channel, the mixer's equal-weight downmix.
    Mono,
    /// The source's (or the mix's) own channels.
    Source,
}

/// One output an extractor emits.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// Its name, unique among the outputs.
    pub name: String,
    /// The kind of data it holds.
    pub kind: DataKind,
    /// Channel names, for `points` and `scalars`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// How the values read between samples, for the sampled kinds; `hold` when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpolation: Option<DataInterpolation>,
    /// How the output is usually used, for interfaces. The engine ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggest: Option<Suggest>,
}

impl OutputSpec {
    /// Numbers a sample of this output carries: channels times components, or zero for words
    /// and intervals.
    #[must_use]
    pub fn sample_width(&self) -> usize {
        match self.kind {
            DataKind::Points => self.channels.len() * 2,
            DataKind::Scalars => self.channels.len(),
            DataKind::Similarity => alpha2_model::SIMILARITY_ARITY,
            DataKind::Words | DataKind::Intervals => 0,
        }
    }
}

/// How an output is usually used.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Suggest {
    /// Values bind to it.
    Bind,
    /// `remove_ranges` applies it.
    RemoveRanges,
    /// `split_at` applies it.
    SplitAt,
}

/// One run of an extractor.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractRequest {
    /// The descriptor's id.
    pub extractor: String,
    /// The descriptor's version.
    pub version: u32,
    /// Parameters, validated against the descriptor's schema by the surface helper.
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// What is read.
    pub input: RequestInput,
    /// For each output recorded, the data set id it becomes; outputs left out are discarded.
    pub outputs: BTreeMap<String, DataSetId>,
}

/// What a run reads.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestInput {
    /// The clock read.
    pub from: InputFrom,
    /// The source, for source input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<SourceId>,
    /// The interval read, on that clock.
    pub range: TimeRange,
    /// The passes; none means one forward pass from the range start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<Pass>,
    /// The longest edge of delivered frames; at most the descriptor's `max_size`, its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u32>,
    /// The frame rate of timeline frames; the project's by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<FrameRate>,
    /// The source's audio stream ordinal, for source audio; the first by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<u32>,
    /// The audio rate; the descriptor's if fixed, else the project's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// The audio channels; the descriptor's if fixed, else mono.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<AudioChannels>,
}

/// One pass of a run, from a start time in one direction.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pass {
    /// Where the pass starts, inside the range.
    pub start: MediaTime,
    /// Which way it runs.
    pub direction: Direction,
}

/// The direction of a pass.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Toward the range end.
    Forward,
    /// Toward the range start.
    Backward,
}
