use std::env;

use alpha2_media::{AudioCodec, BitrateMode, VideoCodec, VideoRateControl};
use alpha2_model::{MediaKind, Project};
use alpha2_time::{FrameRate, TimeRange};

use crate::protocol::{AudioQualityRequest, VideoQualityRequest};
use crate::{
    DEFAULT_DECODE_CONCURRENCY, DEFAULT_DECODE_LOOKAHEAD, DEFAULT_DECODER_SESSIONS,
    DEFAULT_IN_FLIGHT_VIDEO_FRAMES, DEFAULT_REVERSE_HARDWARE_SURFACES, MAX_DECODE_CONCURRENCY,
    MAX_DECODE_LOOKAHEAD, MAX_DECODER_SESSIONS, MAX_ENCODE_CHANNEL_DEPTH,
    MAX_IN_FLIGHT_VIDEO_FRAMES, MAX_REVERSE_HARDWARE_SURFACES,
};

/// `ALPHA2_GPU_TIMEOUT_MS` (default 30 s, enough for slow GPUs rendering large frames).
pub fn configured_gpu_wait_timeout() -> std::time::Duration {
    env::var("ALPHA2_GPU_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map_or(
            alpha2_render::DEFAULT_GPU_WAIT_TIMEOUT,
            std::time::Duration::from_millis,
        )
}

/// How long a worker may stay inside one `FFmpeg` operation before `DECODE_STALLED` or
/// `ENCODE_STALLED`.
///
/// A decoder operation spends at most [`alpha2_native::DECODE_WORK_LIMIT`] between reported
/// frames, and none of this host's GPU waits outlasts [`configured_gpu_wait_timeout`], so a call
/// longer than both is blocked inside `FFmpeg`. Measured on [`alpha2_os::AwakeInstant`], so a
/// laptop sleeping through an export has not stalled.
pub fn configured_stall_timeout() -> std::time::Duration {
    alpha2_native::DECODE_WORK_LIMIT.saturating_add(configured_gpu_wait_timeout())
}

/// `ALPHA2_GPU_ENCODE` set to `0`, `false` or `off` forces the readback path, to compare the two or
/// avoid a driver problem without rebuilding. `video_session_hazard` is why `FFmpeg` must not open
/// a video session on the device (Linux only).
#[cfg(feature = "hwencode")]
pub fn configured_gpu_encode(video_session_hazard: Option<String>) -> Result<(), String> {
    gpu_encode_gate(
        env::var("ALPHA2_GPU_ENCODE").ok().as_deref(),
        video_session_hazard,
    )
}

/// No value turns the path on against a hazard. Any value but an off one is reported once and
/// read as unset.
#[cfg(feature = "hwencode")]
pub fn gpu_encode_gate(setting: Option<&str>, hazard: Option<String>) -> Result<(), String> {
    match setting {
        Some("0" | "false" | "off") => return Err("ALPHA2_GPU_ENCODE is off".to_owned()),
        None | Some("") => {}
        Some(other) => {
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| {
                alpha2_os::diagnostic!(
                    "alpha2-host: ignoring ALPHA2_GPU_ENCODE={other}: only 0, false or off turn \
                     the GPU path off"
                );
            });
        }
    }
    hazard.map_or(Ok(()), |hazard| {
        Err(format!(
            "{hazard}; use a Khronos validation layer 1.4 or newer, or run without validation \
             layers (a release build, with VK_INSTANCE_LAYERS and VK_LOADER_LAYERS_ENABLE unset)"
        ))
    })
}

/// On unless `ALPHA2_YUV_TRANSCODE` is off, which exists to compare the two.
///
/// # Errors
/// Names the switch when it is off.
#[cfg(feature = "hwencode")]
pub fn configured_yuv_transcode() -> Result<(), String> {
    match env::var("ALPHA2_YUV_TRANSCODE").ok().as_deref() {
        Some("0" | "false" | "off") => Err("ALPHA2_YUV_TRANSCODE is off".to_owned()),
        _ => Ok(()),
    }
}

/// Whether frames may, must, or must not reach the encoder without a CPU readback. Every export
/// reports the path as `videoPipeline` and why the GPU path was not taken as
/// `gpuOutputPathUnavailable`.
#[derive(Copy, Clone, Debug, Default, serde::Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoInputPolicy {
    /// The GPU-resident path when device, codec, size and pinned encoder allow it; otherwise
    /// readback, with the reason.
    #[default]
    Auto,
    /// The GPU-resident path or an error naming what it lacks; nothing falls back silently.
    GpuRequired,
    CpuReadback,
}

