use std::collections::HashMap;
use std::path::PathBuf;

use crate::PROTOCOL_VERSION;
use crate::assets::ProxyRequest;
use crate::effect_request::EffectRequest;
use alpha2_assets::MediaPurpose;
use alpha2_media::{
    AudioCodec, BitrateMode, Container, ContainerError, QualityLevel, VideoCodec, VideoRateControl,
};
use alpha2_native::{
    NativeAudioCodec, NativeVideoCodec, codec_capabilities, hardware_decode_capabilities,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};

/// A project at the engine's [`alpha2_model::MAX_PROJECT_JSON_BYTES`] plus as much again for the
/// rest of the request. A longer line is answered `INVALID_REQUEST` without being parsed.
pub const MAX_REQUEST_LINE_BYTES: usize = 2 * alpha2_model::MAX_PROJECT_JSON_BYTES;

/// The small settings `parse_request` reads into a JSON tree for checking (`videoQuality`,
/// `audioQuality`, `renderCache`, `loudness`, `memory`) may be at most this long.
pub const MAX_REQUEST_SETTING_BYTES: usize = 4 * 1024;

pub enum Request {
    Capabilities {
        protocol_version: u32,
        request_id: String,
    },
    Export(Box<ExportRequest>),
    Conform(ConformRequest),
    Still(Box<StillRequest>),
    Compose(Box<ComposeRequest>),
}

/// A compose or template request: no GPU, no media read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeRequest {
    pub protocol_version: u32,
    pub request_id: String,
    /// `compile`, `bind`, `dataSchema`, `lint`, `requirements` or `templates`.
    pub action: String,
    /// The spec `compile` and `requirements` read.
    #[serde(default)]
    pub spec: Option<serde_json::Value>,
    /// The template `bind`, `dataSchema` and `lint` read.
    #[serde(default)]
    pub template: Option<serde_json::Value>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub facts: Option<serde_json::Value>,
    /// Font files `compile` measures text with.
    #[serde(default)]
    pub fonts: Vec<PathBuf>,
    #[serde(default)]
    pub theme: Option<serde_json::Value>,
    #[serde(default)]
    pub templates: Vec<serde_json::Value>,
}

/// Kept as text so the engine checks its size before parsing; the host never holds a tree of a
/// document the engine would refuse.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct ProjectJson(Box<RawValue>);

impl ProjectJson {
    pub fn from_value(project: &Value) -> Result<Self, HostError> {
        serde_json::value::to_raw_value(project)
            .map(Self)
            .map_err(HostError::json)
    }

    pub fn text(&self) -> &str {
        self.0.get()
    }
}

#[derive(Deserialize)]
struct CapabilitiesRequest {
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "requestId")]
    request_id: String,
}

/// A longer line goes to the server, which refuses it. A cancel names two ids and nothing else.
pub const MAX_CANCEL_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    #[serde(rename = "protocolVersion")]
    protocol_version: u64,
    #[serde(rename = "requestId", default)]
    pub request_id: String,
    #[serde(default)]
    pub target: String,
}

/// # Errors
/// `UNSUPPORTED_PROTOCOL` for another protocol version, `INVALID_REQUEST` for a malformed line or
/// an empty `requestId` or `target`.
pub fn parse_cancel(line: &str) -> Result<CancelRequest, HostError> {
    let cancel: CancelRequest = serde_json::from_str(line)
        .map_err(|error| HostError::new("INVALID_REQUEST", format!("invalid request: {error}")))?;
    if cancel.protocol_version != u64::from(PROTOCOL_VERSION) {
        return Err(HostError::new(
            "UNSUPPORTED_PROTOCOL",
            format!(
                "unsupported protocol major {}; this host supports {PROTOCOL_VERSION}",
                cancel.protocol_version
            ),
        ));
    }
    if cancel.request_id.is_empty() {
        return Err(HostError::new(
            "INVALID_REQUEST",
            "requestId must not be empty",
        ));
    }
    if cancel.target.is_empty() {
        return Err(HostError::new(
            "INVALID_REQUEST",
            "a cancel names the request to cancel in a non-empty `target`",
        ));
    }
    Ok(cancel)
}

