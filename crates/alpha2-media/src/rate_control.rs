//! Output rate control: the vocabulary a caller asks in, and the numbers a
//! sink receives.
//!
//! A caller names a [`QualityLevel`] or an explicit rate. The engine resolves
//! it once into a [`VideoRateControl`], and every sink (native software,
//! native GPU-resident, browser) encodes from that resolved value. No sink
//! ever sees a level, so no two sinks can disagree about what one means.

use std::fmt;
use std::str::FromStr;

use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

/// Video codecs the engine resolves a rate for.
///
/// One set of spellings for every host: the native sinks key on these
/// directly, and the browser adapter maps its `avc` onto [`Self::H264`] at its
/// own edge. `mpeg4-part2` has no browser sink.
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
}

impl VideoCodec {
    /// Every codec, in the order the engine lists them.
    pub const ALL: [Self; 5] = [
        Self::H264,
        Self::Hevc,
        Self::Vp9,
        Self::Av1,
        Self::Mpeg4Part2,
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
        }
    }

    /// Where the codec's 4:2:0 chroma samples sit when a stream does not say,
    /// which is how every export is written: an export packs its chroma
    /// there, since a browser encoder cannot tag it otherwise.
    ///
    /// H.264, HEVC, and MPEG-4 Part 2 imply H.273 chroma location type 0,
    /// co-sited with the even luma columns ([`ChromaLocation::Left`]), as
    /// decoders report them and alpha2 imports them. VP9 and AV1 streams
    /// leave it unspecified, which alpha2 reads as centred.
    ///
    /// [`ChromaLocation::Left`]: crate::ChromaLocation::Left
    #[must_use]
    pub const fn chroma_location(self) -> crate::ChromaLocation {
        match self {
            Self::H264 | Self::Hevc | Self::Mpeg4Part2 => crate::ChromaLocation::Left,
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
/// The names are the browser adapter's, so the engine and the adapter share
/// one vocabulary for the concept. Their meaning is the engine's: see the
/// resolution functions beside this type.
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
/// `bitrate` is always present. With a `quantizer` it is the fallback for an
/// encoder that cannot hold a constant quantizer; without one it is the
/// target.
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
// rounded to 1 kbps and clamped below at MIN_VIDEO_BITRATE. The constants were measured on this
// project's own export path (PSNR-Y on the 4K showcase), not taken from encoder defaults.

/// Luma samples of the reference output the level table is stated at.
pub const REFERENCE_PIXELS: u64 = 1920 * 1080;
/// Frame rate the level table is stated at.
pub const REFERENCE_FPS: f64 = 30.0;

/// H.264 bits per second at 1920x1080, 30 fps, lowest level first.
///
/// Each level is 3 dB of PSNR-Y above the one below it (half the error) on
/// the harder of the two measured contents: 32, 35, 38, 41 and 44 dB. A
/// constant rate must hold its level on content harder than the median;
/// easier content lands higher on the same rate.
const REFERENCE_BITRATE: [f64; 5] = [1.8e6, 3.5e6, 6.0e6, 10.0e6, 16.0e6];

/// How bitrate grows with pixel count at equal quality.
///
/// Measured between 0.16 and 0.54 across the two contents and the level
/// range; this is the steepest, because both measured contents are soft and
/// sharper sources scale closer to linearly.
pub const RESOLUTION_EXPONENT: f64 = 0.5;

/// How bitrate grows with frame rate at equal quality: `(fps / 30) ^ k`.
///
/// Not linear: frames at 60 fps are more alike than frames at 24. Measured
/// 24-vs-60 at equal PSNR, `k` is 0.30 to 0.36 for 24p footage exported at
/// 60 and 0.87 to 0.98 for true 60 fps motion; this is the middle of that bracket,
/// which puts 60 fps at 1.52x of 30.
pub const FRAME_RATE_EXPONENT: f64 = 0.6;

/// No level resolves below this. Below about 300 kbps the measured encoders
/// stopped holding their target at all (asked for 84 kbps they wrote 170 to
/// 260), so a lower number would describe a file the encoder does not make.
pub const MIN_VIDEO_BITRATE: u64 = 250_000;

/// Bits a codec needs, relative to H.264, for the same quality.
///
/// HEVC and AV1 were measured on NVENC against `h264_nvenc` at equal PSNR-Y
/// (0.61 to 0.71 and 0.56 to 0.61 over 40 to 48 dB), MPEG-4 Part 2 on `mpeg4` against
/// `h264_vulkan` (3.3x at 40 dB falling to 2.1x at 48). The software encoders the
/// pinned SDK links were measured the same way, against `h264_nvenc` on the colour
/// clip at 1920x816, at the PSNR-Y `h264_nvenc` reaches at each level's H.264 rate:
/// `libvpx-vp9` (`cpu-used` 4) needs 0.60x at `very-low` rising to 0.87x at
/// `very-high`, so VP9 is the middle of that, 0.75. `libsvtav1` (preset 8) needs
/// 0.47x rising to 0.93x, where `av1_nvenc` on the same frames needs 0.49x to 0.61x;
/// one AV1 factor serves both encoders, so it is the higher of their middles, 0.70, which
/// gives `av1_nvenc` more than it needs rather than `libsvtav1` far less. The same runs put HEVC
/// at 0.60x to 0.73x, in line with its factor. A browser's `WebCodecs` VP9 and AV1
/// encoders take the same factors.
#[must_use]
pub const fn codec_factor(codec: VideoCodec) -> f64 {
    match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::Hevc => 0.65,
        VideoCodec::Vp9 => 0.75,
        VideoCodec::Av1 => 0.70,
        VideoCodec::Mpeg4Part2 => 2.7,
    }
}

/// The constant quantizer a level resolves to for `codec`, where the engine
/// has measured one.
///
/// H.264 only, on its 0-51 QP scale, which `h264_vulkan`, `h264_nvenc` and
/// `WebCodecs`' `avc` quantizer mode share. Each value is the QP at which
/// `h264_vulkan` holds the level's own PSNR-Y definition on the harder
/// measured content at the size the levels are stated at (the colour clip at
/// 1920x816: 32.5, 35.6, 38.4, 41.3 and 44.4 dB under the bitrate path), rounded
/// toward the better picture. QP moves PSNR by 0.66 dB a step there and by
/// about the same at 4K, so one QP per level serves every size. HEVC and AV1
/// encoders open a quantizer mode too, but what their QPs are worth has not
/// been measured, so their levels stay bitrate-only rather than borrowing
/// H.264's; MPEG-4 Part 2's encoder takes its quantizer per frame and not from
/// the options a sink sets, so it has none either.
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
        VideoCodec::Hevc | VideoCodec::Vp9 | VideoCodec::Av1 | VideoCodec::Mpeg4Part2 => None,
    }
}