impl VideoInputPolicy {
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
#[derive(Copy, Clone, Debug, Default, serde::Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExportStrategy {
    #[default]
    Render,
    /// Copy where the range is one source untouched and its packets decode into exactly the
    /// rendered frames; render otherwise, with the reason.
    PreferCopy,
    RequireCopy,
}

impl ExportStrategy {
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

/// Test hook: lets a test stop an export at a chosen frame through the ordinary cancel path and
/// check it leaves no output, partial, leased surface or Vulkan error.
pub fn configured_cancel_at_frame() -> Option<u64> {
    env::var("ALPHA2_EXPORT_CANCEL_AT_FRAME")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|frame| *frame > 0)
}

/// Test hook: fails a frame's readback as if the GPU refused to map it, since a real mapping
/// failure needs a lost or exhausted device.
///
/// With `ALPHA2_EXPORT_FAIL_ONCE_MARKER=<path>` only the export that atomically creates that file
/// fails, so a retry in a new process passes, as a transient fault would.
pub fn configured_fail_readback_at_frame() -> Option<u64> {
    injected_frame("ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME")
}

/// Test hook: fails a frame's readback as if the device dropped its work (a missing canary), so a
/// test can check that submission times decide whether the loss is reported hung. Honours
/// `ALPHA2_EXPORT_FAIL_ONCE_MARKER`.
pub fn configured_lose_device_at_frame() -> Option<u64> {
    injected_frame("ALPHA2_EXPORT_LOSE_DEVICE_AT_FRAME")
}

fn injected_frame(variable: &str) -> Option<u64> {
    let frame = env::var(variable)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())?;
    match env::var_os("ALPHA2_EXPORT_FAIL_ONCE_MARKER") {
        None => Some(frame),
        Some(marker) => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .ok()
            .map(|_| frame),
    }
}

/// Test instrumentation: see [`crate::capture`].
pub fn configured_frame_capture() -> Option<std::path::PathBuf> {
    env::var_os("ALPHA2_EXPORT_FRAMES_Y4M")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
}

/// Test instrumentation: an output video bitrate override in bits per second, so a codec
/// comparison can hold the bitrate fixed. It wins over the request, and the result says so as
/// `videoRateControl.source: "env-override"`.
pub fn configured_video_bit_rate() -> Option<u64> {
    env::var("ALPHA2_EXPORT_VIDEO_BITRATE")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RateSource {
    Request,
    EnvOverride,
}

impl RateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::EnvOverride => "env-override",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResolvedVideoRate {
    pub control: VideoRateControl,
    pub source: RateSource,
    /// What the request asked for, whatever won.
    pub requested: VideoQualityRequest,
}

impl ResolvedVideoRate {
    /// `applied` is `quantizer`, `quality` (the quantizer on the encoder's own scale) or
    /// `bitrate`; a quantizer the encoder refused falls back to bitrate, with the reason in
    /// `quantizerRefused`.
    pub fn to_json(
        self,
        selection: &alpha2_native::NativeVideoEncoderSelection,
    ) -> serde_json::Value {
        serde_json::json!({
            "bitrate": self.control.bitrate,
            "mode": self.control.mode.as_str(),
            "quantizer": self.control.quantizer,
            "source": self.source.as_str(),
            "level": match self.requested {
                VideoQualityRequest::Level(level) => Some(level.as_str()),
                VideoQualityRequest::Explicit(_) => None,
            },
            "applied": selection.rate_control.as_str(),
            "quantizerRefused": selection.quantizer_refused,
        })
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAudioRate {
    pub bitrate: u32,
    pub requested: AudioQualityRequest,
}

impl ResolvedAudioRate {
    pub const fn resolve(requested: AudioQualityRequest, codec: AudioCodec) -> Self {
        let bitrate = match requested {
            AudioQualityRequest::Explicit(bitrate) => bitrate,
            AudioQualityRequest::Level(level) => alpha2_media::resolve_audio_bitrate(codec, level),
        };
        Self { bitrate, requested }
    }

    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "bitrate": self.bitrate,
            "source": RateSource::Request.as_str(),
            "level": match self.requested {
                AudioQualityRequest::Level(level) => Some(level.as_str()),
                AudioQualityRequest::Explicit(_) => None,
            },
        })
    }
}

