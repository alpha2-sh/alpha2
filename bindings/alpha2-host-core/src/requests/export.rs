//! The `export` request and the format it resolves to.

use std::collections::HashMap;
use std::path::PathBuf;

use alpha2_media::{AudioCodec, Container, ContainerError, VideoCodec};
use serde::Deserialize;
use serde_json::Value;

use super::RequestError;
use super::effect::EffectRequest;
use super::media::{ExportMedia, ProjectJson, ProxyRequest};
use super::settings::{
    AudioQualityRequest, ExportStrategy, LoudnessRequest, MemoryRequest, RateRequest,
    RenderCacheRequest, VideoInputPolicy, VideoQualityRequest,
};
use crate::codecs::{NativeAudioCodec, NativeVideoCodec};

/// An `export` request.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// Always the protocol version.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    /// The caller's id for the request.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Read by the dispatcher.
    #[serde(default)]
    pub operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default)]
    pub attachments: serde::de::IgnoredAny,
    /// The project to export.
    pub project: ProjectJson,
    /// Originals by source ID.
    pub assets: HashMap<String, super::MediaLocation>,
    /// Proxies by source ID.
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    /// Image sequence frames by source ID, in project order; `null` where the sequence has a gap.
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// Files an SVG's `<image href>` or a Lottie file's image assets name, by source ID and then by
    /// locator.
    #[serde(default)]
    pub resources: HashMap<String, HashMap<String, super::MediaLocation>>,
    /// Originals unless the caller asks for proxies.
    #[serde(default)]
    pub media: ExportMedia,
    /// Lookup tables of runtime effects, by name.
    #[serde(rename = "effectAssets", default)]
    pub effect_assets: HashMap<String, PathBuf>,
    /// Font files, in fallback order.
    #[serde(rename = "fonts", default)]
    pub font_files: Vec<PathBuf>,
    /// Runtime effects the project's clips name.
    #[serde(default)]
    pub effects: Vec<EffectRequest>,
    /// The first tick of the range.
    #[serde(rename = "startTicks")]
    pub start_ticks: i64,
    /// The tick after the range.
    #[serde(rename = "endTicks")]
    pub end_ticks: i64,
    /// The output's frame rate.
    #[serde(rename = "frameRate")]
    pub frame_rate: RateRequest,
    /// Whether the export has sound; true by default, as in the CLI, `@alpha2-sh/node` and the
    /// browser exporter.
    #[serde(rename = "includeAudio", default = "include_audio_by_default")]
    pub include_audio: bool,
    /// The container; the one `output`'s extension names when absent.
    #[serde(default)]
    pub container: Option<Container>,
    /// The video codec; the container's default when absent.
    #[serde(rename = "videoCodec", default)]
    pub video_codec: Option<VideoCodec>,
    /// Draw without the project background and write an alpha plane (VP9 in `WebM` or Matroska,
    /// `ProRes` 4444).
    #[serde(default)]
    pub alpha: bool,
    /// 8 or 10; the codec's own when absent (10 for `ProRes`, else 8).
    #[serde(rename = "bitDepth", default)]
    pub bit_depth: Option<u8>,
    /// An encoder by `FFmpeg` name instead of the host's choice.
    #[serde(rename = "videoEncoder", default)]
    pub video_encoder: Option<String>,
    /// Whether frames may reach the encoder without a readback.
    #[serde(rename = "inputPolicy", default)]
    pub input_policy: VideoInputPolicy,
    /// Whether the export may copy packets instead of rendering.
    #[serde(default)]
    pub strategy: ExportStrategy,
    /// The video rate.
    #[serde(rename = "videoQuality", default)]
    pub video_quality: VideoQualityRequest,
    /// The audio codec; the container's default when absent.
    #[serde(rename = "audioCodec", default)]
    pub audio_codec: Option<AudioCodec>,
    /// The audio rate.
    #[serde(rename = "audioQuality", default)]
    pub audio_quality: AudioQualityRequest,
    /// A loudness target; without one the mix keeps its own loudness.
    #[serde(default)]
    pub loudness: Option<LoudnessRequest>,
    /// Frames baked in an earlier export, reused where they match.
    #[serde(rename = "renderCache", default, deserialize_with = "render_cache")]
    pub render_cache: Option<RenderCacheRequest>,
    /// Byte budgets; absent ones fall back to the environment, then the host's defaults.
    #[serde(default)]
    pub memory: MemoryRequest,
    /// The file written.
    pub output: PathBuf,
    /// Replace a regular file at `output` once the export is whole; otherwise an existing output
    /// is refused.
    #[serde(default)]
    pub overwrite: bool,
}