/// Resolves a quality level into the rate control every sink encodes with.
///
/// This is the one definition of what a level means. The native host and the
/// browser binding both call it, so two sinks asked for the same level at the
/// same output receive the same numbers.
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
    // The quantizer, where there is one, is what a sink runs; the bitrate is its fallback on an
    // encoder that cannot hold a constant quantizer, and the whole of the rate control on a codec
    // without one.
    Ok(VideoRateControl {
        bitrate: kilobits.saturating_mul(1_000).max(MIN_VIDEO_BITRATE),
        mode: BitrateMode::Variable,
        quantizer: level_quantizer(codec, level),
    })
}

/// Audio codecs the engine resolves a rate for.
///
/// AAC and Opus. The native sinks encode Opus with `libopus`, the browser
/// with `WebCodecs`; which containers offer which is
/// [`Container::audio_codecs`](crate::Container::audio_codecs).
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
/// No resolution or duration term: an audio stream's cost depends on its
/// codec and channel layout, and every alpha2 export is stereo.
///
/// AAC-LC: `high`, the default, is 192 kbps. The ladder runs 64 / 96 / 128 /
/// 192 / 256 kbps: 64 kbps is where AAC-LC stereo is still intelligible and
/// 256 is past the point where AAC-LC stereo is commonly held to be
/// transparent, and the steps between are the conventional encoder settings.
/// Opus reaches the same quality at roughly two thirds of the AAC rate and is
/// transparent near 128 kbps stereo, so it runs 48 / 64 / 96 / 128 / 160.
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