/// A level goes through the engine's one mapping, the same one the browser binding calls.
///
/// # Errors
/// Returns the engine's message for a zero output dimension.
pub fn resolve_video_rate_control(
    requested: VideoQualityRequest,
    codec: VideoCodec,
    width: u32,
    height: u32,
    frame_rate: FrameRate,
) -> Result<ResolvedVideoRate, String> {
    if let Some(bitrate) = configured_video_bit_rate() {
        return Ok(ResolvedVideoRate {
            control: VideoRateControl {
                bitrate,
                mode: BitrateMode::Variable,
                quantizer: None,
            },
            source: RateSource::EnvOverride,
            requested,
        });
    }
    let control = match requested {
        VideoQualityRequest::Explicit(rate) => rate,
        VideoQualityRequest::Level(level) => {
            alpha2_media::resolve_video_rate_control(codec, width, height, frame_rate, level)
                .map_err(|error| error.to_string())?
        }
    };
    Ok(ResolvedVideoRate {
        control,
        source: RateSource::Request,
        requested,
    })
}

/// One when the pipeline is off. Otherwise `ALPHA2_DECODE_CONCURRENCY` or the platform default,
/// clamped to the platform maximum and to `workload_parallelism`, past which shards would idle.
pub fn configured_decode_concurrency(
    pipeline_enabled: bool,
    hardware_decode: bool,
    workload_parallelism: usize,
) -> usize {
    if !pipeline_enabled {
        return 1;
    }
    let (default, maximum) = decode_concurrency_policy(hardware_decode);
    env::var("ALPHA2_DECODE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, maximum)
        .min(workload_parallelism.max(1))
}

const fn decode_concurrency_policy(hardware_decode: bool) -> (usize, usize) {
    #[cfg(target_os = "macos")]
    if hardware_decode {
        // Four VideoToolbox decode shards contend for sessions and surfaces; three saturate the
        // decoder.
        return (3, 3);
    }
    let _ = hardware_decode;
    (DEFAULT_DECODE_CONCURRENCY, MAX_DECODE_CONCURRENCY)
}

/// The peak number of decoder cursors reading at once
/// ([`alpha2_eval::ActiveIndex::peak_source_reads`]) and when each reads. Provider sources open no
/// decoder.
pub fn visual_decode_reads(
    project: &Project,
    export_range: TimeRange,
) -> (usize, crate::decode::CursorReads) {
    let decoded = project
        .sources
        .iter()
        .filter(|source| source.kind == MediaKind::Video && source.provider.is_none())
        .map(|source| &source.id)
        .collect::<std::collections::HashSet<_>>();
    // An export validated the project already; an index it cannot build reads nothing.
    let Ok(index) = alpha2_eval::ActiveIndex::build(project) else {
        return (1, crate::decode::CursorReads::default());
    };
    let counted = |source: &alpha2_media::SourceId| decoded.contains(source);
    (
        index.peak_source_reads(export_range, counted).max(1),
        crate::decode::CursorReads::new(index.source_reads(export_range, counted)),
    )
}

/// Chosen once when the host starts, because the renderer's device is created to match it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VideoDecodePolicy {
    Software,
    /// Use the resident GPU path where the stream and device support it.
    Auto,
    HardwareRequired,
}

impl Default for VideoDecodePolicy {
    /// `auto` on Linux and macOS, `software` on Windows, by measurement.
    ///
    /// `auto` is the default only where hardware decode matches software (within one code), never
    /// falls back, is no slower (at most 1.02x steady time) and gains (at most 0.8x host CPU or
    /// 0.95x steady time). On a 1080p30 H.264 export, Vulkan Video and `VideoToolbox` matched
    /// software speed at about half the CPU or less; D3D12VA on an integrated GPU was up to a
    /// quarter slower, failing the steady-time rule.
    fn default() -> Self {
        if cfg!(all(
            any(target_os = "linux", target_os = "macos"),
            feature = "hwdecode"
        )) {
            Self::Auto
        } else {
            Self::Software
        }
    }
}

