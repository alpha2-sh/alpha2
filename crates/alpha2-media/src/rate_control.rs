//! Output rate control.
//!
//! A caller names a [`QualityLevel`] or an explicit rate, which the engine resolves once into a
//! [`VideoRateControl`]. Sinks only see resolved values, so they cannot disagree on a level.

use std::fmt;
use std::str::FromStr;

use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

/// Video codecs the engine resolves a rate for.
///
/// `mpeg4-part2` has no browser sink.
#[derive(Copy, Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoCodec {
    /// H.264/AVC.
    H264,
    /// H.265/HEVC.
    Hevc,
    /// VP9.
    Vp9,
    /// AV1.
    Av1,
    /// MPEG-4 Part 2.
    Mpeg4Part2,
    /// Apple `ProRes`: 422 HQ, or 4444 with alpha. Intra-only, 10-bit.
    #[serde(rename = "prores")]
    ProRes,
}

impl VideoCodec {
    /// Every codec, in the order the engine lists them.
    pub const ALL: [Self; 6] = [
        Self::H264,
        Self::Hevc,
        Self::Vp9,
        Self::Av1,
        Self::Mpeg4Part2,
        Self::ProRes,
    ];

    /// Stable identifier used by every protocol and binding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
            Self::Mpeg4Part2 => "mpeg4-part2",
            Self::ProRes => "prores",
        }
    }

    /// The chroma siting a stream of this codec implies when untagged. Exports pack chroma
    /// there because browser encoders cannot tag it.
    ///
    /// H.264, HEVC and MPEG-4 Part 2 imply H.273 location type 0 ([`ChromaLocation::Left`]). VP9
    /// and AV1 leave it unspecified, read as centred. `ProRes` 4:2:2 co-sites with even luma
    /// columns, as BT.709 4:2:2 does.
    ///
    /// [`ChromaLocation::Left`]: crate::ChromaLocation::Left
    #[must_use]
    pub const fn chroma_location(self) -> crate::ChromaLocation {
        match self {
            Self::H264 | Self::Hevc | Self::Mpeg4Part2 | Self::ProRes => {
                crate::ChromaLocation::Left
            }
            Self::Vp9 | Self::Av1 => crate::ChromaLocation::Center,
        }
    }
}

impl FromStr for VideoCodec {
    type Err = RateControlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|codec| codec.as_str() == value)
            .ok_or_else(|| RateControlError::UnknownVideoCodec(value.to_owned()))
    }
}

/// The five qualitative output levels.
///
/// See [`resolve_video_rate_control`] for what each level means.
#[derive(
    Copy, Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum QualityLevel {
    /// Smallest output that is still watchable.
    VeryLow,
    /// Previews and proxies.
    Low,
    /// Streaming-sized delivery.
    Medium,
    /// Delivery quality. The default.
    #[default]
    High,
    /// Close to visually transparent; mastering and re-encode sources.
    VeryHigh,
}

impl QualityLevel {
    /// Every level, lowest first.
    pub const ALL: [Self; 5] = [
        Self::VeryLow,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::VeryHigh,
    ];

    /// Stable identifier used by every protocol and binding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VeryLow => "very-low",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::VeryHigh => "very-high",
        }
    }
}

impl fmt::Display for QualityLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for QualityLevel {
    type Err = RateControlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == value)
            .ok_or_else(|| RateControlError::UnknownLevel(value.to_owned()))
    }
}

/// How an encoder is asked to hold its bitrate.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BitrateMode {
    /// Average rate; the encoder may spend more on hard frames. The default.
    #[default]
    Variable,
    /// Constant rate, held against a one-second buffer.
    Constant,
}

impl BitrateMode {
    /// Stable identifier used by every protocol and binding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Variable => "variable",
            Self::Constant => "constant",
        }
    }
}

impl FromStr for BitrateMode {
    type Err = RateControlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "variable" => Ok(Self::Variable),
            "constant" => Ok(Self::Constant),
            other => Err(RateControlError::UnknownMode(other.to_owned())),
        }
    }
}

/// A resolved video rate control: what a sink encodes with.
///
/// `bitrate` is always present: the target, or with a `quantizer` the fallback for encoders
/// without a constant-quantizer mode.
#[derive(Copy, Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct VideoRateControl {
    /// Target, or fallback, bits per second. Always positive.
    pub bitrate: u64,
    /// How the bitrate is held when the encoder runs on it.
    pub mode: BitrateMode,
    /// Constant quantizer on the codec's own scale, where the encoder has one.
    pub quantizer: Option<u32>,
}

