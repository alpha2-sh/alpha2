//! The settings of an export request that parse into more than their JSON shape.

use std::path::PathBuf;

use alpha2_media::{BitrateMode, QualityLevel, VideoRateControl};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A frame rate as a fraction.
#[derive(Copy, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateRequest {
    /// Numerator.
    pub num: u32,
    /// Denominator.
    pub den: u32,
}

/// The largest quantizer a request may name, the top of AV1's scale on NVENC and Vulkan. A value
/// past the chosen codec's scale is the encoder's to refuse, and the bitrate fallback runs.
pub const MAX_QUANTIZER: u32 = 255;

/// One of the engine's five levels or an explicit rate. A level is resolved into a
/// [`VideoRateControl`] before any sink opens.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VideoQualityRequest {
    /// A level the host resolves for the codec.
    Level(QualityLevel),
    /// `{ "bitrate": <bps>, "mode"?: "constant" | "variable", "quantizer"?: <n> }`. With a
    /// quantizer the bitrate is the fallback for an encoder that cannot hold one.
    Explicit(VideoRateControl),
}

impl Default for VideoQualityRequest {
    fn default() -> Self {
        Self::Level(QualityLevel::default())
    }
}

impl VideoQualityRequest {
    /// # Errors
    /// Returns a message naming the field for an unknown level, a bitrate that is not a positive
    /// integer, an unknown mode, or any other key.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        const FIELD: &str = "videoQuality";
        match value {
            Value::String(level) => level
                .parse()
                .map(Self::Level)
                .map_err(|error| format!("{FIELD}: {error}")),
            Value::Object(fields) => {
                if let Some(key) = fields
                    .keys()
                    .find(|key| !matches!(key.as_str(), "bitrate" | "mode" | "quantizer"))
                {
                    return Err(format!(
                        "{FIELD} has an unknown key \"{key}\"; an explicit rate takes bitrate, mode and quantizer"
                    ));
                }
                let bitrate = fields
                    .get("bitrate")
                    .ok_or_else(|| format!("{FIELD}.bitrate is required in an explicit rate"))?;
                let bitrate = bitrate.as_u64().filter(|bitrate| *bitrate > 0).ok_or_else(|| {
                    format!("{FIELD}.bitrate must be a positive integer number of bits per second, received {bitrate}")
                })?;
                let mode = match fields.get("mode") {
                    None => BitrateMode::default(),
                    Some(Value::String(mode)) => mode
                        .parse()
                        .map_err(|error| format!("{FIELD}.mode: {error}"))?,
                    Some(other) => {
                        return Err(format!(
                            "{FIELD}.mode must be \"constant\" or \"variable\", received {other}"
                        ));
                    }
                };
                let quantizer = match fields.get("quantizer") {
                    None => None,
                    Some(quantizer) => Some(
                        quantizer
                            .as_u64()
                            .and_then(|quantizer| u32::try_from(quantizer).ok())
                            .filter(|quantizer| *quantizer <= MAX_QUANTIZER)
                            .ok_or_else(|| {
                                format!("{FIELD}.quantizer must be an integer from 0 to {MAX_QUANTIZER}, received {quantizer}")
                            })?,
                    ),
                };
                VideoRateControl::bitrate(bitrate, mode)
                    .map(|rate| Self::Explicit(VideoRateControl { quantizer, ..rate }))
                    .map_err(|error| format!("{FIELD}.bitrate: {error}"))
            }
            other => Err(format!(
                "{FIELD} must be a quality level name or an explicit rate object, received {other}"
            )),
        }
    }

    /// A level name, or a bare number of bits per second.
    ///
    /// # Errors
    /// Returns a message naming the flag for anything else.
    pub fn from_cli(value: &str) -> Result<Self, String> {
        if value.bytes().all(|byte| byte.is_ascii_digit()) && !value.is_empty() {
            let bitrate = value
                .parse::<u64>()
                .map_err(|_| format!("--video-quality bitrate {value} is out of range"))?;
            return VideoRateControl::bitrate(bitrate, BitrateMode::default())
                .map(Self::Explicit)
                .map_err(|error| format!("--video-quality: {error}"));
        }
        value
            .parse()
            .map(Self::Level)
            .map_err(|error| format!("--video-quality: {error}"))
    }
}

impl<'de> Deserialize<'de> for VideoQualityRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

/// A loudness target (EBU R128, ITU-R BS.1770-4), read from
/// `{ "targetLufs": <LUFS>, "truePeakCeilingDbtp"?: <dBTP> }`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LoudnessRequest {
    /// Integrated loudness, in `-60..=0` LUFS.
    pub target_lufs: f64,
    /// The true-peak ceiling in `-20..=0` dBTP, by default
    /// [`alpha2_compose::DEFAULT_TRUE_PEAK_CEILING_DBTP`]. When the target would pass it, the gain
    /// is lowered and the export left quieter than the target.
    pub true_peak_ceiling_dbtp: f64,
}