impl ExportRequest {
    /// Cross-field rules no single setting's parse can see, checked by the protocol and the CLI
    /// before a device or encoder opens.
    ///
    /// # Errors
    /// `INVALID_REQUEST` for a URL in a path field; `INVALID_INPUT` naming the rule: loudness
    /// needs `includeAudio`, and the container and codecs must resolve ([`Self::format`]).
    pub fn check(&self) -> Result<(), RequestError> {
        super::refuse_urls(
            &self.proxies,
            &self.frames,
            &self.effect_assets,
            &self.font_files,
        )?;
        super::refuse_url("output", &self.output)?;
        if let Some(cache) = &self.render_cache {
            super::refuse_url("renderCache.directory", &cache.directory)?;
        }
        if self.loudness.is_some() && !self.include_audio {
            return Err(RequestError::invalid_input(
                "loudness normalizes the export's audio and needs includeAudio: true",
            ));
        }
        self.format().map(|_| ())
    }

    /// `container` must agree with `output`'s extension, and each named codec must be offered by
    /// the container; unnamed ones take its default.
    ///
    /// # Errors
    /// `INVALID_INPUT` naming what does not resolve.
    pub fn format(&self) -> Result<ResolvedFormat, RequestError> {
        let bit_depth = self
            .bit_depth
            .map(|bits| {
                alpha2_media::BitDepth::from_bits(bits).ok_or_else(|| {
                    RequestError::invalid_input(format!("bitDepth: {bits} is not 8 or 10"))
                })
            })
            .transpose()?;
        ResolvedFormat::resolve(
            self.container,
            &self.output,
            self.video_codec,
            self.audio_codec,
        )?
        .with_pixels(alpha2_media::OutputPixels {
            bit_depth,
            alpha: self.alpha,
        })
    }
}

/// The container, codecs and pixel layout an export writes, resolved once at the host's edge from
/// the engine's policy ([`alpha2_media::resolve_export_format`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResolvedFormat {
    /// The output container.
    pub container: Container,
    /// The video encoder family.
    pub video: NativeVideoCodec,
    /// The audio encoder family.
    pub audio: NativeAudioCodec,
    /// The output pixel layout ([`alpha2_media::resolve_pixels`]).
    pub pixels: alpha2_media::PixelLayout,
}

impl ResolvedFormat {
    /// # Errors
    /// `INVALID_INPUT` with the policy's reason.
    pub fn resolve(
        container: Option<Container>,
        output: &std::path::Path,
        video: Option<VideoCodec>,
        audio: Option<AudioCodec>,
    ) -> Result<Self, RequestError> {
        let extension = output
            .extension()
            .map_or(Some(""), std::ffi::OsStr::to_str)
            .ok_or_else(|| RequestError::invalid_input("output path is not UTF-8"))?;
        let (container, video, audio) =
            alpha2_media::resolve_export_format(container, Some(extension), video, audio)
                .map_err(|error| RequestError::invalid_input(container_message(&error)))?;
        Ok(Self {
            container,
            video: NativeVideoCodec::from_model(video),
            audio: NativeAudioCodec::from_model(audio),
            pixels: alpha2_media::resolve_pixels(
                video,
                container,
                alpha2_media::OutputPixels::default(),
            )
            .map_err(|error| RequestError::invalid_input(format!("videoCodec: {error}")))?,
        })
    }

