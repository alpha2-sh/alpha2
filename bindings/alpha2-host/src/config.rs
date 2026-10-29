//! Environment-tunable pipeline knobs.

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

/// How long any wait on GPU work may take before it fails with
/// `GPU_TIMEOUT`, from `ALPHA2_GPU_TIMEOUT_MS` (default 30 s).
///
/// The default allows slow GPUs to render large frames without false timeouts.
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

/// How long a decode shard or the encode worker may stay inside one `FFmpeg`
/// operation before the export fails with `DECODE_STALLED` or
/// `ENCODE_STALLED`.
///
/// A decoder operation spends at most [`alpha2_native::DECODE_WORK_LIMIT`]
/// between the frames it decodes, each of which it reports as progress
/// ([`alpha2_native::watch_decode_progress`]), an encoder or muxer call encodes or
/// writes one frame or packet, and none of this host's own waits on the GPU
/// outlasts [`configured_gpu_wait_timeout`]. One that has run longer than
/// both together is blocked inside `FFmpeg`, which cannot be interrupted:
/// Vulkan decode and encode wait on their execution fences with no timeout,
/// and a write to a hard-mounted share that stopped answering is retried
/// forever.
pub fn configured_stall_timeout() -> std::time::Duration {
    alpha2_native::DECODE_WORK_LIMIT.saturating_add(configured_gpu_wait_timeout())
}

/// Whether an H.264 export may keep its output on the GPU, and if not, why.
///
/// On by default where the device supports it. `0`, `false` or `off` forces
/// the readback path, which is how a run compares the two or steps around a
/// driver problem without rebuilding; `1`, `true` or `on` forces it on.
#[cfg(all(target_os = "linux", feature = "hwencode"))]
pub fn configured_gpu_encode(video_session_hazard: Option<String>) -> Result<(), String> {
    gpu_encode_gate(
        env::var("ALPHA2_GPU_ENCODE").ok().as_deref(),
        video_session_hazard,
    )
}

/// `video_session_hazard` is why `FFmpeg` must not open a video session on
/// the device ([`alpha2_native_gpu::SharedVulkanContext::video_session_hazard`]),
/// which `1` overrides.
#[cfg(all(target_os = "linux", feature = "hwencode"))]
fn gpu_encode_gate(
    setting: Option<&str>,
    video_session_hazard: Option<String>,
) -> Result<(), String> {
    match setting {
        Some("0" | "false" | "off") => Err("ALPHA2_GPU_ENCODE is off".to_owned()),
        Some("1" | "true" | "on") => Ok(()),
        _ => video_session_hazard.map_or(Ok(()), |hazard| {
            Err(format!(
                "{hazard}; use a 1.4 layer or set ALPHA2_GPU_ENCODE=1"
            ))
        }),
    }
}

/// The `ALPHA2_GPU_ENCODE` switch alone, for a platform with no validation
/// layer known to crash its encoder.
#[cfg(all(not(target_os = "linux"), feature = "hwencode"))]
pub fn configured_gpu_encode_switch() -> Result<(), String> {
    match env::var("ALPHA2_GPU_ENCODE").ok().as_deref() {
        Some("0" | "false" | "off") => Err("ALPHA2_GPU_ENCODE is off".to_owned()),
        _ => Ok(()),
    }
}

/// Whether an eligible GPU-resident export may hand decoded pictures to the
/// encoder unchanged instead of rendering them. On unless
/// `ALPHA2_YUV_TRANSCODE` is off, which exists to compare the two.
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

/// How an export's encoder receives its frames.
///
/// The codec and a pinned encoder say *what* encodes; this says whether the
/// frames may, must, or must not reach it without a CPU readback. Every
/// export reports the path it took as `videoPipeline`, and why the GPU
/// path was not taken as `gpuOutputPathUnavailable`.
#[derive(Copy, Clone, Debug, Default, serde::Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoInputPolicy {
    /// The GPU-resident path when the device, codec, size, and pinned
    /// encoder allow it; the readback path otherwise, with the reason.
    #[default]
    Auto,
    /// The GPU-resident path or an error naming what it lacks. Nothing falls
    /// back silently.
    GpuRequired,
    /// The readback path, for comparison and diagnosis.
    CpuReadback,
}

impl VideoInputPolicy {
    /// Stable identifier used by the protocol and the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::GpuRequired => "gpu-required",
            Self::CpuReadback => "cpu-readback",
        }
    }

    /// Parses the CLI spelling, which is the protocol's.
    ///
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