impl VideoDecodePolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Auto => "auto",
            Self::HardwareRequired => "hardware-required",
        }
    }

    /// # Errors
    /// Returns a message naming the accepted values for anything else.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "software" => Ok(Self::Software),
            "auto" => Ok(Self::Auto),
            "hardware-required" => Ok(Self::HardwareRequired),
            other => Err(format!(
                "video decode mode must be software, auto, or hardware-required; got {other}"
            )),
        }
    }

    /// Reads `ALPHA2_VIDEO_DECODE`, defaulting to [`Self::default`].
    ///
    /// # Errors
    /// Returns a message when the variable holds an unknown value.
    pub fn from_env() -> Result<Self, String> {
        match env::var("ALPHA2_VIDEO_DECODE") {
            Ok(value) => Self::parse(&value),
            Err(_) => Ok(Self::default()),
        }
    }

    #[cfg_attr(
        not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )),
        allow(dead_code)
    )]
    #[must_use]
    pub const fn allows_hardware(self) -> bool {
        matches!(self, Self::Auto | Self::HardwareRequired)
    }

    #[must_use]
    pub const fn requires_hardware(self) -> bool {
        matches!(self, Self::HardwareRequired)
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum RendererPolicy {
    /// A hardware GPU when there is one, else the CPU renderer; never a software GPU adapter
    /// (lavapipe, llvmpipe, WARP, `SwiftShader`).
    #[default]
    Auto,
    /// The GPU renderer on whatever adapter is found, software ones included.
    Gpu,
    Cpu,
}

impl RendererPolicy {
    /// # Errors
    /// Returns a message naming the accepted values for anything else.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "gpu" => Ok(Self::Gpu),
            "cpu" => Ok(Self::Cpu),
            other => Err(format!("renderer must be auto, gpu, or cpu; got {other}")),
        }
    }

    /// Reads `ALPHA2_RENDERER`, defaulting to `auto`.
    ///
    /// # Errors
    /// Returns a message when the variable holds an unknown value.
    pub fn from_env() -> Result<Self, String> {
        match env::var("ALPHA2_RENDERER") {
            Ok(value) => Self::parse(&value),
            Err(_) => Ok(Self::default()),
        }
    }
}

/// `ALPHA2_RENDER_THREADS`, else every thread the process may run on (affinity and cgroup quota
/// counted).
pub fn configured_render_threads() -> usize {
    env::var("ALPHA2_RENDER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        })
}

/// Reverse playback holds a whole window at once, so by default a reverse cursor decodes in
/// software and leaves surfaces to forward cursors. Strict hardware runs default to a small window
/// instead of failing.
pub fn configured_reverse_hardware_surfaces(policy: VideoDecodePolicy) -> usize {
    let hardware_required = policy.requires_hardware();
    let default = if hardware_required {
        DEFAULT_REVERSE_HARDWARE_SURFACES
    } else {
        0
    };
    env::var("ALPHA2_REVERSE_DECODE_SURFACES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .min(MAX_REVERSE_HARDWARE_SURFACES)
}

/// From `ALPHA2_DECODE_SURFACES` and `ALPHA2_DECODE_SURFACE_BYTES`, read once per export.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
pub fn configured_decode_surface_limits() -> (u32, u64) {
    decode_surface_limits(
        env::var_os("ALPHA2_DECODE_SURFACES"),
        env::var_os("ALPHA2_DECODE_SURFACE_BYTES"),
    )
}

/// A ceiling of `0` would refuse every hardware frame, so like a zero byte budget it falls back to
/// the default, as does a non-number; each is named on stderr.
#[cfg(any(
    test,
    all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )
))]
fn decode_surface_limits(
    surfaces: Option<std::ffi::OsString>,
    bytes: Option<std::ffi::OsString>,
) -> (u32, u64) {
    (
        positive_setting(
            "ALPHA2_DECODE_SURFACES",
            surfaces,
            crate::DEFAULT_DECODE_SURFACES,
        ),
        positive_setting(
            "ALPHA2_DECODE_SURFACE_BYTES",
            bytes,
            crate::DEFAULT_DECODE_SURFACE_BYTES,
        ),
    )
}