impl LoudnessRequest {
    /// # Errors
    /// Returns a message naming the flag or field for a value out of range.
    pub fn new(target_lufs: f64, true_peak_ceiling_dbtp: f64) -> Result<Self, String> {
        if !(target_lufs.is_finite() && (-60.0..=0.0).contains(&target_lufs)) {
            return Err(format!(
                "loudness.targetLufs must be within -60..=0, received {target_lufs}"
            ));
        }
        if !(true_peak_ceiling_dbtp.is_finite() && (-20.0..=0.0).contains(&true_peak_ceiling_dbtp))
        {
            return Err(format!(
                "loudness.truePeakCeilingDbtp must be within -20..=0, received {true_peak_ceiling_dbtp}"
            ));
        }
        Ok(Self {
            target_lufs,
            true_peak_ceiling_dbtp,
        })
    }

    /// # Errors
    /// Returns a message naming the field for a missing, mistyped, unknown, or out-of-range entry.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let Value::Object(fields) = value else {
            return Err(format!(
                "loudness must be an object {{ targetLufs, truePeakCeilingDbtp? }}, received {value}"
            ));
        };
        if let Some(key) = fields
            .keys()
            .find(|key| !matches!(key.as_str(), "targetLufs" | "truePeakCeilingDbtp"))
        {
            return Err(format!(
                "loudness has an unknown key \"{key}\"; it takes targetLufs and truePeakCeilingDbtp"
            ));
        }
        let number = |key: &str, default: Option<f64>| match fields.get(key) {
            Some(value) => value
                .as_f64()
                .ok_or_else(|| format!("loudness.{key} must be a number")),
            None => default.ok_or_else(|| format!("loudness.{key} is required")),
        };
        Self::new(
            number("targetLufs", None)?,
            number(
                "truePeakCeilingDbtp",
                Some(alpha2_compose::DEFAULT_TRUE_PEAK_CEILING_DBTP),
            )?,
        )
    }
}

impl<'de> Deserialize<'de> for LoudnessRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

/// One of the engine's five levels or an explicit bitrate.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AudioQualityRequest {
    /// A level the host resolves for the codec.
    Level(QualityLevel),
    /// Bits per second.
    Explicit(u32),
}

impl Default for AudioQualityRequest {
    fn default() -> Self {
        Self::Level(QualityLevel::default())
    }
}

impl AudioQualityRequest {
    /// # Errors
    /// Returns a message naming the field for an unknown level, a bitrate that is not a positive
    /// integer, or any key other than `bitrate`.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        const FIELD: &str = "audioQuality";
        match value {
            Value::String(level) => level
                .parse()
                .map(Self::Level)
                .map_err(|error| format!("{FIELD}: {error}")),
            Value::Object(fields) => {
                if let Some(key) = fields.keys().find(|key| key.as_str() != "bitrate") {
                    return Err(format!(
                        "{FIELD} has an unknown key \"{key}\"; an explicit rate takes bitrate"
                    ));
                }
                let bitrate = fields
                    .get("bitrate")
                    .ok_or_else(|| format!("{FIELD}.bitrate is required in an explicit rate"))?;
                bitrate
                    .as_u64()
                    .filter(|bitrate| *bitrate > 0)
                    .and_then(|bitrate| u32::try_from(bitrate).ok())
                    .map(Self::Explicit)
                    .ok_or_else(|| {
                        format!("{FIELD}.bitrate must be a positive integer number of bits per second, received {bitrate}")
                    })
            }
            other => Err(format!(
                "{FIELD} must be a quality level name or an explicit rate object, received {other}"
            )),
        }
    }

    /// A level name, or a bare number of bits per second.
    ///
    /// # Errors
    /// Returns a message naming the flag for anything else.
    pub fn from_cli(value: &str) -> Result<Self, String> {
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            return value
                .parse::<u32>()
                .ok()
                .filter(|bitrate| *bitrate > 0)
                .map(Self::Explicit)
                .ok_or_else(|| {
                    format!("--audio-quality bitrate {value} must be a positive integer")
                });
        }
        value
            .parse()
            .map(Self::Level)
            .map_err(|error| format!("--audio-quality: {error}"))
    }
}

impl<'de> Deserialize<'de> for AudioQualityRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

/// Whether frames may, must, or must not reach the encoder without a CPU readback. Every export
/// reports the path as `videoPipeline` and why the GPU path was not taken as
/// `gpuOutputPathUnavailable`.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoInputPolicy {
    /// The GPU-resident path when device, codec, size and pinned encoder allow it; otherwise
    /// readback, with the reason.
    #[default]
    Auto,
    /// The GPU-resident path or an error naming what it lacks; nothing falls back silently.
    GpuRequired,
    /// Readback always.
    CpuReadback,
}

impl VideoInputPolicy {
    /// The protocol's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::GpuRequired => "gpu-required",
            Self::CpuReadback => "cpu-readback",
        }
    }

    /// # Errors
    /// Names the accepted values.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "gpu-required" => Ok(Self::GpuRequired),
            "cpu-readback" => Ok(Self::CpuReadback),
            other => Err(format!(
                "input policy must be auto, gpu-required, or cpu-readback, not {other}"
            )),
        }
    }
}