/// Whether an export may copy its source's compressed packets instead of
/// rendering and encoding.
///
/// Copying keeps the source's own encoding (its bitrate, profile, and GOP),
/// so it answers a different request than a render at a quality level, and is
/// only ever taken when asked for.
#[derive(Copy, Clone, Debug, Default, serde::Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExportStrategy {
    /// Render and encode every frame.
    #[default]
    Render,
    /// Copy where the range is one source untouched and its packets decode
    /// into exactly the rendered frames; render otherwise, with the reason.
    PreferCopy,
    /// Copy, or fail with the reason it cannot.
    RequireCopy,
}

impl ExportStrategy {
    /// Stable identifier used by the protocol and the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::PreferCopy => "prefer-copy",
            Self::RequireCopy => "require-copy",
        }
    }

    /// Parses the CLI spelling, which is the protocol's.
    ///
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

/// Output frame after which an export cancels itself.
///
/// A termination request cancels an export too (`crate::termination`), but
/// at whatever frame the signal lands on. Every other way an export stops
/// early (a codec failure, a missing source) is a failure of the thing that
/// stopped it, so none of them exercises the ordinary cancel path either.
/// This is what lets a test stop an export in flight at a chosen frame and
/// then assert on what it left behind: no output, no partial, no leased
/// decoder surface, and no Vulkan error on the way out.
pub fn configured_cancel_at_frame() -> Option<u64> {
    env::var("ALPHA2_EXPORT_CANCEL_AT_FRAME")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|frame| *frame > 0)
}

/// Output frame whose readback an export fails as if the GPU had refused to
/// map it.
///
/// A real mapping failure needs a lost or exhausted device, which no test can
/// produce on demand. This is what lets one check, end to end, that such a
/// failure reaches the caller as `READBACK_FAILED` at the frame it happened,
/// with the device evidence (none) beside it, and that a retry starts clean.
///
/// With `ALPHA2_EXPORT_FAIL_ONCE_MARKER=<path>` it fails only the export that
/// creates that file, atomically, so a retry in a new host process passes:
/// what a transient fault looks like to a retry policy.
pub fn configured_fail_readback_at_frame() -> Option<u64> {
    let frame = env::var("ALPHA2_EXPORT_FAIL_READBACK_AT_FRAME")
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

/// Where an export also writes the frames it encodes, as YUV4MPEG2. Test
/// instrumentation: see [`crate::capture`].
pub fn configured_frame_capture() -> Option<std::path::PathBuf> {
    env::var_os("ALPHA2_EXPORT_FRAMES_Y4M")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
}

/// Output video bitrate override in bits per second. Test instrumentation.
///
/// It exists so a quality comparison between two codecs can hold the bitrate
/// fixed and raise it for a reference encode, which is the only way to say
/// whether a codec change improved the picture rather than only the wall
/// clock. When set it wins over the request, and the export result says so as
/// `videoRateControl.source: "env-override"`, so a run meant to measure a
/// default that was actually measuring this is a visible mistake.
pub fn configured_video_bit_rate() -> Option<u64> {
    env::var("ALPHA2_EXPORT_VIDEO_BITRATE")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

/// Where an export's video rate came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RateSource {
    /// The request's `videoQuality`, resolved by the engine.
    Request,
    /// `ALPHA2_EXPORT_VIDEO_BITRATE`, which overrode the request.
    EnvOverride,
}

impl RateSource {
    /// Stable identifier used in the export result's `videoRateControl.source`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::EnvOverride => "env-override",
        }
    }
}

/// The rate an export encodes video at, and what decided it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResolvedVideoRate {
    pub control: VideoRateControl,
    pub source: RateSource,
    /// What the request asked for, whatever won.
    pub requested: VideoQualityRequest,
}

impl ResolvedVideoRate {
    /// The export result's `videoRateControl`, with what the encoder ran.
    ///
    /// `applied` is `quantizer` or `bitrate`; a quantizer the encoder refused
    /// falls back to the bitrate silently to the caller and visibly here, with
    /// the reason in `quantizerRefused`.
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

/// The bitrate an export encodes audio at, and what decided it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAudioRate {
    pub bitrate: u32,
    pub requested: AudioQualityRequest,
}

impl ResolvedAudioRate {
    /// Resolves through the engine's table; the sinks receive the number.
    pub const fn resolve(requested: AudioQualityRequest, codec: AudioCodec) -> Self {
        let bitrate = match requested {
            AudioQualityRequest::Explicit(bitrate) => bitrate,
            AudioQualityRequest::Level(level) => alpha2_media::resolve_audio_bitrate(codec, level),
        };
        Self { bitrate, requested }
    }