#[derive(Deserialize)]
pub struct ExportRequest {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub project: ProjectJson,
    pub assets: HashMap<String, PathBuf>,
    /// By source ID.
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    /// In project order, by source ID; `null` where the sequence has a gap.
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// Files an SVG's `<image href>` or a Lottie file's image assets name, by source ID and then by
    /// locator.
    #[serde(default)]
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
    /// Originals unless the caller asks for proxies.
    #[serde(default)]
    pub media: ExportMedia,
    #[serde(rename = "effectAssets", default)]
    pub effect_assets: HashMap<String, PathBuf>,
    /// In fallback order.
    #[serde(rename = "fonts", default)]
    pub font_files: Vec<PathBuf>,
    #[serde(default)]
    pub effects: Vec<EffectRequest>,
    #[serde(rename = "startTicks")]
    pub start_ticks: i64,
    #[serde(rename = "endTicks")]
    pub end_ticks: i64,
    #[serde(rename = "frameRate")]
    pub frame_rate: RateRequest,
    /// Defaults to true, as the CLI, @alpha2-sh/node and the browser exporter do.
    #[serde(rename = "includeAudio", default = "include_audio_by_default")]
    pub include_audio: bool,
    /// Absent: the one `output`'s extension names.
    #[serde(default)]
    pub container: Option<Container>,
    /// Absent: the container's default.
    #[serde(rename = "videoCodec", default)]
    pub video_codec: Option<VideoCodec>,
    /// Draw without the project background and write an alpha plane (VP9 in `WebM` or Matroska,
    /// `ProRes` 4444).
    #[serde(default)]
    pub alpha: bool,
    /// 8 or 10; absent: the codec's own (10 for `ProRes`, else 8).
    #[serde(rename = "bitDepth", default)]
    pub bit_depth: Option<u8>,
    #[serde(rename = "videoEncoder", default)]
    pub video_encoder: Option<String>,
    #[serde(rename = "inputPolicy", default)]
    pub input_policy: crate::config::VideoInputPolicy,
    #[serde(default)]
    pub strategy: crate::config::ExportStrategy,
    #[serde(rename = "videoQuality", default)]
    pub video_quality: VideoQualityRequest,
    /// Absent: the container's default.
    #[serde(rename = "audioCodec", default)]
    pub audio_codec: Option<AudioCodec>,
    #[serde(rename = "audioQuality", default)]
    pub audio_quality: AudioQualityRequest,
    /// Absent: the mix keeps its own loudness.
    #[serde(default)]
    pub loudness: Option<LoudnessRequest>,
    #[serde(rename = "renderCache", default, deserialize_with = "render_cache")]
    pub render_cache: Option<crate::render_cache::RenderCacheRequest>,
    /// Absent fields fall back to the environment, then the defaults ([`crate::memory`]).
    #[serde(default)]
    pub memory: crate::memory::MemoryRequest,
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
    /// `INVALID_INPUT` naming the rule: loudness needs `includeAudio`, and the container and codecs
    /// must resolve ([`Self::format`]).
    pub fn check(&self) -> Result<(), HostError> {
        if self.loudness.is_some() && !self.include_audio {
            return Err(HostError::invalid_input(
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
    pub fn format(&self) -> Result<ResolvedFormat, HostError> {
        let bit_depth = self
            .bit_depth
            .map(|bits| {
                alpha2_media::BitDepth::from_bits(bits).ok_or_else(|| {
                    HostError::invalid_input(format!("bitDepth: {bits} is not 8 or 10"))
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

/// Resolved once at the host's edge from the engine's policy
/// ([`alpha2_media::resolve_export_format`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResolvedFormat {
    pub container: Container,
    pub video: NativeVideoCodec,
    pub audio: NativeAudioCodec,
    /// ([`alpha2_media::resolve_pixels`]).
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
    ) -> Result<Self, HostError> {
        let extension = output
            .extension()
            .map_or(Some(""), std::ffi::OsStr::to_str)
            .ok_or_else(|| HostError::invalid_input("output path is not UTF-8"))?;
        let (container, video, audio) =
            alpha2_media::resolve_export_format(container, Some(extension), video, audio)
                .map_err(|error| HostError::invalid_input(container_message(&error)))?;
        Ok(Self {
            container,
            video: NativeVideoCodec::from_model(video),
            audio: NativeAudioCodec::from_model(audio),
            pixels: alpha2_media::resolve_pixels(
                video,
                container,
                alpha2_media::OutputPixels::default(),
            )
            .map_err(|error| HostError::invalid_input(format!("videoCodec: {error}")))?,
        })
    }

    /// # Errors
    /// `INVALID_INPUT` naming `alpha` or `bitDepth` when the codec cannot be written so in the
    /// container.
    pub fn with_pixels(self, pixels: alpha2_media::OutputPixels) -> Result<Self, HostError> {
        let layout = alpha2_media::resolve_pixels(self.video.model(), self.container, pixels)
            .map_err(|error| {
                let field = match error {
                    alpha2_media::OutputPixelsError::BitDepthUnsupported { .. } => "bitDepth",
                    _ => "alpha",
                };
                HostError::invalid_input(format!("{field}: {error}"))
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
fn check_export_settings(
    setting: &dyn Fn(&str) -> Result<Option<Value>, HostError>,
) -> Result<(), HostError> {
    if let Some(quality) = setting("videoQuality")? {
        VideoQualityRequest::from_json(&quality).map_err(HostError::invalid_input)?;
    }
    if let Some(quality) = setting("audioQuality")? {
        AudioQualityRequest::from_json(&quality).map_err(HostError::invalid_input)?;
    }
    if let Some(cache) = setting("renderCache")?.filter(|value| !value.is_null()) {
        crate::render_cache::RenderCacheRequest::from_json(&cache)
            .map_err(HostError::invalid_input)?;
    }
    if let Some(loudness) = setting("loudness")?.filter(|value| !value.is_null()) {
        LoudnessRequest::from_json(&loudness).map_err(HostError::invalid_input)?;
    }
    // A budget the caller sets is its input too: a misspelled or mistyped field names itself.
    if let Some(alpha) = setting("alpha")?
        && !alpha.is_boolean()
    {
        return Err(HostError::invalid_input(format!(
            "alpha: {alpha} is not true or false"
        )));
    }
    if let Some(bits) = setting("bitDepth")?.filter(|value| !value.is_null())
        && !matches!(bits.as_u64(), Some(8 | 10))
    {
        return Err(HostError::invalid_input(format!(
            "bitDepth: {bits} is not 8 or 10"
        )));
    }
    if let Some(memory) = setting("memory")? {
        serde_json::from_value::<crate::memory::MemoryRequest>(memory)
            .map_err(|error| HostError::invalid_input(format!("memory: {error}")))?;
    }
    check_format_names(setting)
}

/// An unknown `container`, `videoCodec` or `audioCodec` is `INVALID_INPUT` naming the field, not a
/// malformed request.
fn check_format_names(
    setting: &dyn Fn(&str) -> Result<Option<Value>, HostError>,
) -> Result<(), HostError> {
    if let Some(container) = setting("container")?.filter(|value| !value.is_null()) {
        serde_json::from_value::<Container>(container.clone()).map_err(|_| {
            HostError::invalid_input(format!(
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
                return Err(HostError::invalid_input(format!(
                    "{field}: unknown codec {codec}; expected one of {names}"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct ConformRequest {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub project: ProjectJson,
    #[serde(default)]
    pub assets: HashMap<String, PathBuf>,
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    #[serde(default)]
    pub purpose: PurposeRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StillRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: String,
    pub project: ProjectJson,
    pub assets: HashMap<String, PathBuf>,
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    #[serde(default)]
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
    #[serde(default)]
    pub media: ExportMedia,
    #[serde(default)]
    pub effect_assets: HashMap<String, PathBuf>,
    #[serde(rename = "fonts", default)]
    pub font_files: Vec<PathBuf>,
    #[serde(default)]
    pub effects: Vec<EffectRequest>,
    /// Absent: the project's canvas.
    #[serde(default)]
    pub size: Option<StillSize>,
    /// 1 to [`MAX_STILLS`], each to its own file.
    pub stills: Vec<StillEntry>,
    /// Draw without the project background, over transparent pixels.
    #[serde(default)]
    pub alpha: bool,
    #[serde(default)]
    pub overwrite: bool,
}

pub const MAX_STILLS: usize = 256;

pub const MAX_STILL_DIMENSION: u32 = alpha2_model::MAX_CANVAS_DIMENSION;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StillSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StillEntry {
    /// Ticks; the frame drawn is the grid frame that holds it.
    pub time: i64,
    pub output: PathBuf,
}

impl StillRequest {
    /// # Errors
    /// `INVALID_INPUT` for no stills or more than [`MAX_STILLS`], a negative time, two stills to
    /// one output, or a size out of `1..=MAX_STILL_DIMENSION`.
    pub fn check(&self) -> Result<(), HostError> {
        if self.operation != "still" {
            return Err(HostError::new(
                "INVALID_REQUEST",
                "invalid request: the operation is not `still`",
            ));
        }
        if self.stills.is_empty() || self.stills.len() > MAX_STILLS {
            return Err(HostError::invalid_input(format!(
                "stills has {} entries; a still request renders 1 to {MAX_STILLS}",
                self.stills.len()
            )));
        }
        // Keyed as a case- and normalization-insensitive file system sees the name.
        let mut outputs = std::collections::HashMap::new();
        for still in &self.stills {
            if still.time < 0 {
                return Err(HostError::invalid_input(format!(
                    "a still's time is a timeline time from 0, not {}",
                    still.time
                )));
            }
            if still.output.as_os_str().is_empty() {
                return Err(HostError::invalid_input(
                    "a still's output must not be empty",
                ));
            }
            if let Some(earlier) =
                outputs.insert(crate::paths::output_key(&still.output), &still.output)
            {
                return Err(HostError::invalid_input(format!(
                    "two stills write one file: {} and {}",
                    earlier.display(),
                    still.output.display()
                )));
            }
        }
        let fits = |pixels: u32| (1..=MAX_STILL_DIMENSION).contains(&pixels);
        if self
            .size
            .is_some_and(|size| !fits(size.width) || !fits(size.height))
        {
            return Err(HostError::invalid_input(format!(
                "a still's size is 1 to {MAX_STILL_DIMENSION} pixels each way"
            )));
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExportMedia {
    #[default]
    Original,
    /// Fresh proxies, and originals where there are none.
    Proxies,
}

impl ExportMedia {
    pub const fn purpose(self) -> MediaPurpose {
        match self {
            Self::Original => MediaPurpose::ExportOriginal,
            Self::Proxies => MediaPurpose::ExportProxy,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum PurposeRequest {
    Preview,
    PreviewProxy,
    Thumbnail,
    RemoteEdit,
    #[default]
    ExportOriginal,
    ExportProxy,
}

impl PurposeRequest {
    pub const fn purpose(self) -> MediaPurpose {
        match self {
            Self::Preview => MediaPurpose::Preview,
            Self::PreviewProxy => MediaPurpose::PreviewProxy,
            Self::Thumbnail => MediaPurpose::Thumbnail,
            Self::RemoteEdit => MediaPurpose::RemoteEdit,
            Self::ExportOriginal => MediaPurpose::ExportOriginal,
            Self::ExportProxy => MediaPurpose::ExportProxy,
        }
    }
}

impl Request {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Capabilities { request_id, .. } => request_id,
            Self::Export(export) => &export.request_id,
            Self::Conform(conform) => &conform.request_id,
            Self::Still(still) => &still.request_id,
            Self::Compose(compose) => &compose.request_id,
        }
    }

    pub fn protocol_version(&self) -> u32 {
        match self {
            Self::Capabilities {
                protocol_version, ..
            } => *protocol_version,
            Self::Export(export) => export.protocol_version,
            Self::Conform(conform) => conform.protocol_version,
            Self::Still(still) => still.protocol_version,
            Self::Compose(compose) => compose.protocol_version,
        }
    }
}

/// Only the few small settings checked first become JSON trees: the top level is read as raw text
/// per field, the operation's fields parse into their types, and the project stays text until the
/// engine checks its size.
pub fn parse_request(line: &str) -> Result<Request, HostError> {
    let invalid = |error: serde_json::Error| {
        HostError::new("INVALID_REQUEST", format!("invalid request: {error}"))
    };
    let fields: HashMap<String, &RawValue> = serde_json::from_str(line).map_err(|error| {
        let what = if error.is_data() {
            "a request is a JSON object"
        } else {
            "invalid JSON"
        };
        HostError::new("INVALID_REQUEST", format!("{what}: {error}"))
    })?;
    let setting = |name: &str| -> Result<Option<Value>, HostError> {
        let Some(raw) = fields.get(name) else {
            return Ok(None);
        };
        if raw.get().len() > MAX_REQUEST_SETTING_BYTES {
            return Err(HostError::invalid_input(format!(
                "{name} is {} bytes; a request setting is at most {MAX_REQUEST_SETTING_BYTES}",
                raw.get().len()
            )));
        }
        serde_json::from_str(raw.get())
            .map(Some)
            .map_err(|error| HostError::new("INVALID_REQUEST", format!("invalid JSON: {error}")))
    };
    let version = fields
        .get("protocolVersion")
        .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
        .ok_or_else(|| {
            HostError::new(
                "UNSUPPORTED_PROTOCOL",
                "protocolVersion must be the integer 1",
            )
        })?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(HostError::new(
            "UNSUPPORTED_PROTOCOL",
            format!("unsupported protocol major {version}; this host supports {PROTOCOL_VERSION}"),
        ));
    }
    let operation = fields
        .get("operation")
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok());
    let request = match operation.as_deref() {
        Some("capabilities") => {
            let CapabilitiesRequest {
                protocol_version,
                request_id,
            } = serde_json::from_str(line).map_err(invalid)?;
            Request::Capabilities {
                protocol_version,
                request_id,
            }
        }
        Some("export") => {
            check_export_settings(&setting)?;
            let export: ExportRequest = serde_json::from_str(line).map_err(invalid)?;
            export.check()?;
            Request::Export(Box::new(export))
        }
        Some("conform") => Request::Conform(serde_json::from_str(line).map_err(invalid)?),
        Some("compose") => Request::Compose(Box::new(serde_json::from_str(line).map_err(invalid)?)),
        Some("still") => {
            let still: StillRequest = serde_json::from_str(line).map_err(invalid)?;
            still.check()?;
            Request::Still(Box::new(still))
        }
        // The reader answers every cancel line it takes; one reaches here only when too long.
        Some("cancel") => {
            return Err(HostError::new(
                "INVALID_REQUEST",
                format!("a cancel line is at most {MAX_CANCEL_LINE_BYTES} bytes"),
            ));
        }
        Some(other) => {
            return Err(HostError::new(
                "INVALID_REQUEST",
                format!(
                    "invalid request: unknown operation `{other}`, expected one of `capabilities`, `export`, `conform`, `still`, `compose`, `cancel`"
                ),
            ));
        }
        None => {
            return Err(HostError::new(
                "INVALID_REQUEST",
                "invalid request: missing field `operation`",
            ));
        }
    };
    debug_assert_eq!(request.protocol_version(), PROTOCOL_VERSION);
    if request.request_id().is_empty() {
        return Err(HostError::new(
            "INVALID_REQUEST",
            "requestId must not be empty",
        ));
    }
    Ok(request)
}

const fn include_audio_by_default() -> bool {
    true
}

fn render_cache<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<crate::render_cache::RenderCacheRequest>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            crate::render_cache::RenderCacheRequest::from_json(&value)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

/// The rest of the line is skipped, not parsed into a tree.
pub fn request_id_from_line(line: &str) -> String {
    #[derive(Deserialize)]
    struct Id {
        #[serde(rename = "requestId")]
        request_id: Option<String>,
    }
    serde_json::from_str::<Id>(line)
        .ok()
        .and_then(|id| id.request_id)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Says whether a refusal answers an export, whose error carries a failure report.
pub fn operation_from_line(line: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Operation {
        operation: Option<String>,
    }
    serde_json::from_str::<Operation>(line)
        .ok()
        .and_then(|operation| operation.operation)
}

crate::tag::tags! {
    /// Like every tag here, it is stripped before the protocol error is serialized
    /// ([`crate::tag`]).
    pub fn gpu_timeout_tag = "GPU_TIMEOUT] ";
    /// A defect: code panicked while handling the request.
    pub fn internal_tag = "INTERNAL] ";
    pub fn decode_stalled_tag = "DECODE_STALLED] ";
    pub fn encode_stalled_tag = "ENCODE_STALLED] ";
    /// `inputPolicy: gpu-required` found the GPU-resident path unavailable.
    pub fn gpu_output_unavailable_tag = "GPU_OUTPUT_UNAVAILABLE] ";
    /// `strategy: require-copy` found the range cannot be copied.
    pub fn copy_ineligible_tag = "COPY_INELIGIBLE] ";
    fn code_tag_open = "code=";
    fn encoder_rejected_tag_open = "rejected=";
    fn kept_path_tag_open = "kept=";
}
/// Kept in a [`decode_stalled_tag`] message to say software decode may succeed. The
/// `resource: "gpu"` comes from the stall's evidence ([`crate::failure::hardware_stall`]), not from
/// these words.
pub const HARDWARE_DECODE_STALL_MARKER: &str = "holding hardware decode sessions";
/// Kept in an [`encode_stalled_tag`] message. The `resource: "gpu"` comes from the stall's
/// evidence, not from these words.
pub const HARDWARE_ENCODE_STALL_MARKER: &str = "holding a hardware encode session";

pub const PROTOCOL_CODES: [&str; 25] = [
    "UNSUPPORTED_PROTOCOL",
    "INVALID_REQUEST",
    "INVALID_INPUT",
    "CONFORM_FAILED",
    "MISSING_INPUT",
    "INCOMPLETE_COMPOSITION",
    "EFFECT_ERROR",
    "GPU_INIT_FAILED",
    "HARDWARE_DECODE_UNAVAILABLE",
    "GPU_DEVICE_LOST",
    "GPU_DEVICE_HUNG",
    "GPU_TIMEOUT",
    "DECODE_STALLED",
    "ENCODE_STALLED",
    "COPY_INELIGIBLE",
    "GPU_OUTPUT_UNAVAILABLE",
    "ENCODE_ERROR",
    "ENCODER_BUSY",
    "IO_ERROR",
    "OUTPUT_IN_USE",
    "OUTPUT_PUBLISH_FAILED",
    "CANCELLED",
    "RESOURCE_LIMIT",
    "INTERNAL",
    "ALPHA2_ERROR",
];

/// Keeps an existing code, since the first is the cause's. [`HostError::classify`] takes and
/// strips it.
pub fn code_tag(code: &'static str, message: impl Into<String>) -> String {
    let message = message.into();
    debug_assert!(
        PROTOCOL_CODES.contains(&code),
        "{code} is not a protocol code"
    );
    if message.contains(code_tag_open()) {
        return message;
    }
    format!("{}{code}] {message}", code_tag_open())
}

/// The encoder refused frame `frame` on a still-usable device, so the export may start over on
/// another path; otherwise it is answered `ENCODE_ERROR`.
pub fn encoder_rejected_tag(frame: u64, message: impl Into<String>) -> String {
    let message = message.into();
    if message.contains(encoder_rejected_tag_open()) {
        return message;
    }
    code_tag(
        "ENCODE_ERROR",
        format!("{}{frame}] {message}", encoder_rejected_tag_open()),
    )
}

/// Answered `OUTPUT_PUBLISH_FAILED`, with the path the finished file was kept at, if any, as
/// `details.keptPath`. The path travels as a JSON string with `]` escaped so it cannot end the tag.
pub fn output_publish_failed(kept: Option<&std::path::Path>, message: impl Into<String>) -> String {
    let message = message.into();
    let tag = kept.map_or_else(String::new, |kept| {
        let path = Value::String(crate::host::written_path(kept))
            .to_string()
            .replace(']', "\\u005d");
        format!("{}{path}] ", kept_path_tag_open())
    });
    code_tag("OUTPUT_PUBLISH_FAILED", format!("{tag}{message}"))
}

fn take_kept_path(mut message: String) -> (Option<String>, String) {
    let mut kept = None;
    while let Some(start) = message.find(kept_path_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        if kept.is_none() {
            kept = serde_json::from_str::<String>(
                &message[start + kept_path_tag_open().len()..end - 2],
            )
            .ok();
        }
        message.replace_range(start..end, "");
    }
    (kept, message)
}

#[cfg(feature = "hwencode")]
pub fn take_encoder_rejected(mut message: String) -> (Option<u64>, String) {
    let mut frame = None;
    while let Some(start) = message.find(encoder_rejected_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        if frame.is_none() {
            frame = message[start + encoder_rejected_tag_open().len()..end - 2]
                .parse()
                .ok();
        }
        message.replace_range(start..end, "");
    }
    (frame, message)
}

pub fn missing_input(message: impl Into<String>) -> String {
    code_tag("MISSING_INPUT", message)
}

pub fn cancelled(message: impl Into<String>) -> String {
    code_tag("CANCELLED", message)
}

pub fn is_cancelled(message: &str) -> bool {
    message.contains(&format!("{}CANCELLED] ", code_tag_open()))
}

/// A timed-out ([`gpu_timeout_tag`]) or lost device; nothing else on that device can stand in for
/// the work.
pub fn names_device_fault(message: &str) -> bool {
    message.contains(gpu_timeout_tag())
        || message.contains(&format!("{}GPU_DEVICE_LOST] ", code_tag_open()))
}

fn take_code_tag(mut message: String) -> (Option<&'static str>, String) {
    let mut code = None;
    while let Some(start) = message.find(code_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        let name = &message[start + code_tag_open().len()..end - 2];
        if code.is_none() {
            code = PROTOCOL_CODES.iter().copied().find(|known| *known == name);
        }
        message.replace_range(start..end, "");
    }
    (code, message)
}

/// The `error` object of a failed response.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostError {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// For a failed export ([`crate::failure`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Value>,
    /// Typed evidence no protocol code names, from [`crate::failure::evidence_tag`]. Not
    /// serialized.
    #[serde(skip)]
    pub evidence: Option<&'static str>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProgressPhase {
    Frames,
    /// Encoders flushing and the container being written; the output is not complete until the
    /// response.
    Finalizing,
}

impl ProgressPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Frames => "frames",
            Self::Finalizing => "finalizing",
        }
    }
}

/// Frames an encoder completes while flushing still count, but the phase stays `finalizing`.
pub fn finalizing(
    progress: &mut dyn FnMut(Progress) -> Result<(), HostError>,
) -> impl FnMut(Progress) -> Result<(), HostError> + '_ {
    move |event| {
        progress(Progress {
            phase: ProgressPhase::Finalizing,
            ..event
        })
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Progress {
    pub phase: ProgressPhase,
    pub completed: u64,
    pub total: u64,
    /// The GPU-resident encoder rejected a frame and the export restarted on the readback path.
    /// Written only when true.
    pub restarted: bool,
}

impl Progress {
    pub const fn frames(completed: u64, total: u64) -> Self {
        Self {
            phase: ProgressPhase::Frames,
            completed,
            total,
            restarted: false,
        }
    }

    #[cfg(feature = "hwencode")]
    pub const fn restarted(total: u64) -> Self {
        Self {
            phase: ProgressPhase::Frames,
            completed: 0,
            total,
            restarted: true,
        }
    }

    /// `completed` is what the encoder has finished so far; reporting `total` here would claim
    /// frames still in the encoder, and the next report would go back.
    pub const fn finalizing(completed: u64, total: u64) -> Self {
        Self {
            phase: ProgressPhase::Finalizing,
            completed,
            total,
            restarted: false,
        }
    }
}

impl HostError {
    /// Any tag of this process in `message` is stripped, so no tag leaves the process.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: crate::tag::untagged(&message.into()),
            details: None,
            failure: None,
            evidence: None,
        }
    }

    #[must_use]
    pub fn with_failure(mut self, failure: Value) -> Self {
        self.failure = Some(failure);
        self
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("INVALID_INPUT", message)
    }

    #[allow(clippy::needless_pass_by_value)] // Exact signature required by Result::map_err.
    pub fn io(error: std::io::Error) -> Self {
        Self::new("IO_ERROR", error.to_string())
    }

    #[allow(clippy::needless_pass_by_value)] // Exact signature required by Result::map_err.
    pub fn json(error: serde_json::Error) -> Self {
        Self::new("ALPHA2_ERROR", error.to_string())
    }

    /// Takes the typed evidence, then the first code tag or marker, and strips every internal
    /// tag. Only this process's tags decide the code, never the words; untagged is `ALPHA2_ERROR`.
    pub fn classify(message: String) -> Self {
        let (evidence, message) = crate::failure::take_evidence_tag(message);
        let (kept, message) = take_kept_path(message);
        let stage = crate::memory::take_stage(&message);
        let mut error = Self::classify_code(message);
        error.evidence = evidence;
        if let Some(kept) = kept {
            error.details = Some(json!({ "keptPath": kept }));
        }
        // A memory refusal's step, which its `failure.stage` repeats.
        if let Some(stage) = stage {
            error.details = Some(json!({ "stage": stage.as_str() }));
        }
        error
    }

    fn classify_code(message: String) -> Self {
        let (code, message) = take_code_tag(crate::failure::strip_phase_tags(message));
        for (tag, code) in [
            (internal_tag(), "INTERNAL"),
            (crate::memory::resource_limit_tag(), "RESOURCE_LIMIT"),
            (copy_ineligible_tag(), "COPY_INELIGIBLE"),
            (gpu_output_unavailable_tag(), "GPU_OUTPUT_UNAVAILABLE"),
            (decode_stalled_tag(), "DECODE_STALLED"),
            (encode_stalled_tag(), "ENCODE_STALLED"),
            (gpu_timeout_tag(), "GPU_TIMEOUT"),
        ] {
            if message.contains(tag) {
                return Self::new(code, message);
            }
        }
        Self::new(code.unwrap_or("ALPHA2_ERROR"), message)
    }
}

#[derive(Copy, Clone, Deserialize)]
pub struct RateRequest {
    pub num: u32,
    pub den: u32,
}

/// AV1 on NVENC and Vulkan runs to 255. A value past the chosen codec's scale is the encoder's to
/// refuse, and the bitrate fallback runs.
pub const MAX_QUANTIZER: u32 = 255;

/// One of the engine's five levels or an explicit rate. A level is resolved into a
/// [`VideoRateControl`] before any sink opens.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VideoQualityRequest {
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

/// EBU R128 / ITU-R BS.1770-4: `{ "targetLufs": <LUFS>, "truePeakCeilingDbtp": <dBTP> }`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LoudnessRequest {
    /// Integrated loudness, in `-60..=0` LUFS.
    pub target_lufs: f64,
    /// In `-20..=0` dBTP. When the target would pass it, the gain is lowered and the export left
    /// quieter than the target.
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
                "loudness must be an object {{ targetLufs, truePeakCeilingDbtp }}, received {value}"
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
        let number = |key: &str| {
            fields
                .get(key)
                .ok_or_else(|| format!("loudness.{key} is required"))?
                .as_f64()
                .ok_or_else(|| format!("loudness.{key} must be a number"))
        };
        Self::new(number("targetLufs")?, number("truePeakCeilingDbtp")?)
    }
}

impl<'de> Deserialize<'de> for LoudnessRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AudioQualityRequest {
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

/// `deviceReady` says a shared decode context exists, which is separate from a codec being
/// registered or any stream decoding on the GPU.
pub fn decode_capabilities_json(
    mode: &str,
    device_ready: bool,
    ten_bit: bool,
    device_codecs: Option<&[&'static str]>,
) -> Value {
    let capabilities = hardware_decode_capabilities();
    json!({
        "mode": mode,
        "backend": capabilities.backend,
        "deviceReady": device_ready,
        "tenBit": ten_bit,
        "deviceCodecs": device_codecs,
        "codecs": capabilities.codecs.into_iter().map(|capability| json!({
            "id": capability.codec.as_str(),
            "decoder": capability.decoder_name,
            "hardwareConfig": capability.hardware_config,
            "softwareDecoder": capability.software_decoder,
        })).collect::<Vec<_>>(),
        "alpha": alpha_decode_codecs(),
    })
}

/// VP9 alpha needs libvpx's VP9 decoder; HEVC's auxiliary alpha layer is not decoded.
fn alpha_decode_codecs() -> Vec<&'static str> {
    let mut codecs = vec!["prores"];
    if alpha2_native::decodes_vp9_alpha() {
        codecs.push("vp9");
    }
    codecs
}

/// Status (`available`, `not-installed`, `disabled`, `unusable`), what loaded, why not, and the
/// remedy. Cisco's licence notice is always included, since this is where a user sees the codec.
pub fn openh264_json() -> Value {
    let status = alpha2_native::openh264_status();
    let library = status.library();
    json!({
        "status": status.as_str(),
        "encoder": alpha2_native::OPENH264_ENCODER_NAME,
        "provider": library.map(|library| library.provider().as_str()),
        "version": library.map(|library| library.version()),
        "library": library.map(|library| library.path()),
        "pinnedVersion": alpha2_native::OPENH264_VERSION,
        "notice": alpha2_native::OPENH264_NOTICE,
        "reason": status.reason(),
        "remedy": status.remedy(),
    })
}

/// `available` when an encoder takes CPU frames, `gpu-only` when only the GPU-resident path
/// encodes it (an export that path declines fails), `unavailable` otherwise, with `reason` and
/// `remedy`.
pub fn h264_status_json(video: &Value, gpu_output_path: &Value) -> Value {
    let id = NativeVideoCodec::H264.as_str();
    let encoders = video
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["id"] == id)
        .filter_map(|entry| entry["encoder"].as_str())
        .collect::<Vec<_>>();
    let gpu_encoder = gpu_output_path["codecs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["videoCodec"] == id)
        .and_then(|entry| entry["encoder"].as_str());
    let (status, reason) = match (encoders.is_empty(), gpu_encoder) {
        (false, _) => ("available", None),
        (true, Some(encoder)) => (
            "gpu-only",
            Some(format!(
                "only the GPU-resident path encodes H.264 here ({encoder}), so an export it \
                 declines (input policy cpu-readback, a size outside its limits, \
                 ALPHA2_GPU_ENCODE=0) has no H.264 encoder"
            )),
        ),
        (true, None) => (
            "unavailable",
            Some(format!(
                "no H.264 encoder takes CPU frames, and the GPU-resident path is unavailable \
                 ({})",
                gpu_output_path["reason"]
                    .as_str()
                    .unwrap_or("it encodes no H.264 on this device")
            )),
        ),
    };
    json!({
        "status": status,
        "encoders": encoders,
        "gpuEncoder": gpu_encoder,
        "reason": reason,
        "remedy": reason.is_some().then(alpha2_native::h264_encoder_remedy),
    })
}

/// `OpenH264`'s provider, version, path and notice, or null for an `FFmpeg` encoder.
pub fn encoder_library_json(name: &str) -> Value {
    if name != alpha2_native::OPENH264_ENCODER_NAME {
        return Value::Null;
    }
    alpha2_native::openh264_status()
        .library()
        .map_or(Value::Null, |library| {
            json!({
                "provider": library.provider().as_str(),
                "version": library.version(),
                "library": library.path(),
                "notice": alpha2_native::OPENH264_NOTICE,
            })
        })
}

/// Codecs whose encoder here takes alpha or 10-bit pictures under the policy
/// ([`alpha2_media::resolve_pixels`]). 10-bit also needs the `rgba16float` intermediate.
fn output_pixels_json(capabilities: &alpha2_native::NativeCodecCapabilities) -> Value {
    let codecs = |pixels: alpha2_media::OutputPixels| {
        alpha2_media::VideoCodec::ALL
            .into_iter()
            .filter(|&codec| {
                Container::ALL.into_iter().any(|container| {
                    container.offers_video(codec)
                        && alpha2_media::resolve_pixels(codec, container, pixels).is_ok_and(
                            |layout| {
                                alpha2_native::encodes_pixels(
                                    capabilities,
                                    NativeVideoCodec::from_model(codec),
                                    layout,
                                )
                            },
                        )
                })
            })
            .map(alpha2_media::VideoCodec::as_str)
            .collect::<Vec<_>>()
    };
    json!({
        "alpha": codecs(alpha2_media::OutputPixels { bit_depth: None, alpha: true }),
        "tenBit": codecs(alpha2_media::OutputPixels {
            bit_depth: Some(alpha2_media::BitDepth::Ten),
            alpha: false,
        }),
    })
}

/// The containers are the engine's policy on every runtime; the encoders say which codecs this
/// runtime encodes.
pub fn codec_capabilities_json() -> Value {
    let capabilities = codec_capabilities();
    let output = output_pixels_json(&capabilities);
    json!({
        "containers": Container::ALL.into_iter().map(|container| json!({
            "id": container.as_str(),
            "extensions": container.extensions(),
            "video": container.video_codecs().iter().map(|codec| codec.as_str()).collect::<Vec<_>>(),
            "audio": container.audio_codecs().iter().map(|codec| codec.as_str()).collect::<Vec<_>>(),
            "defaultVideoCodec": container.default_video_codec().as_str(),
            "defaultAudioCodec": container.default_audio_codec().as_str(),
        })).collect::<Vec<_>>(),
        "video": capabilities.video.into_iter().map(|capability| {
            let library = capability.library.as_ref();
            json!({
                "id": capability.codec.as_str(),
                "encoder": capability.encoder_name,
                "encoderClass": capability.class.as_str(),
                "hardwareAccelerated": capability.hardware_accelerated,
                "preferred": capability.preferred,
                "minimumWidth": capability.minimum_width,
                "minimumHeight": capability.minimum_height,
                // A user-installed encoder (OpenH264): provider, version, path, and the licence
                // notice to show. Null for FFmpeg's encoders.
                "provider": library.map(|library| library.provider),
                "version": library.map(|library| library.version.clone()),
                "library": library.map(|library| library.path.clone()),
                "notice": library.map(|library| library.notice),
            })
        }).collect::<Vec<_>>(),
        "openh264": openh264_json(),
        "output": output,
        "audio": capabilities.audio.into_iter().map(|capability| json!({
            "id": capability.codec.as_str(),
            "encoder": capability.encoder_name,
        })).collect::<Vec<_>>(),
    })
}