    /// # Errors
    /// `INVALID_INPUT` naming `alpha` or `bitDepth` when the codec cannot be written so in the
    /// container.
    pub fn with_pixels(self, pixels: alpha2_media::OutputPixels) -> Result<Self, RequestError> {
        let layout = alpha2_media::resolve_pixels(self.video.model(), self.container, pixels)
            .map_err(|error| {
                let field = match error {
                    alpha2_media::OutputPixelsError::BitDepthUnsupported { .. } => "bitDepth",
                    _ => "alpha",
                };
                RequestError::invalid_input(format!("{field}: {error}"))
            })?;
        Ok(Self {
            pixels: layout,
            ..self
        })
    }
}

fn container_message(error: &ContainerError) -> String {
    match error {
        ContainerError::Unknown(_) => format!("container: {error}"),
        ContainerError::UnknownExtension(_) | ContainerError::ExtensionMismatch { .. } => {
            format!("output: {error}")
        }
        ContainerError::VideoCodecNotOffered { .. } => format!("videoCodec: {error}"),
        ContainerError::AudioCodecNotOffered { .. } => format!("audioCodec: {error}"),
    }
}

/// Checked before the typed parse so a bad value is reported as `INVALID_INPUT` naming the field,
/// not as a malformed request.
pub(super) fn check_settings(
    setting: &dyn Fn(&str) -> Result<Option<Value>, RequestError>,
) -> Result<(), RequestError> {
    if let Some(quality) = setting("videoQuality")? {
        VideoQualityRequest::from_json(&quality).map_err(RequestError::invalid_input)?;
    }
    if let Some(quality) = setting("audioQuality")? {
        AudioQualityRequest::from_json(&quality).map_err(RequestError::invalid_input)?;
    }
    if let Some(cache) = setting("renderCache")?.filter(|value| !value.is_null()) {
        RenderCacheRequest::from_json(&cache).map_err(RequestError::invalid_input)?;
    }
    if let Some(loudness) = setting("loudness")?.filter(|value| !value.is_null()) {
        LoudnessRequest::from_json(&loudness).map_err(RequestError::invalid_input)?;
    }
    // A budget the caller sets is its input too: a misspelled or mistyped field names itself.
    if let Some(alpha) = setting("alpha")?
        && !alpha.is_boolean()
    {
        return Err(RequestError::invalid_input(format!(
            "alpha: {alpha} is not true or false"
        )));
    }
    if let Some(bits) = setting("bitDepth")?.filter(|value| !value.is_null())
        && !matches!(bits.as_u64(), Some(8 | 10))
    {
        return Err(RequestError::invalid_input(format!(
            "bitDepth: {bits} is not 8 or 10"
        )));
    }
    if let Some(memory) = setting("memory")? {
        serde_json::from_value::<MemoryRequest>(memory)
            .map_err(|error| RequestError::invalid_input(format!("memory: {error}")))?;
    }
    check_format_names(setting)
}

/// An unknown `container`, `videoCodec` or `audioCodec` is `INVALID_INPUT` naming the field, not a
/// malformed request.
fn check_format_names(
    setting: &dyn Fn(&str) -> Result<Option<Value>, RequestError>,
) -> Result<(), RequestError> {
    if let Some(container) = setting("container")?.filter(|value| !value.is_null()) {
        serde_json::from_value::<Container>(container.clone()).map_err(|_| {
            RequestError::invalid_input(format!(
                "container: {}",
                ContainerError::Unknown(container.to_string().trim_matches('"').to_owned())
            ))
        })?;
    }
    for (field, names) in [
        ("videoCodec", "h264, hevc, vp9, av1, mpeg4-part2, prores"),
        ("audioCodec", "aac, opus"),
    ] {
        if let Some(codec) = setting(field)?.filter(|value| !value.is_null()) {
            let known = codec.as_str().is_some_and(|name| {
                if field == "videoCodec" {
                    name.parse::<VideoCodec>().is_ok()
                } else {
                    name.parse::<AudioCodec>().is_ok()
                }
            });
            if !known {
                return Err(RequestError::invalid_input(format!(
                    "{field}: unknown codec {codec}; expected one of {names}"
                )));
            }
        }
    }
    Ok(())
}

const fn include_audio_by_default() -> bool {
    true
}

fn render_cache<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<RenderCacheRequest>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .filter(|value| !value.is_null())
        .map(|value| RenderCacheRequest::from_json(&value).map_err(serde::de::Error::custom))
        .transpose()
}