    /// The export result's `audioRateControl`.
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

/// Resolves the rate an export encodes video at, before any sink opens.
///
/// A level goes through the engine's one mapping, the same function the
/// browser binding calls, so both sinks mean the same thing by it.
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

/// Decode shards an export runs at once, from `ALPHA2_DECODE_CONCURRENCY`.
///
/// One when the pipeline is off. Otherwise the setting or the platform default, clamped to
/// the platform maximum and to `workload_parallelism`, since shards past the most sources read
/// at once would sit idle.
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
        // Four simultaneous VideoToolbox decode shards contend for sessions and surfaces; three
        // saturated the tested decoder.
        return (3, 3);
    }
    let _ = hardware_decode;
    (DEFAULT_DECODE_CONCURRENCY, MAX_DECODE_CONCURRENCY)
}

/// The most video decoder cursors reading at once over `export_range`: each
/// clip's own source and each media step of its compositions, as
/// [`alpha2_eval::ActiveIndex::peak_source_reads`] counts them; and when each
/// of those cursors reads ([`alpha2_eval::ActiveIndex::source_reads`]), from
/// one index. A source an extension provider draws opens no decoder.
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

/// How a run may decode video.
///
/// The value is chosen once when the host starts, because the renderer's
/// device is created to match it, and is reported with every export so a
/// measurement records which path it exercised.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VideoDecodePolicy {
    /// Decode every video frame on the CPU.
    Software,
    /// Use the resident GPU path where the stream and device support it.
    Auto,
    /// Fail rather than decode a video frame in software.
    HardwareRequired,
}

impl Default for VideoDecodePolicy {
    /// Linux defaults to the resident GPU path; other platforms stay on the CPU.
    ///
    /// The rule for every platform: the default is `auto` only where measured
    /// exports show a gain, and `software` everywhere else until they do.
    ///
    /// On Linux the shared Vulkan device measured no regression across encoder
    /// configurations and a large gain wherever the encoder competes for the
    /// CPU, and a device that cannot decode falls back with its reason
    /// reported. macOS and Windows have not shown that gain, so enabling them
    /// by default would only add an interop path to every export.
    fn default() -> Self {
        if cfg!(all(target_os = "linux", feature = "hwdecode")) {
            Self::Auto
        } else {
            Self::Software
        }
    }
}

impl VideoDecodePolicy {
    /// Stable identifier used by the CLI, the protocol, and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Auto => "auto",
            Self::HardwareRequired => "hardware-required",
        }
    }

    /// Parses one of the three accepted identifiers.
    ///
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

    /// Reads `ALPHA2_VIDEO_DECODE`, defaulting to [`Self::default`]: `auto`
    /// on Linux with hardware decode built in, `software` elsewhere.
    ///
    /// # Errors
    /// Returns a message when the variable holds an unknown value.
    pub fn from_env() -> Result<Self, String> {
        match env::var("ALPHA2_VIDEO_DECODE") {
            Ok(value) => Self::parse(&value),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Whether the GPU decode path may be used at all.
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

    /// Whether a software video frame is a failure.
    #[must_use]
    pub const fn requires_hardware(self) -> bool {
        matches!(self, Self::HardwareRequired)
    }
}

/// Which renderer a host draws with, chosen once when it starts.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum RendererPolicy {
    /// A hardware GPU when there is one, else the CPU renderer; never a
    /// software GPU adapter (lavapipe, llvmpipe, WARP, `SwiftShader`).
    #[default]
    Auto,
    /// The GPU renderer on whatever adapter is found, software ones
    /// included.
    Gpu,
    /// The CPU renderer, opening no graphics driver.
    Cpu,
}