/// Copying keeps the source's own encoding (bitrate, profile, GOP), so it answers a different
/// request than a render and is only taken when asked for.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExportStrategy {
    /// Render every frame.
    #[default]
    Render,
    /// Copy where the range is one source untouched and its packets decode into exactly the
    /// rendered frames; render otherwise, with the reason.
    PreferCopy,
    /// Copy or refuse with `COPY_INELIGIBLE`.
    RequireCopy,
}

impl ExportStrategy {
    /// The protocol's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::PreferCopy => "prefer-copy",
            Self::RequireCopy => "require-copy",
        }
    }

    /// # Errors
    /// Names the accepted values.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "render" => Ok(Self::Render),
            "prefer-copy" => Ok(Self::PreferCopy),
            "require-copy" => Ok(Self::RequireCopy),
            other => Err(format!(
                "strategy must be render, prefer-copy, or require-copy, not {other}"
            )),
        }
    }
}

/// An export's `memory`: byte budgets that override the environment and the defaults.
#[allow(clippy::struct_field_names)] // Named as the protocol names them.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRequest {
    /// Readback staging and packed encoder frames, or on the GPU-resident path the plane ring and
    /// encoder surfaces.
    pub in_flight_bytes: Option<u64>,
    /// Packets and frames decoders hold ahead of the renderer.
    pub decode_queue_bytes: Option<u64>,
    /// Frames a reversed clip keeps decoded.
    pub reverse_cache_bytes: Option<u64>,
    /// Decoded audio an export holds.
    pub audio_window_bytes: Option<u64>,
    /// Kept frames and images of SVG, animated image, Lottie, `OpenEXR` and image sequence sources.
    pub provider_source_bytes: Option<u64>,
    /// Decoder sessions, counted by the host's per-session estimate, which bounds how many an
    /// export opens.
    pub decoder_session_bytes: Option<u64>,
    /// Blocks fetched of remote sources, at least [`crate::remote::MIN_CACHE_BYTES`].
    pub remote_cache_bytes: Option<u64>,
    /// A factor in (0, 1] applied to every budget after resolution, so a retry can ask for less
    /// without knowing the host's defaults.
    pub scale: Option<f64>,
}

/// Whether a remote original whose project records a content hash is read whole and hashed.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteContentHash {
    /// Compared without its hash, as `consistent` at best.
    #[default]
    Skip,
    /// Read whole and hashed, as a file is.
    Verify,
}

/// The quota of a render cache whose request names none.
pub const DEFAULT_RENDER_CACHE_QUOTA_BYTES: u64 = 4 << 30;

/// What a render cache's writer does when it falls behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backlog {
    /// Skips the frame, counted as `busySkips`.
    #[default]
    Skip,
    /// Waits for the writer.
    Wait,
}

impl Backlog {
    /// The value the protocol's name stands for; `None` for an unknown name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "skip" => Some(Self::Skip),
            "wait" => Some(Self::Wait),
            _ => None,
        }
    }

    /// The protocol's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Wait => "wait",
        }
    }
}

/// An export's `renderCache`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderCacheRequest {
    /// The cache directory.
    pub directory: PathBuf,
    /// The most the cache keeps on disk.
    pub quota_bytes: u64,
    /// What the writer does when it falls behind.
    pub backlog: Backlog,
}

impl RenderCacheRequest {
    /// The only constructor, so the protocol and the CLI refuse the same values.
    ///
    /// # Errors
    /// An empty directory, which would bake into the working directory, or a zero quota.
    pub fn new(directory: PathBuf, quota_bytes: u64, backlog: Backlog) -> Result<Self, String> {
        if directory.as_os_str().is_empty() {
            return Err("renderCache.directory must be a non-empty path".to_owned());
        }
        if quota_bytes == 0 {
            return Err("renderCache.quotaBytes must be a positive integer".to_owned());
        }
        Ok(Self {
            directory,
            quota_bytes,
            backlog,
        })
    }

    /// # Errors
    /// Describes the first malformed field.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or_else(|| {
            format!(
                "renderCache must be an object {{ directory, quotaBytes?, backlog? }}, received {value}"
            )
        })?;
        for key in object.keys() {
            if !["directory", "quotaBytes", "backlog"].contains(&key.as_str()) {
                return Err(format!(
                    "renderCache has an unknown key \"{key}\"; it takes directory, quotaBytes, and backlog"
                ));
            }
        }
        let directory = object
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| "renderCache.directory must be a non-empty path".to_owned())?;
        let quota_bytes = match object.get("quotaBytes") {
            None | Some(Value::Null) => DEFAULT_RENDER_CACHE_QUOTA_BYTES,
            Some(quota) => quota
                .as_u64()
                .ok_or_else(|| "renderCache.quotaBytes must be a positive integer".to_owned())?,
        };
        let backlog = match object.get("backlog") {
            None | Some(Value::Null) => Backlog::default(),
            Some(backlog) => backlog
                .as_str()
                .and_then(Backlog::parse)
                .ok_or_else(|| "renderCache.backlog must be \"skip\" or \"wait\"".to_owned())?,
        };
        Self::new(PathBuf::from(directory), quota_bytes, backlog)
    }
}