#[cfg(any(
    test,
    all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )
))]
fn positive_setting<T>(variable: &str, setting: Option<std::ffi::OsString>, default: T) -> T
where
    T: std::str::FromStr + PartialOrd + Default + std::fmt::Display,
{
    let Some(setting) = setting else {
        return default;
    };
    if let Some(value) = setting
        .to_str()
        .and_then(|text| text.parse::<T>().ok())
        .filter(|value| *value > T::default())
    {
        return value;
    }
    alpha2_os::diagnostic!(
        "alpha2-host: ignoring {variable}={setting:?}: not a positive integer; \
         using the default {default}"
    );
    default
}

/// Clamped to `1..=MAX_DECODER_SESSIONS`; `None` when unset or not an integer (named on stderr).
pub fn configured_decoder_session_override() -> Option<usize> {
    let setting = env::var_os("ALPHA2_DECODER_SESSIONS")?;
    if let Some(sessions) = setting.to_str().and_then(|text| text.parse::<usize>().ok()) {
        return Some(sessions.clamp(1, MAX_DECODER_SESSIONS));
    }
    alpha2_os::diagnostic!(
        "alpha2-host: ignoring ALPHA2_DECODER_SESSIONS={setting:?}: not an integer; \
         sizing decoder sessions to the export"
    );
    None
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecoderSessionLimit {
    /// The most videos read at once, or the `DEFAULT_DECODER_SESSIONS` floor.
    Workload,
    /// `memory.decoderSessionBytes` over the estimate for one session of the largest video.
    Memory,
    Override,
    Cap,
}

impl DecoderSessionLimit {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workload => "workload",
            Self::Memory => "memory",
            Self::Override => "override",
            Self::Cap => "cap",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DecoderSessionCount {
    pub sessions: usize,
    pub limit: DecoderSessionLimit,
    /// ([`visual_decode_reads`]).
    pub peak: usize,
}

/// The lowest of the workload (`peak`, floored at `DEFAULT_DECODER_SESSIONS`), what `budget_bytes`
/// holds, and `MAX_DECODER_SESSIONS`; at least one. `requested` replaces all three. A tie names the
/// first of workload, memory, cap: the one a caller can change least.
pub fn decoder_session_count(
    peak: usize,
    session_bytes: u64,
    budget_bytes: u64,
    requested: Option<usize>,
) -> DecoderSessionCount {
    if let Some(sessions) = requested {
        return DecoderSessionCount {
            sessions: sessions.clamp(1, MAX_DECODER_SESSIONS),
            limit: DecoderSessionLimit::Override,
            peak,
        };
    }
    let fit = usize::try_from(budget_bytes / session_bytes.max(1))
        .unwrap_or(usize::MAX)
        .max(1);
    let (sessions, limit) = [
        (
            peak.max(DEFAULT_DECODER_SESSIONS),
            DecoderSessionLimit::Workload,
        ),
        (fit, DecoderSessionLimit::Memory),
        (MAX_DECODER_SESSIONS, DecoderSessionLimit::Cap),
    ]
    .into_iter()
    .min_by_key(|(sessions, _)| *sessions)
    .unwrap_or((DEFAULT_DECODER_SESSIONS, DecoderSessionLimit::Workload));
    DecoderSessionCount {
        sessions,
        limit,
        peak,
    }
}

/// Two pictures per frame thread (decoding, handed on) plus four (references, returned, decoded
/// ahead). Measured 8-bit H.264 sessions held 24 frames at 1080p and 4K at default threads against
/// 26 here, and four with one thread. 10-bit counts twice 8-bit through `frame_bytes`.
pub fn decoder_session_bytes(frame_bytes: u64, threads: i64) -> u64 {
    let threads = u64::try_from(threads.max(1)).unwrap_or(1);
    frame_bytes.saturating_mul(threads.saturating_mul(2).saturating_add(4))
}

/// Clamped to `1..=MAX_IN_FLIGHT_VIDEO_FRAMES`; always 1 on a single-core machine.
pub fn configured_frame_depth() -> usize {
    let configured = env::var("ALPHA2_EXPORT_IN_FLIGHT_FRAMES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_IN_FLIGHT_VIDEO_FRAMES)
        .clamp(1, MAX_IN_FLIGHT_VIDEO_FRAMES);
    if std::thread::available_parallelism().is_ok_and(|cores| cores.get() == 1) {
        1
    } else {
        configured
    }
}

/// `None` derives the depth from the in-flight byte budget. A single-core machine gets `Some(0)`,
/// a rendezvous channel. Rust floors a cgroup CPU quota, so one below two CPUs counts as one; the
/// host says so once per process.
pub fn configured_encode_channel_depth() -> Option<usize> {
    if std::thread::available_parallelism().is_ok_and(|cores| cores.get() == 1) {
        static NOTED: std::sync::Once = std::sync::Once::new();
        NOTED.call_once(|| {
            alpha2_os::diagnostic!(
                "alpha2-host: one CPU is available to this process; the encode channel is a \
                 rendezvous (depth 0)"
            );
        });
        return Some(0);
    }
    env::var("ALPHA2_ENCODE_QUEUE_DEPTH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.min(MAX_ENCODE_CHANNEL_DEPTH))
}

/// Off (`ALPHA2_EXPORT_PIPELINE` `0`, `false` or `off`) selects the synchronous comparison path.
pub fn configured_pipeline_enabled() -> bool {
    env::var("ALPHA2_EXPORT_PIPELINE").map_or(true, |value| {
        !matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "off")
    })
}

/// Clamped to `1..=MAX_DECODE_LOOKAHEAD`; one when the pipeline is off or on a single-core machine.
pub fn configured_decode_lookahead(pipeline_enabled: bool) -> usize {
    if !pipeline_enabled {
        return 1;
    }
    let configured = env::var("ALPHA2_DECODE_LOOKAHEAD")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_DECODE_LOOKAHEAD)
        .clamp(1, MAX_DECODE_LOOKAHEAD);
    if std::thread::available_parallelism().is_ok_and(|cores| cores.get() == 1) {
        1
    } else {
        configured
    }
}

/// An `ALPHA2_FFMPEG_THREADS` the budget ignored is named on stderr and reported as the default,
/// never as `explicit`.
pub fn codec_thread_policy(video_encoder_hardware: bool) -> &'static str {
    let setting = alpha2_native::configured_codec_thread_override();
    if let Err(ignored) = &setting
        && !video_encoder_hardware
    {
        alpha2_os::diagnostic!(
            "alpha2-host: ignoring ALPHA2_FFMPEG_THREADS={ignored:?}: not a positive worker count; \
             using the default budget"
        );
    }
    codec_thread_policy_for(video_encoder_hardware, &setting)
}

pub fn codec_thread_policy_for(
    video_encoder_hardware: bool,
    setting: &Result<Option<usize>, String>,
) -> &'static str {
    if video_encoder_hardware {
        "codec-default"
    } else if matches!(setting, Ok(Some(_))) {
        "explicit"
    } else {
        // The encoder takes what is left after decode is sized to its demand, not an even share.
        "output-weighted"
    }
}