impl RendererPolicy {
    /// Parses one of the three accepted identifiers.
    ///
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

/// The CPU renderer's threads: `ALPHA2_RENDER_THREADS`, else every thread
/// the process may run on (its affinity and cgroup quota counted).
pub fn configured_render_threads() -> usize {
    env::var("ALPHA2_RENDER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        })
}

/// Decoder surfaces one reverse window may retain before it uses software.
///
/// Reverse playback holds every frame of a window at once, so the default is
/// zero: a reverse cursor decodes in software and keeps decoder surfaces
/// available to the forward cursors. Strict hardware runs default to a small
/// window instead of failing outright, and either can be set explicitly.
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

/// The decoder-surface budget's ceilings, surfaces and bytes, from
/// `ALPHA2_DECODE_SURFACES` and `ALPHA2_DECODE_SURFACE_BYTES`.
///
/// Read once per export, for every platform's hardware import.
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

/// [`configured_decode_surface_limits`] from the two settings.
///
/// Either ceiling at `0` would refuse every hardware frame, and the export
/// would then fail, or under `auto` download every frame: like a byte budget
/// of `0` everywhere else, it falls back to the default, and so does a value
/// that is not a number, each named on stderr.
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

/// `setting` when it is a positive integer, else `default`, naming on stderr
/// a setting that was ignored.
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

/// The decoder sessions `ALPHA2_DECODER_SESSIONS` asks for, clamped to
/// `1..=MAX_DECODER_SESSIONS`; `None` when it is unset, or not an integer
/// (named on stderr).
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

/// What decided how many decoder sessions an export keeps open.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecoderSessionLimit {
    /// The most videos the export reads at once, or the floor of
    /// `DEFAULT_DECODER_SESSIONS` above it.
    Workload,
    /// The decoder session budget (`memory.decoderSessionBytes`) over the
    /// estimate for one session of the largest video.
    Memory,
    /// `ALPHA2_DECODER_SESSIONS`.
    Override,
    /// `MAX_DECODER_SESSIONS`.
    Cap,
}

impl DecoderSessionLimit {
    /// The name results and diagnostics use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workload => "workload",
            Self::Memory => "memory",
            Self::Override => "override",
            Self::Cap => "cap",
        }
    }
}

/// How many decoder sessions an export keeps open, and why.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DecoderSessionCount {
    pub sessions: usize,
    pub limit: DecoderSessionLimit,
    /// The most video decoder cursors the export reads at once
    /// ([`visual_decode_reads`]).
    pub peak: usize,
}

/// The decoder sessions of an export that reads `peak` videos at once: the
/// lowest of the workload (`peak`, at least `DEFAULT_DECODER_SESSIONS`, so a
/// project that reads few never runs short), what `budget_bytes` holds of
/// sessions of `session_bytes` each, and `MAX_DECODER_SESSIONS`; at least one.
/// `requested` (`ALPHA2_DECODER_SESSIONS`) replaces all three. A tie names
/// the first of workload, memory, cap: the one a caller can change least.
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

/// What one decoder session of a video of `luma_samples` per frame is
/// estimated to hold: 8-bit 4:2:0 frames, as many as `threads` frame
/// threads keep (two each: the one it decodes and the one it hands on) plus
/// four, for the references, the frame returned and the one decoded ahead.
/// Measured with H.264 at the default threads, software sessions held 24
/// frames at 1080p and at 4K, against 26 here; one with a single thread held
/// four.
pub fn decoder_session_bytes(luma_samples: u64, threads: i64) -> u64 {
    let frame = luma_samples.saturating_mul(3).div_ceil(2);
    let threads = u64::try_from(threads.max(1)).unwrap_or(1);
    frame.saturating_mul(threads.saturating_mul(2).saturating_add(4))
}

/// Video frames in flight between render and encode, from `ALPHA2_EXPORT_IN_FLIGHT_FRAMES`,
/// clamped to `1..=MAX_IN_FLIGHT_VIDEO_FRAMES`. Always 1 on a single-core machine, where
/// nothing could overlap.
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

/// Encode channel depth the caller asked for, or `None` to derive it.
///
/// By default, the in-flight byte budget determines depth: each slot holds
/// one packed frame, so smaller outputs can use deeper channels. A
/// single-core machine always gets `Some(0)`, a rendezvous channel.
pub fn configured_encode_channel_depth() -> Option<usize> {
    if std::thread::available_parallelism().is_ok_and(|cores| cores.get() == 1) {
        return Some(0);
    }
    env::var("ALPHA2_ENCODE_QUEUE_DEPTH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.min(MAX_ENCODE_CHANNEL_DEPTH))
}

/// Whether exports run the pipelined path. On unless `ALPHA2_EXPORT_PIPELINE` is `0`,
/// `false`, or `off`; off selects the synchronous comparison path.
pub fn configured_pipeline_enabled() -> bool {
    env::var("ALPHA2_EXPORT_PIPELINE").map_or(true, |value| {
        !matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "off")
    })
}

/// Frame sets the decoder may run ahead of the renderer, from `ALPHA2_DECODE_LOOKAHEAD`,
/// clamped to `1..=MAX_DECODE_LOOKAHEAD`. One when the pipeline is off or on a single-core
/// machine.
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

/// How the software video encoder's worker count was arrived at.
///
/// An `ALPHA2_FFMPEG_THREADS` the budget ignored (`0`, a word) is named on
/// stderr and leaves the default policy, as an ignored byte budget is
/// reported as the default, so the report never says `explicit` while the
/// default ran.
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