impl VideoRateControl {
    /// An explicit bitrate with no quantizer.
    ///
    /// # Errors
    /// Returns [`RateControlError::NonPositiveBitrate`] for zero.
    pub const fn bitrate(bitrate: u64, mode: BitrateMode) -> Result<Self, RateControlError> {
        if bitrate == 0 {
            return Err(RateControlError::NonPositiveBitrate);
        }
        Ok(Self {
            bitrate,
            mode,
            quantizer: None,
        })
    }
}

/// Why a rate-control request is not one the engine can resolve.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RateControlError {
    /// A level name outside the five.
    UnknownLevel(String),
    /// A bitrate mode other than `constant` or `variable`.
    UnknownMode(String),
    /// A bitrate of zero or less.
    NonPositiveBitrate,
    /// A video codec name outside [`VideoCodec::ALL`].
    UnknownVideoCodec(String),
    /// An audio codec name other than `aac` or `opus`.
    UnknownAudioCodec(String),
    /// A zero output dimension.
    EmptyFrame,
}

impl fmt::Display for RateControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLevel(level) => write!(
                formatter,
                "unknown quality level \"{level}\"; expected one of very-low, low, medium, high, very-high"
            ),
            Self::UnknownMode(mode) => write!(
                formatter,
                "unknown bitrate mode \"{mode}\"; expected constant or variable"
            ),
            Self::NonPositiveBitrate => formatter.write_str("bitrate must be a positive integer"),
            Self::UnknownVideoCodec(codec) => write!(
                formatter,
                "unknown video codec \"{codec}\"; expected one of h264, hevc, vp9, av1, mpeg4-part2"
            ),
            Self::UnknownAudioCodec(codec) => write!(
                formatter,
                "unknown audio codec \"{codec}\"; expected aac or opus"
            ),
            Self::EmptyFrame => formatter.write_str("output width and height must be non-zero"),
        }
    }
}

impl std::error::Error for RateControlError {}

// Level to rate:
//
// bitrate = REFERENCE_BITRATE[level]
//         * (pixels / REFERENCE_PIXELS) ^ RESOLUTION_EXPONENT
//         * (fps / REFERENCE_FPS) ^ FRAME_RATE_EXPONENT
//         * codec_factor(codec)
//
// rounded to 1 kbps and clamped below at MIN_VIDEO_BITRATE. The constants are PSNR-Y measurements
// of this project's export path, not encoder defaults.

/// Luma samples of the reference output the level table is stated at.
pub const REFERENCE_PIXELS: u64 = 1920 * 1080;
/// Frame rate the level table is stated at.
pub const REFERENCE_FPS: f64 = 30.0;

/// H.264 bits per second at 1920x1080, 30 fps, lowest level first.
///
/// Levels are 3 dB PSNR-Y apart (32, 35, 38, 41, 44 dB) on the harder measured content, so
/// easier content lands higher at the same rate.
const REFERENCE_BITRATE: [f64; 5] = [1.8e6, 3.5e6, 6.0e6, 10.0e6, 16.0e6];

/// How bitrate grows with pixel count at equal quality.
///
/// Measured between 0.16 and 0.54; the steepest value is used because sharper sources than the
/// measured ones scale closer to linearly.
pub const RESOLUTION_EXPONENT: f64 = 0.5;

/// How bitrate grows with frame rate at equal quality: `(fps / 30) ^ k`.
///
/// Measured `k` is 0.30 to 0.36 for 24p footage exported at 60 and 0.87 to 0.98 for true 60 fps
/// motion; 0.6 is the middle, putting 60 fps at 1.52x of 30.
pub const FRAME_RATE_EXPONENT: f64 = 0.6;

/// No level resolves below this; encoders stop holding their target below about 300 kbps.
pub const MIN_VIDEO_BITRATE: u64 = 250_000;

/// Bits a codec needs, relative to H.264, for the same quality.
///
/// Measured at equal PSNR-Y against H.264: HEVC 0.60 to 0.73, MPEG-4 Part 2 3.3 at 40 dB to 2.1
/// at 48 dB, `libvpx-vp9` (`cpu-used` 4) 0.60 to 0.87 (factor 0.75), `libsvtav1` (preset 8) 0.47
/// to 0.93 and `av1_nvenc` 0.49 to 0.61. One AV1 factor serves both encoders, so it takes the
/// higher middle, 0.70. `ProRes` ignores the rate (its profile fixes it); its factor of 22
/// (422 HQ at 1080p30 is about 220 Mbit/s) only estimates the output size.
#[must_use]
pub const fn codec_factor(codec: VideoCodec) -> f64 {
    match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::Hevc => 0.65,
        VideoCodec::Vp9 => 0.75,
        VideoCodec::Av1 => 0.70,
        VideoCodec::Mpeg4Part2 => 2.7,
        VideoCodec::ProRes => 22.0,
    }
}

