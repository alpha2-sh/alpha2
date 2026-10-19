//! What a source's media was when the project recorded it: evidence a host
//! compares with the bytes it finds, never a locator.

use alpha2_media::ColorMetadata;
use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

/// Evidence of a source's media, each part recorded only when it was known.
///
/// A matching content hash proves the same bytes. Without one, the other
/// parts can only show a difference. `modified_ms` narrows a search for
/// candidates and never decides whether a candidate is the same media.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MediaFingerprint {
    /// SHA-256 of the whole file, as 64 lowercase hexadecimal digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// The file's length in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    /// The file's modification time, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<i64>,
    /// The streams the file carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streams: Option<StreamTopology>,
    /// Where the media ends on its own timeline, in ticks: its first
    /// presentation plus its length, the end of any source range that reads
    /// it. A transport stream that starts at 1.4 s and lasts 2 s ends at
    /// 3.4 s. Non-negative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ticks: Option<i64>,
    /// The color the decoded pictures carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<ColorMetadata>,
}

impl MediaFingerprint {
    /// Whether every recorded part is well formed.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.malformed_part().is_none()
    }

    /// The first recorded part that is not well formed, by its field name:
    /// a hash that is not 64 lowercase hexadecimal digits, a negative
    /// duration, a stream without size, channels or rate, or a color whose
    /// dynamic range contradicts its transfer.
    #[must_use]
    pub fn malformed_part(&self) -> Option<&'static str> {
        if self
            .content_sha256
            .as_deref()
            .is_some_and(|hash| !is_sha256_hex(hash))
        {
            Some("content_sha256")
        } else if self.duration_ticks.is_some_and(|ticks| ticks < 0) {
            Some("duration_ticks")
        } else if self
            .streams
            .as_ref()
            .is_some_and(|streams| !streams.is_valid())
        {
            Some("streams")
        } else if self.color.is_some_and(|color| !color.is_consistent()) {
            Some("color")
        } else {
            None
        }
    }
}

/// The streams a media file carries.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StreamTopology {
    /// The primary video stream, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoStreamShape>,
    /// The primary audio stream, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioStreamShape>,
}

impl StreamTopology {
    fn is_valid(&self) -> bool {
        self.video
            .as_ref()
            .is_none_or(|video| video.width > 0 && video.height > 0)
            && self
                .audio
                .as_ref()
                .is_none_or(|audio| audio.channels > 0 && audio.sample_rate > 0)
    }
}

/// A video stream's displayed size and frame rate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VideoStreamShape {
    /// Displayed width in square pixels: the stored width stretched by a
    /// pixel aspect ratio wider than tall, then turned by the stream's
    /// orientation.
    pub width: u32,
    /// Displayed height in square pixels: the stored height stretched by a
    /// pixel aspect ratio taller than wide, then turned.
    pub height: u32,
    /// The rate its first frames are presented at, as every host measures
    /// it from packet times and rounds it (`alpha2_assets::FrameRateEvidence`),
    /// when it can be measured. Not the rate a stream declares: a declared
    /// rate is a guess for variable-rate media, and browsers report none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<FrameRate>,
}

impl VideoStreamShape {
    /// A stream of `width` x `height` pictures at `frame_rate`.
    #[must_use]
    pub const fn new(width: u32, height: u32, frame_rate: Option<FrameRate>) -> Self {
        Self {
            width,
            height,
            frame_rate,
        }
    }
}

/// An audio stream's channel count and sample rate.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioStreamShape {
    /// Channel count.
    pub channels: u32,
    /// Samples per second per channel.
    pub sample_rate: u32,
}

impl AudioStreamShape {
    /// A stream of `channels` channels at `sample_rate`.
    #[must_use]
    pub const fn new(channels: u32, sample_rate: u32) -> Self {
        Self {
            channels,
            sample_rate,
        }
    }
}

fn is_sha256_hex(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