/// [`codec_thread_policy`] for a parsed `ALPHA2_FFMPEG_THREADS` setting.
pub fn codec_thread_policy_for(
    video_encoder_hardware: bool,
    setting: &Result<Option<usize>, String>,
) -> &'static str {
    if video_encoder_hardware {
        // A hardware encoder ignores the software budget entirely.
        "codec-default"
    } else if matches!(setting, Ok(Some(_))) {
        "explicit"
    } else {
        // The encoder takes the budget left after decode is sized to its own demand, rather than an
        // even share of it.
        "output-weighted"
    }
}

/// How each decode session's worker count was arrived at.
pub fn decoder_thread_policy(hardware_decode: bool, software_video_encoder: bool) -> &'static str {
    if hardware_decode {
        // A device-resident session decodes nothing on the CPU, so it opens with a fixed count
        // (`HARDWARE_DECODER_THREADS`); a fallback from it takes the even share. On the pinned
        // FFmpeg the count sizes no surface pool: Vulkan, D3D12 and VideoToolbox set no fixed
        // initial pool size for frame threads to grow.
        "hardware-fixed"
    } else if software_video_encoder {
        "source-area"
    } else {
        "even-share"
    }
}

#[cfg(test)]
mod tests {
    /// An `ALPHA2_FFMPEG_THREADS` the budget ignored for its default (`0`, a
    /// word) is not reported as an explicit worker count.
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

    /// A decoder-surface ceiling of `0` is the default, as a byte budget of
    /// `0` is everywhere else, and so is one that is not a positive integer,
    /// since either would refuse every hardware frame.
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

    #[cfg(all(target_os = "linux", feature = "hwencode"))]
    #[test]
    fn only_a_video_session_hazard_keeps_exports_off_the_gpu() {
        let hazard = || Some("the loaded Vulkan validation layer is 1.3.204".to_owned());
        assert_eq!(super::gpu_encode_gate(None, None), Ok(()));
        let refused = super::gpu_encode_gate(None, hazard()).expect_err("1.3.204");
        assert!(refused.contains("1.3.204"), "{refused}");
        assert!(refused.contains("ALPHA2_GPU_ENCODE=1"), "{refused}");
        assert_eq!(super::gpu_encode_gate(Some("1"), hazard()), Ok(()));
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

        // Reverse playback must not hold decoder surfaces unless a run asked for it, and a strict
        // run must still have a window it can use.
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
    fn the_default_mode_follows_the_platform() {
        use super::VideoDecodePolicy;

        let expected = if cfg!(all(target_os = "linux", feature = "hwdecode")) {
            VideoDecodePolicy::Auto
        } else {
            VideoDecodePolicy::Software
        };
        assert_eq!(VideoDecodePolicy::default(), expected);
        // An unset variable must not be stricter than the default: a device without a decode path
        // falls back, it does not fail the export.
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
    fn a_decoder_session_is_estimated_as_its_frames_and_two_per_thread() {
        use super::decoder_session_bytes;

        let hd = 1920 * 1080;
        // 1080p at eleven threads, the default split of this machine's 22: 26 frames, against the
        // 24 measured.
        assert_eq!(decoder_session_bytes(hd, 11), 3_110_400 * 26);
        assert_eq!(decoder_session_bytes(hd, 1), 3_110_400 * 6);
        // No thread count below one, and no video at all costs nothing.
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
        // The videos read at once, never fewer than the floor.
        assert_eq!(count(12, 80 * mb, 4096 * mb, None), (12, Limit::Workload));
        assert_eq!(
            count(2, 80 * mb, 4096 * mb, None),
            (DEFAULT_DECODER_SESSIONS, Limit::Workload)
        );
        // What the budget holds, at least one.
        assert_eq!(count(16, 320 * mb, 4096 * mb, None), (12, Limit::Memory));
        assert_eq!(count(16, 320 * mb, 100 * mb, None), (1, Limit::Memory));
        // No video to estimate fits any number.
        assert_eq!(count(16, 0, 4096 * mb, None), (16, Limit::Workload));
        // The cap.
        assert_eq!(
            count(40, 10 * mb, 4096 * mb, None),
            (MAX_DECODER_SESSIONS, Limit::Cap)
        );
        // A tie names the workload, then the memory.
        assert_eq!(count(12, 100 * mb, 1200 * mb, None), (12, Limit::Workload));
        assert_eq!(count(40, 128 * mb, 4096 * mb, None), (32, Limit::Memory));
        // The setting replaces all three, within one and the cap.
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