pub fn decoder_thread_policy(hardware_decode: bool, software_video_encoder: bool) -> &'static str {
    if hardware_decode {
        // A device-resident session decodes nothing on the CPU, so it opens with a fixed count; a
        // fallback from it takes the even share. On the pinned FFmpeg the count sizes no surface
        // pool.
        "hardware-fixed"
    } else if software_video_encoder {
        "source-area"
    } else {
        "even-share"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_ignored_thread_setting_is_not_reported_as_explicit() {
        use std::ffi::OsStr;
        let policy = |setting: Option<&str>| {
            super::codec_thread_policy_for(
                false,
                &alpha2_native::codec_thread_override(setting.map(OsStr::new)),
            )
        };
        assert_eq!(policy(Some("6")), "explicit");
        assert_eq!(policy(None), "output-weighted");
        for ignored in ["0", "abc", "-2", ""] {
            assert_eq!(policy(Some(ignored)), "output-weighted", "{ignored:?}");
        }
        assert_eq!(
            alpha2_native::codec_thread_override(Some(OsStr::new("500"))),
            Ok(Some(64))
        );
        assert_eq!(
            super::codec_thread_policy_for(true, &Ok(Some(6))),
            "codec-default"
        );
    }

    #[test]
    fn a_decoder_surface_ceiling_of_zero_or_not_a_number_is_the_default() {
        use std::ffi::OsString;
        let defaults = (
            crate::DEFAULT_DECODE_SURFACES,
            crate::DEFAULT_DECODE_SURFACE_BYTES,
        );
        let limits = |surfaces: Option<&str>, bytes: Option<&str>| {
            super::decode_surface_limits(surfaces.map(OsString::from), bytes.map(OsString::from))
        };
        assert_eq!(limits(None, None), defaults);
        for ignored in ["0", "-1", "", "lots", "512MiB", "4294967296"] {
            assert_eq!(
                limits(Some(ignored), None).0,
                defaults.0,
                "ALPHA2_DECODE_SURFACES={ignored:?}"
            );
        }
        for ignored in ["0", "-1", "", "lots", "512MiB", "18446744073709551616"] {
            assert_eq!(
                limits(None, Some(ignored)).1,
                defaults.1,
                "ALPHA2_DECODE_SURFACE_BYTES={ignored:?}"
            );
        }
        assert_eq!(limits(Some("0"), Some("0")), defaults);
        assert_eq!(limits(Some("4"), Some("50000000")), (4, 50_000_000));
        // A 4K NV12 surface fits the budget the zeros leave.
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        {
            let (surfaces, bytes) = limits(Some("0"), Some("0"));
            alpha2_native_gpu::SurfaceBudget::new(surfaces, bytes)
                .try_reserve(3840 * 2160 * 3 / 2)
                .expect("a 4K NV12 surface");
        }
    }

    #[cfg(feature = "hwencode")]
    #[test]
    fn a_video_session_hazard_cannot_be_overridden() {
        let hazard = || Some("the loaded Vulkan validation layer is 1.3.204".to_owned());
        assert_eq!(super::gpu_encode_gate(None, None), Ok(()));
        assert_eq!(super::gpu_encode_gate(Some("1"), None), Ok(()));
        let refused = super::gpu_encode_gate(None, hazard()).expect_err("1.3.204");
        assert!(refused.contains("1.3.204"), "{refused}");
        assert!(refused.contains("1.4 or newer"), "{refused}");
        assert!(!refused.contains("ALPHA2_GPU_ENCODE=1"), "{refused}");
        assert_eq!(super::gpu_encode_gate(Some("1"), hazard()), Err(refused));
        assert_eq!(
            super::gpu_encode_gate(Some("off"), None),
            Err("ALPHA2_GPU_ENCODE is off".to_owned())
        );
    }

    use super::visual_decode_reads;
    use alpha2_time::{MediaTime, TimeRange};

    fn range(start: i64, end: i64) -> TimeRange {
        TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)).unwrap()
    }

    #[test]
    fn reverse_hardware_surfaces_default_to_software_unless_strict() {
        use super::VideoDecodePolicy;

        assert_eq!(
            super::configured_reverse_hardware_surfaces(VideoDecodePolicy::Auto),
            0
        );
        assert_eq!(
            super::configured_reverse_hardware_surfaces(VideoDecodePolicy::HardwareRequired),
            crate::DEFAULT_REVERSE_HARDWARE_SURFACES
        );
    }

    #[test]
    fn video_decode_modes_round_trip_and_reject_unknown_values() {
        use super::VideoDecodePolicy;

        for policy in [
            VideoDecodePolicy::Software,
            VideoDecodePolicy::Auto,
            VideoDecodePolicy::HardwareRequired,
        ] {
            assert_eq!(VideoDecodePolicy::parse(policy.as_str()), Ok(policy));
        }
        assert!(VideoDecodePolicy::parse("gpu").is_err());
        assert!(!VideoDecodePolicy::Software.allows_hardware());
        assert!(VideoDecodePolicy::Auto.allows_hardware());
        assert!(!VideoDecodePolicy::Auto.requires_hardware());
        assert!(VideoDecodePolicy::HardwareRequired.requires_hardware());
    }

    #[test]
    fn the_decode_default_is_auto_where_measured() {
        use super::VideoDecodePolicy;

        let expected = if cfg!(all(
            any(target_os = "linux", target_os = "macos"),
            feature = "hwdecode"
        )) {
            VideoDecodePolicy::Auto
        } else {
            VideoDecodePolicy::Software
        };
        assert_eq!(VideoDecodePolicy::default(), expected);
        // A device without a decode path must fall back, not fail the export.
        assert!(!VideoDecodePolicy::default().requires_hardware());
    }

    #[test]
    fn decode_parallelism_counts_what_compositions_read() {
        use serde_json::json;
        let span = |start, end| json!({"start": start, "end": end});
        let video = |id: &str| json!({"id": id, "kind": "video", "locator": format!("{id}.mp4")});
        let media = |id: &str| json!({"id": id, "op": {"type": "media", "source_id": id}});
        let quad = json!({"id": "quad", "nodes": [
            media("a"), media("b"), media("c"), media("d"),
            {"id": "mix", "op": {"type": "merge", "layers": [
                {"input": "a"}, {"input": "b"}, {"input": "c"}, {"input": "d"}
            ]}}
        ], "output": "mix"});
        let project = |clip: serde_json::Value| {
            alpha2_model::Project::from_json(
                &json!({
                    "schema_version": 1, "id": "p", "name": "p",
                    "settings": {"width": 64, "height": 36, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
                    "sources": [video("a"), video("b"), video("c"), video("d"),
                        {"id": "split", "kind": "composition", "composition": "quad"}],
                    "compositions": [quad],
                    "timeline": {"tracks": [{"id": "t", "name": "V", "kind": "video", "clips": [clip]}]}
                })
                .to_string(),
            )
            .expect("valid")
        };
        let composed = project(json!({
            "id": "c", "source_id": "a", "composition": {"id": "quad"},
            "timeline_range": span(0, 120_000), "source_range": span(0, 120_000)
        }));
        assert_eq!(visual_decode_reads(&composed, range(0, 120_000)).0, 4);
        let source = project(json!({
            "id": "c", "source_id": "split",
            "timeline_range": span(0, 120_000), "source_range": span(0, 120_000)
        }));
        assert_eq!(visual_decode_reads(&source, range(0, 120_000)).0, 4);
    }

    #[test]
    fn a_decoder_session_is_estimated_from_its_frame_bytes() {
        use super::decoder_session_bytes;

        let hd = 1920 * 1080 * 3 / 2;
        // 1080p 8-bit 4:2:0 at eleven threads (half of 22): 26 frames, against the 24 measured.
        assert_eq!(decoder_session_bytes(hd, 11), 3_110_400 * 26);
        assert_eq!(decoder_session_bytes(hd, 1), 3_110_400 * 6);
        let (uhd_8, uhd_10) = (3840 * 2160 * 3 / 2, 3840 * 2160 * 3);
        assert_eq!(
            decoder_session_bytes(uhd_10, 11),
            2 * decoder_session_bytes(uhd_8, 11)
        );
        assert_eq!(decoder_session_bytes(uhd_10, 11), 24_883_200 * 26);
        assert_eq!(decoder_session_bytes(hd, 0), decoder_session_bytes(hd, 1));
        assert_eq!(decoder_session_bytes(0, 11), 0);
    }

    #[test]
    fn decoder_sessions_are_the_lowest_of_the_workload_the_memory_and_the_cap() {
        use super::{DecoderSessionLimit as Limit, decoder_session_count};
        use crate::{DEFAULT_DECODER_SESSIONS, MAX_DECODER_SESSIONS};

        let mb = 1 << 20;
        let count = |peak, session, budget, requested| {
            let count = decoder_session_count(peak, session, budget, requested);
            assert_eq!(count.peak, peak);
            (count.sessions, count.limit)
        };
        assert_eq!(count(12, 80 * mb, 4096 * mb, None), (12, Limit::Workload));
        assert_eq!(
            count(2, 80 * mb, 4096 * mb, None),
            (DEFAULT_DECODER_SESSIONS, Limit::Workload)
        );
        assert_eq!(count(16, 320 * mb, 4096 * mb, None), (12, Limit::Memory));
        assert_eq!(count(16, 320 * mb, 100 * mb, None), (1, Limit::Memory));
        assert_eq!(count(16, 0, 4096 * mb, None), (16, Limit::Workload));
        assert_eq!(
            count(40, 10 * mb, 4096 * mb, None),
            (MAX_DECODER_SESSIONS, Limit::Cap)
        );
        // A tie names the workload, then the memory.
        assert_eq!(count(12, 100 * mb, 1200 * mb, None), (12, Limit::Workload));
        assert_eq!(count(40, 128 * mb, 4096 * mb, None), (32, Limit::Memory));
        assert_eq!(
            count(40, 320 * mb, 100 * mb, Some(20)),
            (20, Limit::Override)
        );
        assert_eq!(count(2, 1, 1, Some(0)), (1, Limit::Override));
        assert_eq!(
            count(2, 1, 1, Some(500)),
            (MAX_DECODER_SESSIONS, Limit::Override)
        );
    }
}