/// The constant quantizer a level resolves to for `codec`, where the engine
/// has measured one.
///
/// H.264 only, on the 0-51 QP scale shared by `h264_vulkan`, `h264_nvenc` and `WebCodecs` `avc`.
/// Each value is the QP that holds the level's PSNR-Y target on the harder measured content,
/// rounded toward quality. One QP step is about 0.66 dB at every size, so one QP per level
/// serves all sizes. HEVC and AV1 QPs are unmeasured, and MPEG-4 Part 2 sets its quantizer per
/// frame, so those stay bitrate-only.
#[must_use]
pub const fn level_quantizer(codec: VideoCodec, level: QualityLevel) -> Option<u32> {
    match codec {
        VideoCodec::H264 => Some(match level {
            QualityLevel::VeryLow => 41,
            QualityLevel::Low => 35,
            QualityLevel::Medium => 31,
            QualityLevel::High => 27,
            QualityLevel::VeryHigh => 23,
        }),
        VideoCodec::Hevc
        | VideoCodec::Vp9
        | VideoCodec::Av1
        | VideoCodec::Mpeg4Part2
        | VideoCodec::ProRes => None,
    }
}

/// Resolves a quality level into the rate control every sink encodes with.
///
/// This is the single definition of what a level means for every host.
///
/// # Errors
/// Returns [`RateControlError::EmptyFrame`] for a zero dimension.
pub fn resolve_video_rate_control(
    codec: VideoCodec,
    width: u32,
    height: u32,
    frame_rate: FrameRate,
    level: QualityLevel,
) -> Result<VideoRateControl, RateControlError> {
    if width == 0 || height == 0 {
        return Err(RateControlError::EmptyFrame);
    }
    let pixels = f64::from(width) * f64::from(height);
    #[allow(clippy::cast_precision_loss)] // 2^53 pixels is not an output size.
    let resolution = (pixels / REFERENCE_PIXELS as f64).powf(RESOLUTION_EXPONENT);
    let fps = f64::from(frame_rate.numerator()) / f64::from(frame_rate.denominator());
    let temporal = (fps / REFERENCE_FPS).powf(FRAME_RATE_EXPONENT);
    let bitrate = REFERENCE_BITRATE[level as usize] * resolution * temporal * codec_factor(codec);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Finite and positive.
    let kilobits = (bitrate / 1_000.0).round() as u64;
    // Sinks run the quantizer when present and fall back to the bitrate otherwise.
    Ok(VideoRateControl {
        bitrate: kilobits.saturating_mul(1_000).max(MIN_VIDEO_BITRATE),
        mode: BitrateMode::Variable,
        quantizer: level_quantizer(codec, level),
    })
}

/// Audio codecs the engine resolves a rate for.
///
/// Containers offer them per [`Container::audio_codecs`](crate::Container::audio_codecs).
#[derive(Copy, Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioCodec {
    /// AAC-LC.
    Aac,
    /// Opus.
    Opus,
}

impl AudioCodec {
    /// Stable identifier used by every protocol and binding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }
}

impl FromStr for AudioCodec {
    type Err = RateControlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "aac" => Ok(Self::Aac),
            "opus" => Ok(Self::Opus),
            other => Err(RateControlError::UnknownAudioCodec(other.to_owned())),
        }
    }
}

/// Resolves a quality level into a stereo audio bitrate in bits per second.
///
/// AAC-LC runs 64 / 96 / 128 / 192 / 256 kbps, from still intelligible to past transparent.
/// Opus needs about two thirds of that and is transparent near 128 kbps, so it runs
/// 48 / 64 / 96 / 128 / 160.
#[must_use]
pub const fn resolve_audio_bitrate(codec: AudioCodec, level: QualityLevel) -> u32 {
    let table: [u32; 5] = match codec {
        AudioCodec::Aac => [64_000, 96_000, 128_000, 192_000, 256_000],
        AudioCodec::Opus => [48_000, 64_000, 96_000, 128_000, 160_000],
    };
    table[level as usize]
}

#[cfg(test)]
#[path = "rate_control_tests.rs"]
mod tests;
