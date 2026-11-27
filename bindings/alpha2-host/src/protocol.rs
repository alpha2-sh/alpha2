use alpha2_media::Container;
use alpha2_native::{NativeVideoCodec, codec_capabilities, hardware_decode_capabilities};
use serde::Serialize;
use serde_json::{Value, json};

pub use alpha2_host_core::requests::{
    AudioQualityRequest, ComposeRequest, ConformRequest, EditRequest, EffectRequest, ExportMedia,
    ExportRequest, LoudnessRequest, MAX_CONTROL_HEADER_BYTES, MAX_QUANTIZER, ProjectJson,
    RateRequest, Request, RequestError, ResolvedFormat, StillEntry, StillRequest, StillSize,
    VideoQualityRequest, operation_from_header, parse_cancel, parse_request, parse_status,
    request_id_from_header,
};

/// A project at the engine's [`alpha2_model::MAX_PROJECT_JSON_BYTES`] plus as much again for the
/// rest of the request. A longer header is answered `INVALID_REQUEST` without being parsed.
#[allow(clippy::cast_possible_truncation)]
pub const MAX_REQUEST_HEADER_BYTES: u32 = 2 * alpha2_model::MAX_PROJECT_JSON_BYTES as u32;

/// All the attachments of one request together.
pub const MAX_REQUEST_ATTACHMENT_BYTES: u32 = 256 * 1024 * 1024;

/// The longest header a response may have. A longer one is not written: the request is answered
/// `RESOURCE_LIMIT` instead.
pub const MAX_RESPONSE_HEADER_BYTES: u32 = 256 * 1024 * 1024;

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
    fn media_details_tag_open = "media=";
}
/// Kept in a [`decode_stalled_tag`] message to say software decode may succeed. The
/// `resource: "gpu"` comes from the stall's evidence ([`crate::failure::hardware_stall`]), not from
/// these words.
pub const HARDWARE_DECODE_STALL_MARKER: &str = "holding hardware decode sessions";
/// Kept in an [`encode_stalled_tag`] message. The `resource: "gpu"` comes from the stall's
/// evidence, not from these words.
pub const HARDWARE_ENCODE_STALL_MARKER: &str = "holding a hardware encode session";

pub const PROTOCOL_CODES: [&str; 48] = [
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
    "HOST_RETIRING",
    "INVALID_EMIT",
    "EXTRACTOR_FAILED",
    "EXTRACTOR_TIMEOUT",
    "PLUGIN_FETCH_FAILED",
    "PLUGIN_HASH_MISMATCH",
    "PLUGIN_ABI_UNSUPPORTED",
    "PLUGIN_NO_ARTIFACT",
    "PLUGIN_NATIVE_REFUSED",
    "PLUGIN_REFUSED",
    "PLUGIN_DESCRIPTOR_MISMATCH",
    "PLUGIN_UNSUPPORTED_FEATURE",
    "PLUGIN_LOAD_FAILED",
    "PLUGIN_TRAPPED",
    "PLUGIN_LIMIT",
    "PLUGIN_RESOURCE_INVALID",
    "MEDIA_REFUSED",
    "MEDIA_TLS_FAILED",
    "MEDIA_UNREACHABLE",
    "MEDIA_BUSY",
    "MEDIA_RANGE_UNSUPPORTED",
    "MEDIA_REJECTED",
    "MEDIA_CHANGED",
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

/// Answered with the remote failure's code and its `details` (shown URL, status, rule, delay). The
/// details travel as JSON with `]` escaped so they cannot end the tag.
pub fn source_failure(failure: &alpha2_media::SourceFailure) -> String {
    let details = crate::media_policy::source_error(failure)
        .details
        .unwrap_or_default()
        .to_string()
        .replace(']', "\\u005d");
    code_tag(
        failure.code,
        format!("{}{details}] {}", media_details_tag_open(), failure.message),
    )
}

fn take_media_details(mut message: String) -> (Option<Value>, String) {
    let mut details = None;
    while let Some(start) = message.find(media_details_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        if details.is_none() {
            details = serde_json::from_str::<Value>(
                &message[start + media_details_tag_open().len()..end - 2],
            )
            .ok();
        }
        message.replace_range(start..end, "");
    }
    (details, message)
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

/// Progress once the encoder was told to finish: it publishes the file whatever the caller does,
/// so a message that cannot be written must not fail the export.
pub fn after_finish(
    progress: &mut dyn FnMut(Progress) -> Result<(), HostError>,
) -> impl FnMut(Progress) -> Result<(), HostError> + '_ {
    move |event| {
        let _ = progress(event);
        Ok(())
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

impl From<RequestError> for HostError {
    fn from(error: RequestError) -> Self {
        Self::new(error.code, error.message)
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
        let (media, message) = take_media_details(message);
        let stage = crate::memory::take_stage(&message);
        let mut error = Self::classify_code(message);
        error.evidence = evidence;
        if let Some(media) = media {
            error.details = Some(media);
        }
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
        "sourceDecoders": alpha2_native::source_decoders(),
        "everyDecoder": alpha2_native::every_decoder_allowed(),
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
