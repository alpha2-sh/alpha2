//! Codec capability discovery and encoder selection.

use std::ffi::CString;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

use crate::convert::{encode_adapter_error, reject_unused_encode_options, video_encoder_options};
use crate::video_encode::VideoEncodeConfig;
use alpha2_ffmpeg_bridge::{EncoderContext, LogLine};
pub use alpha2_host_core::codecs::{NativeAudioCodec, NativeVideoCodec};
pub(crate) use alpha2_host_core::encode_policy::keyframe_interval;
pub(crate) use alpha2_host_core::probe_cache::{ProbeCache, cached_probe, mode_label};
use alpha2_media::{ColorMetadata, EncodeError, EncoderRefusal, OpenFailureKind};
use rsmpeg::avcodec::{AVCodec, AVCodecContext, AVCodecRef};
use rsmpeg::avutil::AVRational;
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

/// Configuration for transactional audio/video export.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AvEncodeConfig {
    /// Video encoder settings.
    pub video: VideoEncodeConfig,
    /// Project audio sample rate.
    pub audio_sample_rate: u32,
    /// Audio channel count.
    pub audio_channels: u16,
    /// Audio codec selected by the downstream host.
    pub audio_codec: NativeAudioCodec,
    /// Audio bitrate in bits per second.
    pub audio_bit_rate: u32,
}

/// `FFmpeg` identities of the host-core codec enums.
pub(crate) trait FfmpegCodec {
    /// The `FFmpeg` codec ID.
    fn codec_id(self) -> ffi::AVCodecID;
}

impl FfmpegCodec for NativeVideoCodec {
    fn codec_id(self) -> ffi::AVCodecID {
        match self {
            Self::H264 => ffi::AV_CODEC_ID_H264,
            Self::Hevc => ffi::AV_CODEC_ID_HEVC,
            Self::Vp9 => ffi::AV_CODEC_ID_VP9,
            Self::Av1 => ffi::AV_CODEC_ID_AV1,
            Self::Mpeg4Part2 => ffi::AV_CODEC_ID_MPEG4,
            Self::ProRes => ffi::AV_CODEC_ID_PRORES,
        }
    }
}

/// The one encoder alpha2 opens for `codec`: `aac`, or the reference `libopus` (`FFmpeg`'s own
/// `opus` is experimental and never a candidate).
pub(crate) const fn audio_encoder_name(codec: NativeAudioCodec) -> &'static std::ffi::CStr {
    match codec {
        NativeAudioCodec::Aac => c"aac",
        NativeAudioCodec::Opus => c"libopus",
    }
}

/// The encoder this runtime opens for `codec`, or `None` when only an experimental one exists.
pub(crate) fn audio_encoder(codec: NativeAudioCodec) -> Option<AVCodecRef<'static>> {
    AVCodec::find_encoder_by_name(audio_encoder_name(codec)).filter(|encoder| {
        u32::try_from(encoder.capabilities).unwrap_or(0) & ffi::AV_CODEC_CAP_EXPERIMENTAL == 0
    })
}

/// How a concrete encoder executes, as the linked runtime declares it.
///
/// Taken from `AV_CODEC_CAP_HARDWARE` / `AV_CODEC_CAP_HYBRID` rather than the name
/// suffix, so unknown encoders classify correctly. A few misdeclared encoders are
/// corrected by `declared_encoder_class`.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeEncoderClass {
    /// Runs on the CPU and draws on the software codec thread budget.
    Software,
    /// Runs on a hardware engine and ignores the thread budget.
    Hardware,
    /// Can do either. `MediaFoundation`'s `_mf` encoders enumerate a synchronous software transform
    /// with `hw_encoding=0` and an asynchronous hardware one with `hw_encoding=1`.
    Hybrid,
}

impl NativeEncoderClass {
    /// Stable identifier for diagnostics and the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Hardware => "hardware",
            Self::Hybrid => "hybrid",
        }
    }

    /// The ways alpha2 runs an encoder of this class, most preferred first (`true` = GPU). A hybrid
    /// encoder runs on the CPU unless `hybrid_hardware` is set, and then keeps the CPU as fallback.
    #[must_use]
    pub const fn modes(self, hybrid_hardware: bool) -> &'static [bool] {
        match self {
            Self::Hardware => &[true],
            Self::Hybrid if hybrid_hardware => &[true, false],
            Self::Software | Self::Hybrid => &[false],
        }
    }
}

/// One way to run a registered encoder. Only a hybrid encoder has two.
pub(crate) struct VideoEncoderCandidate {
    pub(crate) encoder: AVCodecRef<'static>,
    pub(crate) on_hardware: bool,
}

impl VideoEncoderCandidate {
    pub(crate) fn name(&self) -> String {
        self.encoder.name().to_string_lossy().into_owned()
    }

    pub(crate) fn codec(&self) -> NativeVideoCodec {
        [
            NativeVideoCodec::H264,
            NativeVideoCodec::Hevc,
            NativeVideoCodec::Vp9,
            NativeVideoCodec::Av1,
            NativeVideoCodec::Mpeg4Part2,
            NativeVideoCodec::ProRes,
        ]
        .into_iter()
        .find(|codec| codec.codec_id() == self.encoder.id)
        .unwrap_or(NativeVideoCodec::H264)
    }
}

/// Whether hybrid encoders are configured to run on the GPU.
///
/// `_mf` encoders default to `hw_encoding=0` (software MFT). `ALPHA2_VIDEO_ENCODER_HYBRID=hardware`
/// tries `hw_encoding=1` first and keeps the software transform behind it, because `FFmpeg` does
/// not fall back between them itself.
#[must_use]
pub fn hybrid_encoders_use_hardware() -> bool {
    std::env::var("ALPHA2_VIDEO_ENCODER_HYBRID")
        .is_ok_and(|value| value.eq_ignore_ascii_case("hardware"))
}

/// Why one candidate encoder was not the one an export opened.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeEncoderRejection {
    /// The encoder refuses an output this small.
    Dimensions,
    /// `avcodec_open2` failed: no device, no licence, unsupported profile.
    OpenFailed,
    /// The encoder silently ignored an option alpha2 requires it to honour.
    OptionsIgnored,
    /// The encoder takes none of the pixel formats the output can be written in.
    PixelFormat,
}

impl NativeEncoderRejection {
    /// Stable identifier for diagnostics and the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dimensions => "dimensions",
            Self::OpenFailed => "open-failed",
            Self::OptionsIgnored => "options-ignored",
            Self::PixelFormat => "pixel-format",
        }
    }
}

/// One candidate encoder an export tried and moved past.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeVideoEncoderFallback {
    /// `FFmpeg` short name of the candidate.
    pub encoder_name: String,
    /// Whether the candidate was configured to encode on the GPU.
    pub hardware_accelerated: bool,
    /// Typed reason it was rejected.
    pub reason: NativeEncoderRejection,
    /// What the rejection is evidence of; a busy device can open later.
    pub kind: OpenFailureKind,
    /// The runtime's own message, kept verbatim.
    pub detail: String,
}

impl NativeVideoEncoderFallback {
    /// This rejection as a refusal.
    #[must_use]
    pub fn refusal(&self) -> EncoderRefusal {
        EncoderRefusal {
            encoder: self.encoder_name.clone(),
            hardware_accelerated: self.hardware_accelerated,
            kind: self.kind,
            reason: self.detail.clone(),
        }
    }
}

/// The encoder an export actually opened, and the candidates it passed over.
///
/// A platform can register an encoder it cannot open, so every export reports what it opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeVideoEncoderSelection {
    /// `FFmpeg` short name of the encoder that opened.
    pub encoder_name: String,
    /// How that encoder executes.
    pub class: NativeEncoderClass,
    /// Whether it is configured to encode on the GPU.
    pub hardware_accelerated: bool,
    /// Candidates tried first, in preference order.
    pub rejected: Vec<NativeVideoEncoderFallback>,
    /// Whether the encoder runs the rate control's quantizer or its bitrate.
    pub rate_control: crate::AppliedRateControl,
    /// Why a quantizer that was asked for is not the one running.
    pub quantizer_refused: Option<String>,
    /// `FFmpeg` workers the encoder was opened with on the CPU; `None` on the GPU.
    pub software_threads: Option<i64>,
    /// The pictures it takes: the output's layout, or its `P010` form for an encoder that takes
    /// 10-bit 4:2:0 only that way.
    pub pixels: alpha2_media::PixelLayout,
}

/// One linked video encoder available to the native sinks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeVideoEncoderCapability {
    /// Stable logical codec requested through [`VideoEncodeConfig`].
    pub codec: NativeVideoCodec,
    /// Concrete `FFmpeg` encoder implementation for this codec.
    pub encoder_name: String,
    /// How the linked runtime declares this encoder executes.
    pub class: NativeEncoderClass,
    /// Whether this entry runs the encoder on the GPU. A hybrid encoder has an entry per mode.
    pub hardware_accelerated: bool,
    /// Whether this is the default encoder for its logical codec.
    pub preferred: bool,
    /// Smallest even width accepted by this concrete encoder.
    pub minimum_width: u32,
    /// Smallest even height accepted by this concrete encoder.
    pub minimum_height: u32,
    /// The user-installed library behind a non-`FFmpeg` encoder; `None` for `FFmpeg`'s own.
    pub library: Option<NativeEncoderLibrary>,
}

/// A run-time loaded encoder library ([`crate::OPENH264_ENCODER_NAME`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeEncoderLibrary {
    /// Who provided it ([`crate::OpenH264Provider::as_str`]).
    pub provider: &'static str,
    /// The version it reports.
    pub version: String,
    /// Where it was loaded from.
    pub path: String,
    /// The notice its licence requires be shown.
    pub notice: &'static str,
}

impl NativeEncoderLibrary {
    fn openh264(library: &crate::OpenH264Library) -> Self {
        Self {
            provider: library.provider().as_str(),
            version: library.version().to_owned(),
            path: library.path().to_owned(),
            notice: crate::OPENH264_NOTICE,
        }
    }
}

/// One linked audio encoder available to the native sinks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAudioEncoderCapability {
    /// Stable logical codec requested through [`AvEncodeConfig`].
    pub codec: NativeAudioCodec,
    /// Concrete `FFmpeg` encoder selected for that codec on this host.
    pub encoder_name: String,
}

/// Runtime codec inventory for the native sinks.
///
/// Container/codec pairing is engine policy ([`alpha2_media::Container`]); this says which
/// codecs this runtime can encode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCodecCapabilities {
    /// Video encoders accepted by [`NativeVideoEncodeSink`](crate::NativeVideoEncodeSink) and
    /// [`NativeAvEncodeSink`](crate::NativeAvEncodeSink).
    pub video: Vec<NativeVideoEncoderCapability>,
    /// Audio encoders accepted by [`NativeAvEncodeSink`](crate::NativeAvEncodeSink) and the
    /// GPU-resident and packet-copy exports.
    pub audio: Vec<NativeAudioEncoderCapability>,
}

/// Reports the encoders usable by the native sinks: the linked `FFmpeg`'s, and
/// `OpenH264` when installed.
///
/// A hybrid encoder has one entry per mode. Order is usable hardware first, then software in
/// `FFmpeg`'s order, then `OpenH264`; exactly one entry per codec is marked preferred.
#[must_use]
pub fn codec_capabilities() -> NativeCodecCapabilities {
    let video = [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Vp9,
        NativeVideoCodec::Av1,
        NativeVideoCodec::Mpeg4Part2,
        NativeVideoCodec::ProRes,
    ]
    .into_iter()
    .flat_map(|codec| video_encoder_inventory(codec).usable)
    .collect();
    NativeCodecCapabilities {
        video,
        audio: audio_encoder_capabilities(),
    }
}

/// Whether some encoder of `codec` takes `layout`'s pictures or their `P010` form.
#[must_use]
pub fn encodes_pixels(
    capabilities: &NativeCodecCapabilities,
    codec: NativeVideoCodec,
    layout: alpha2_media::PixelLayout,
) -> bool {
    capabilities
        .video
        .iter()
        .filter(|capability| capability.codec == codec)
        .any(|capability| {
            capability.encoder_name == crate::OPENH264_ENCODER_NAME && layout.is_yuv420p()
                || std::ffi::CString::new(capability.encoder_name.as_str())
                    .ok()
                    .and_then(|name| AVCodec::find_encoder_by_name(&name))
                    .is_some_and(|encoder| encoder_layout(&encoder, layout).is_some())
        })
}

/// The audio encoders [`codec_capabilities`] lists, without probing video encoders.
#[must_use]
pub fn audio_encoder_capabilities() -> Vec<NativeAudioEncoderCapability> {
    NativeAudioCodec::ALL
        .into_iter()
        .filter_map(|codec| {
            let encoder = audio_encoder(codec)?;
            Some(NativeAudioEncoderCapability {
                codec,
                encoder_name: encoder.name().to_string_lossy().into_owned(),
            })
        })
        .collect()
}

/// The encoders of one codec this machine runs, and the ones its open probe refused.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EncoderInventory {
    /// The encoders usable for the codec, as [`codec_capabilities`] lists them.
    pub usable: Vec<NativeVideoEncoderCapability>,
    /// Registered encoders whose open probe failed, and why. A busy one is asked again next time.
    pub refused: Vec<EncoderRefusal>,
}

/// The usable encoders for `codec` alone, and the ones whose probe refused.
///
/// Only this codec's encoders are open-probed.
#[must_use]
pub fn video_encoder_inventory(codec: NativeVideoCodec) -> EncoderInventory {
    let (choices, refused) = available_video_encoders(codec);
    let usable = choices
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            let encoder_name = choice.name();
            let (minimum_width, minimum_height) = encoder_minimum_dimensions(&encoder_name);
            NativeVideoEncoderCapability {
                codec,
                class: choice.class(),
                hardware_accelerated: choice.on_hardware(),
                preferred: index == 0,
                encoder_name,
                minimum_width,
                minimum_height,
                library: match &choice {
                    VideoEncoderChoice::Ffmpeg(_) => None,
                    VideoEncoderChoice::OpenH264(library) => {
                        Some(NativeEncoderLibrary::openh264(library))
                    }
                },
            }
        })
        .collect();
    EncoderInventory { usable, refused }
}

/// One encoder an export can try: a registered `FFmpeg` one, or `OpenH264`.
pub(crate) enum VideoEncoderChoice {
    Ffmpeg(VideoEncoderCandidate),
    OpenH264(std::sync::Arc<crate::OpenH264Library>),
}

impl VideoEncoderChoice {
    pub(crate) fn name(&self) -> String {
        match self {
            Self::Ffmpeg(candidate) => candidate.name(),
            Self::OpenH264(_) => crate::OPENH264_ENCODER_NAME.to_owned(),
        }
    }

    pub(crate) const fn on_hardware(&self) -> bool {
        match self {
            Self::Ffmpeg(candidate) => candidate.on_hardware,
            Self::OpenH264(_) => false,
        }
    }

    pub(crate) fn class(&self) -> NativeEncoderClass {
        match self {
            Self::Ffmpeg(candidate) => encoder_class(&candidate.encoder),
            Self::OpenH264(_) => NativeEncoderClass::Software,
        }
    }
}

/// Every encoder for `codec` this machine runs, most preferred first, plus the probe refusals.
/// `OpenH264` comes last because `libx264` compresses better where present.
pub(crate) fn available_video_encoders(
    codec: NativeVideoCodec,
) -> (Vec<VideoEncoderChoice>, Vec<EncoderRefusal>) {
    let (mut choices, refused) = compatible_encoders(registered_video_encoders(codec));
    if codec == NativeVideoCodec::H264
        && let Some(library) = crate::openh264_status().library()
    {
        choices.push(VideoEncoderChoice::OpenH264(std::sync::Arc::clone(library)));
    }
    (choices, refused)
}

/// `candidates` split into those that run here and probe refusals; ones the registration
/// rules out are in neither.
fn compatible_encoders(
    candidates: Vec<VideoEncoderCandidate>,
) -> (Vec<VideoEncoderChoice>, Vec<EncoderRefusal>) {
    let mut choices = Vec::new();
    let mut refused = Vec::new();
    for candidate in candidates {
        let name = candidate.name();
        if !registration_is_compatible(&candidate, &name) {
            continue;
        }
        match encoder_runs_here(&candidate, &name) {
            Ok(()) => choices.push(VideoEncoderChoice::Ffmpeg(candidate)),
            Err(refusal) => refused.push(refusal),
        }
    }
    (choices, refused)
}

/// One codec's hardware decode registration for this platform's backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHardwareDecodeCapability {
    /// Logical codec.
    pub codec: NativeVideoCodec,
    /// Concrete `FFmpeg` decoder implementation, when one is linked.
    pub decoder_name: Option<String>,
    /// Whether that decoder advertises this platform's hardware configuration.
    ///
    /// Registration only; device, profile and surface support are decided when a stream opens.
    pub hardware_config: bool,
    /// The decoder a software session opens, or `None` when only hardware can decode the
    /// codec (AV1 without libdav1d).
    pub software_decoder: Option<String>,
}

/// Hardware decode registration reported by the linked `FFmpeg` build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHardwareDecodeCapabilities {
    /// Stable backend identifier, or `none` on a build without a GPU path.
    pub backend: &'static str,
    /// One entry per codec alpha2 can present from a decoder surface.
    pub codecs: Vec<NativeHardwareDecodeCapability>,
}

/// Backend this build decodes with, independent of any device being present.
#[must_use]
pub const fn hardware_decode_backend() -> &'static str {
    #[cfg(feature = "hwdecode")]
    {
        #[cfg(target_os = "linux")]
        {
            "vulkan"
        }
        #[cfg(target_os = "macos")]
        {
            "videotoolbox"
        }
        #[cfg(target_os = "windows")]
        {
            "d3d12va"
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            "none"
        }
    }
    #[cfg(not(feature = "hwdecode"))]
    {
        "none"
    }
}

/// Reports which codecs the linked `FFmpeg` registers for this backend.
#[must_use]
pub fn hardware_decode_capabilities() -> NativeHardwareDecodeCapabilities {
    let codecs = [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
        NativeVideoCodec::Vp9,
    ]
    .into_iter()
    .map(|codec| {
        let decoder = AVCodec::find_decoder(codec.codec_id());
        NativeHardwareDecodeCapability {
            codec,
            decoder_name: decoder
                .as_ref()
                .map(|decoder| decoder.name().to_string_lossy().into_owned()),
            hardware_config: crate::decode::registered_video_decoders(codec.codec_id())
                .any(|decoder| decoder_has_hardware_config(&decoder)),
            software_decoder: crate::decode::software_video_decoder(codec.codec_id())
                .ok()
                .map(|decoder| decoder.name().to_string_lossy().into_owned()),
        }
    })
    .collect();
    NativeHardwareDecodeCapabilities {
        backend: hardware_decode_backend(),
        codecs,
    }
}

/// Whether `decoder` advertises this platform's hardware device configuration.
#[allow(unused_variables)]
fn decoder_has_hardware_config(decoder: &AVCodecRef<'_>) -> bool {
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    ))]
    {
        #[cfg(target_os = "linux")]
        let (device_type, pixel_format) = (ffi::AV_HWDEVICE_TYPE_VULKAN, ffi::AV_PIX_FMT_VULKAN);
        #[cfg(target_os = "macos")]
        let (device_type, pixel_format) = (
            ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            ffi::AV_PIX_FMT_VIDEOTOOLBOX,
        );
        #[cfg(target_os = "windows")]
        let (device_type, pixel_format) = (ffi::AV_HWDEVICE_TYPE_D3D12VA, ffi::AV_PIX_FMT_D3D12);
        (0..)
            .map_while(|index| decoder.hw_config(index))
            .any(|config| config.device_type == device_type && config.pix_fmt == pixel_format)
    }
    #[cfg(not(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )))]
    {
        false
    }
}

/// Encoders named in `ALPHA2_TEST_SKIP_ENCODERS` (comma separated), hidden in test builds as if
/// unregistered. Valgrind runs use it for `libsvtav1`, which reads uninitialized memory.
#[cfg(test)]
pub(crate) fn test_skips_encoder(name: &std::ffi::CStr) -> bool {
    static SKIPPED: LazyLock<Vec<String>> = LazyLock::new(|| {
        std::env::var("ALPHA2_TEST_SKIP_ENCODERS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    });
    let name = name.to_string_lossy();
    SKIPPED.iter().any(|skipped| *skipped == name)
}

#[cfg(not(test))]
const fn test_skips_encoder(_name: &std::ffi::CStr) -> bool {
    false
}

/// Every way to run each encoder registered for `codec`, GPU ones first.
pub(crate) fn registered_video_encoders(codec: NativeVideoCodec) -> Vec<VideoEncoderCandidate> {
    let hybrid_hardware = hybrid_encoders_use_hardware();
    let mut candidates = AVCodec::iterate()
        .filter(|encoder| {
            encoder.id == codec.codec_id()
                // Only `prores_ks` takes the 4444 alpha and vendor options alpha2 sets.
                && (codec != NativeVideoCodec::ProRes || encoder.name() == c"prores_ks")
                && !test_skips_encoder(encoder.name())
                && AVCodec::find_encoder_by_name(encoder.name())
                    .is_some_and(|found| found.as_ptr() == encoder.as_ptr())
        })
        .flat_map(|encoder| {
            encoder_class(&encoder)
                .modes(hybrid_hardware)
                .iter()
                .filter_map(move |&on_hardware| {
                    AVCodec::find_encoder_by_name(encoder.name()).map(|encoder| {
                        VideoEncoderCandidate {
                            encoder,
                            on_hardware,
                        }
                    })
                })
        })
        .collect::<Vec<_>>();
    // Stable sort: FFmpeg's registration order within each tier, hardware first.
    candidates.sort_by_key(|candidate| !candidate.on_hardware);
    candidates
}

/// Classifies `encoder` from the capability bits it declares.
pub(crate) fn encoder_class(encoder: &AVCodec) -> NativeEncoderClass {
    let name = encoder.name().to_string_lossy();
    declared_encoder_class(&name, u32::try_from(encoder.capabilities).unwrap_or(0))
}

/// Encoders the pinned runtime declares as software although they run only on hardware.
///
/// `FFmpeg` 8.x declares `h264_videotoolbox` with neither `AV_CODEC_CAP_HARDWARE` nor
/// `AV_CODEC_CAP_HYBRID` yet opens it with `allow_sw=0`. An entry applies only while the
/// runtime declares neither bit.
const MISDECLARED_HARDWARE_ENCODERS: &[&str] = &["h264_videotoolbox"];

/// The class from `capabilities`, except that a [`MISDECLARED_HARDWARE_ENCODERS`] entry declaring
/// neither bit is `Hardware`.
pub(crate) fn declared_encoder_class(name: &str, capabilities: u32) -> NativeEncoderClass {
    if capabilities & ffi::AV_CODEC_CAP_HYBRID != 0 {
        NativeEncoderClass::Hybrid
    } else if capabilities & ffi::AV_CODEC_CAP_HARDWARE != 0
        || MISDECLARED_HARDWARE_ENCODERS.contains(&name)
    {
        NativeEncoderClass::Hardware
    } else {
        NativeEncoderClass::Software
    }
}

#[cfg(test)]
pub(crate) fn encoder_is_compatible(candidate: &VideoEncoderCandidate) -> bool {
    let name = candidate.name();
    registration_is_compatible(candidate, &name) && encoder_runs_here(candidate, &name).is_ok()
}

/// What the registration alone rules out: an encoder that cannot take its codec's base pictures
/// ([`NativeVideoCodec::base_layout`]), or a hardware one whose device has no node here.
fn registration_is_compatible(candidate: &VideoEncoderCandidate, name: &str) -> bool {
    // The native sink submits software frames, so encoders needing an `AVHWFramesContext` are out.
    let codec = candidate.codec();
    if encoder_layout(&candidate.encoder, codec.base_layout()).is_none() {
        return false;
    }
    !candidate.on_hardware || hardware_device_node(name) != Some(false)
}

/// The pictures `encoder` takes for `layout`: `layout` itself, or its `P010` form for an encoder
/// that takes 10-bit 4:2:0 only that way (NVENC); `None` when neither.
pub(crate) fn encoder_layout(
    encoder: &AVCodec,
    layout: alpha2_media::PixelLayout,
) -> Option<alpha2_media::PixelLayout> {
    let Some(formats) = encoder.pix_fmts() else {
        return Some(layout);
    };
    [layout, layout.as_p010()]
        .into_iter()
        .find(|candidate| formats.contains(&crate::convert::pixel_format(*candidate)))
}

/// Whether the encoder is listed only after an open probe ([`encoder_runs_here`]).
///
/// This covers hardware encoders without a device node, NVENC, and every `_mf` or `_videotoolbox`
/// encoder. `VideoToolbox` is probed by name because a runtime may misdeclare its class, and an
/// NVIDIA device node does not imply NVENC (compute-only GPUs have none).
pub(crate) fn needs_open_probe(on_hardware: bool, name: &str) -> bool {
    (on_hardware && (hardware_device_node(name).is_none() || name.ends_with("_nvenc")))
        || name.ends_with("_mf")
        || name.ends_with("_videotoolbox")
}

/// Whether `encoder` encodes a frame on this machine, for encoders whose
/// registration does not prove it.
///
/// `MediaFoundation` registers every `_mf` encoder whether or not a working transform exists, and
/// hardware encoders register without their device (`VideoToolbox` in a headless VM, NVENC without
/// the driver). Such encoders are listed only after encoding one 256x144 frame with export options.
/// Verdicts that cannot change (ran, or absent) are cached per process and mode, and so is a
/// run of [`UNCLASSIFIED_FAILURES_KEPT`] unclassified failures. Busy failures are asked again.
///
/// Probes run lazily per codec, since each NVENC probe creates a CUDA context (about 0.4 s).
fn encoder_runs_here(candidate: &VideoEncoderCandidate, name: &str) -> Result<(), EncoderRefusal> {
    probe_outcome(candidate, name, true).unwrap_or(Ok(()))
}

/// `Some(Ok)` for an encoder that needs no probe or passed one, `Some(Err)` for one that failed,
/// and `None` for one not probed yet when `probe` is unset.
fn probe_outcome(
    candidate: &VideoEncoderCandidate,
    name: &str,
    probe: bool,
) -> Option<Result<(), EncoderRefusal>> {
    static VERDICTS: LazyLock<ProbeCache> = LazyLock::new(Mutex::default);
    if !needs_open_probe(candidate.on_hardware, name) {
        return Some(Ok(()));
    }
    cached_probe(
        &VERDICTS,
        name,
        candidate.on_hardware,
        probe.then_some(|| probe_open(candidate, name)),
    )
}

/// Encodes one probe frame and classifies a failure from what the encoder logged.
fn probe_open(
    candidate: &VideoEncoderCandidate,
    name: &str,
) -> Result<(), (OpenFailureKind, String)> {
    let mark = alpha2_ffmpeg_bridge::log_mark();
    match probe_software_encode(candidate, name, 256, 144, None).0 {
        None => Ok(()),
        Some(error) => {
            let lines = alpha2_ffmpeg_bridge::log_since(mark, &[name]);
            Err((
                classify_open_failure(name, &error, &lines),
                format!("{error}{}", alpha2_ffmpeg_bridge::log_detail(&lines)),
            ))
        }
    }
}

/// What a failed open of `encoder` is evidence of, from its error and logged `lines`.
///
/// Only fixed-wording statuses are read: NVENC's parenthesized status, Vulkan's `VkResult`,
/// `VideoToolbox`'s `OSStatus`, and `MediaFoundation`'s missing transform or `E_FAIL`. Anything
/// else is [`OpenFailureKind::Unknown`], never busy and cached only after repeats, so a wording
/// change costs re-probes rather than a wrong class.
pub(crate) fn classify_open_failure(
    encoder: &str,
    error: &str,
    lines: &[LogLine],
) -> OpenFailureKind {
    let said =
        |words: &str| error.contains(words) || lines.iter().any(|line| line.text.contains(words));
    if encoder.ends_with("_nvenc") {
        // OUT_OF_MEMORY, LOCK_BUSY, ENCODER_BUSY and INCOMPATIBLE_CLIENT_KEY are how a
        // consumer GPU reports its session limit (some drivers answer 21); 1-5 mean no or
        // unsupported device. The status decides first, since `FFmpeg` logs "No capable
        // devices found" after any failed device.
        return match lines.iter().find_map(|line| nvenc_status(&line.text)) {
            Some(10 | 13 | 18 | 21) => OpenFailureKind::Busy,
            Some(1..=5) => OpenFailureKind::Absent,
            None if said("No capable devices found") || said("Cannot load") => {
                OpenFailureKind::Absent
            }
            _ => OpenFailureKind::Unknown,
        };
    }
    if encoder.ends_with("_vulkan") {
        let busy = lines.iter().any(|line| {
            line.text.contains("Unable to create video session")
                && (line.text.contains("VK_ERROR_OUT_OF_DEVICE_MEMORY")
                    || line.text.contains("VK_ERROR_TOO_MANY_OBJECTS"))
        });
        return if busy {
            OpenFailureKind::Busy
        } else {
            OpenFailureKind::Unknown
        };
    }
    if encoder.ends_with("_videotoolbox") {
        // kVTVideoEncoderNotAvailableNowErr and kVTCouldNotFindVideoEncoderErr.
        if said("-12915") {
            return OpenFailureKind::Busy;
        }
        if said("-12908") {
            return OpenFailureKind::Absent;
        }
        return OpenFailureKind::Unknown;
    }
    // 80004005 is E_FAIL as `mfenc` logs a failed `ProcessInput`.
    if encoder.ends_with("_mf")
        && (said("could not find any MFT") || said("E_FAIL") || said("80004005"))
    {
        return OpenFailureKind::Absent;
    }
    OpenFailureKind::Unknown
}

/// The first parenthesized number in an `nvenc_print_error` line.
fn nvenc_status(line: &str) -> Option<u32> {
    line.match_indices('(').find_map(|(start, _)| {
        let rest = &line[start + 1..];
        let end = rest.find(')')?;
        let digits = &rest[..end];
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// The encoders [`codec_capabilities`] would list from what is known so far, without running any
/// probe; unprobed encoders are left out. For a startup log line.
#[must_use]
pub fn video_encoders_without_probing() -> (Vec<NativeVideoEncoderCapability>, Vec<String>) {
    let mut unprobed = Vec::new();
    let mut listed = Vec::new();
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Vp9,
        NativeVideoCodec::Av1,
        NativeVideoCodec::Mpeg4Part2,
        NativeVideoCodec::ProRes,
    ] {
        for candidate in registered_video_encoders(codec) {
            let name = candidate.name();
            if !registration_is_compatible(&candidate, &name) {
                continue;
            }
            match probe_outcome(&candidate, &name, false) {
                Some(Ok(())) => {
                    let (minimum_width, minimum_height) = encoder_minimum_dimensions(&name);
                    listed.push(NativeVideoEncoderCapability {
                        codec,
                        class: encoder_class(&candidate.encoder),
                        hardware_accelerated: candidate.on_hardware,
                        preferred: !listed
                            .iter()
                            .any(|listed: &NativeVideoEncoderCapability| listed.codec == codec),
                        encoder_name: name,
                        minimum_width,
                        minimum_height,
                        library: None,
                    });
                }
                Some(Err(_)) => {}
                None => unprobed.push(format!("{name} {}", mode_label(candidate.on_hardware))),
            }
        }
    }
    (listed, unprobed)
}

/// Whether a hardware encoder's device has a node here, for families Linux knows a node
/// for; `None` for the rest.
///
/// A node replaces the open probe for those families, except NVENC, where it only gates the probe
/// so machines without an NVIDIA GPU pay nothing.
#[cfg(target_os = "linux")]
fn hardware_device_node(name: &str) -> Option<bool> {
    if name.ends_with("_nvenc") {
        return Some(Path::new("/dev/nvidia0").exists() || Path::new("/dev/nvidiactl").exists());
    }
    if name.ends_with("_vaapi") || name.ends_with("_qsv") || name.ends_with("_amf") {
        return Some(fs::read_dir("/dev/dri").is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("renderD") || name.starts_with("card"))
            })
        }));
    }
    if name.ends_with("_v4l2m2m") {
        return Some((0..64).any(|index| Path::new(&format!("/dev/video{index}")).exists()));
    }
    None
}

/// Off Linux there is no device node to check; the open probe decides.
#[cfg(not(target_os = "linux"))]
const fn hardware_device_node(_name: &str) -> Option<bool> {
    None
}

/// Why an export of `codec` has no encoder, and what to do about it.
///
/// Names why the GPU path declined (`gpu_output_unavailable`), what the runtime offers instead
/// (`offered`), and for H.264 whether `OpenH264` is installed.
#[must_use]
pub fn no_video_encoder_message(
    codec: NativeVideoCodec,
    offered: &[NativeVideoEncoderCapability],
    gpu_output_unavailable: Option<&str>,
) -> String {
    let identity = crate::ffmpeg_runtime_identity();
    let inventory = offered
        .iter()
        .filter(|capability| capability.library.is_none())
        .map(|capability| format!("{}:{}", capability.codec.as_str(), capability.encoder_name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!("no usable {} encoder: ", codec.as_str());
    if let Some(reason) = gpu_output_unavailable {
        let _ = write!(message, "GPU output path unavailable ({reason}); ");
    }
    let _ = write!(
        message,
        "the linked FFmpeg runtime ({}, pinned SDK {}) offers [{}]",
        identity.version,
        identity.pinned_sdk_release,
        if inventory.is_empty() {
            "none"
        } else {
            inventory.as_str()
        }
    );
    if codec != NativeVideoCodec::H264 {
        return message;
    }
    let status = crate::openh264_status();
    if let Some(reason) = status.reason() {
        let _ = write!(
            message,
            "; OpenH264 is {}: {reason}",
            status.as_str().replace('-', " ")
        );
    }
    let _ = write!(message, ". To export H.264 here: {}", h264_encoder_remedy());
    message
}

/// Remedies for a machine without a CPU-frame H.264 encoder: install `OpenH264`, use an `FFmpeg`
/// with `libx264`, or export another codec.
#[must_use]
pub fn h264_encoder_remedy() -> String {
    let openh264 = match crate::openh264_status() {
        crate::OpenH264Status::Disabled => {
            "unset ALPHA2_OPENH264=0 to use the installed OpenH264 encoder".to_owned()
        }
        _ => format!(
            "install the OpenH264 encoder ({})",
            crate::OPENH264_INSTALL_COMMAND
        ),
    };
    format!(
        "{openh264}, build alpha2-host from source against an FFmpeg 8 that has libx264, or \
         export VP9 (.webm) or AV1 instead"
    )
}

/// [`no_video_encoder_message`] as an encode error, with `codec`'s probe refusals so a busy encoder
/// is told from a missing one.
fn unavailable_codec_error(codec: NativeVideoCodec, refused: Vec<EncoderRefusal>) -> EncodeError {
    let message = no_video_encoder_message(codec, &codec_capabilities().video, None);
    if refused.is_empty() {
        return EncodeError::Adapter(message);
    }
    EncodeError::EncodersRefused {
        message: format!("{}; {message}", refusal_list(&refused)),
        refusals: refused,
    }
}

/// `refusals` for a message: `"h264_nvenc on GPU (busy): <reason>; ..."`.
fn refusal_list(refusals: &[EncoderRefusal]) -> String {
    refusals
        .iter()
        .map(|refusal| {
            format!(
                "{} {} ({}): {}",
                refusal.encoder,
                mode_label(refusal.hardware_accelerated),
                refusal.kind.as_str(),
                refusal.reason
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// [`video_encoder_candidates_and_refusals`] without the refusals.
#[cfg(test)]
pub(crate) fn video_encoder_candidates(
    codec: NativeVideoCodec,
    requested_name: Option<&str>,
) -> Result<Vec<VideoEncoderChoice>, EncodeError> {
    video_encoder_candidates_and_refusals(codec, requested_name).map(|(candidates, _)| candidates)
}

/// Encoders to try for `codec`, most preferred first, and the probe refusals left out.
///
/// A pinned encoder is tried alone (no silent substitution), each way it runs.
fn video_encoder_candidates_and_refusals(
    codec: NativeVideoCodec,
    requested_name: Option<&str>,
) -> Result<(Vec<VideoEncoderChoice>, Vec<EncoderRefusal>), EncodeError> {
    if requested_name == Some(crate::OPENH264_ENCODER_NAME) {
        if codec != NativeVideoCodec::H264 {
            return Err(EncodeError::Adapter(format!(
                "video encoder {} does not implement {}",
                crate::OPENH264_ENCODER_NAME,
                codec.as_str()
            )));
        }
        let status = crate::openh264_status();
        return match status.library() {
            Some(library) => Ok((
                vec![VideoEncoderChoice::OpenH264(std::sync::Arc::clone(library))],
                Vec::new(),
            )),
            None => Err(EncodeError::Adapter(format!(
                "video encoder {} is unavailable: {}{}",
                crate::OPENH264_ENCODER_NAME,
                status.reason().unwrap_or_default(),
                status
                    .remedy()
                    .map(|remedy| format!("; {remedy}"))
                    .unwrap_or_default()
            ))),
        };
    }
    if let Some(name) = requested_name {
        let encoder = select_video_encoder(codec, Some(name))?;
        let (candidates, refused) = compatible_encoders(
            registered_video_encoders(codec)
                .into_iter()
                .filter(|candidate| candidate.encoder.as_ptr() == encoder.as_ptr())
                .collect(),
        );
        if candidates.is_empty() && !refused.is_empty() {
            return Err(EncodeError::EncodersRefused {
                message: refusal_list(&refused),
                refusals: refused,
            });
        }
        return Ok((candidates, refused));
    }
    let (candidates, refused) = available_video_encoders(codec);
    if candidates.is_empty() {
        return Err(unavailable_codec_error(codec, refused));
    }
    Ok((candidates, refused))
}

/// The one encoder an export opens without fallback: the pinned `requested_name` (which must
/// implement `codec` and be advertised), else the codec's preferred encoder.
pub(crate) fn select_video_encoder(
    codec: NativeVideoCodec,
    requested_name: Option<&str>,
) -> Result<AVCodecRef<'static>, EncodeError> {
    if let Some(name) = requested_name {
        let name_c = CString::new(name)
            .map_err(|_| EncodeError::Adapter("video encoder name contains a NUL byte".into()))?;
        let encoder = AVCodec::find_encoder_by_name(&name_c)
            .ok_or_else(|| EncodeError::Adapter(format!("video encoder {name} is unavailable")))?;
        if encoder.id != codec.codec_id() {
            return Err(EncodeError::Adapter(format!(
                "video encoder {name} does not implement {}",
                codec.as_str()
            )));
        }
        let inventory = video_encoder_inventory(codec);
        if !inventory
            .usable
            .iter()
            .any(|capability| capability.encoder_name == name)
        {
            let refused = inventory
                .refused
                .into_iter()
                .filter(|refusal| refusal.encoder == name)
                .collect::<Vec<_>>();
            if !refused.is_empty() {
                return Err(EncodeError::EncodersRefused {
                    message: refusal_list(&refused),
                    refusals: refused,
                });
            }
            return Err(EncodeError::Adapter(format!(
                "video encoder {name} is not compatible with the native encode path"
            )));
        }
        return Ok(encoder);
    }

    let inventory = video_encoder_inventory(codec);
    let Some(capability) = inventory
        .usable
        .into_iter()
        .find(|capability| capability.library.is_none())
    else {
        return Err(unavailable_codec_error(codec, inventory.refused));
    };
    let name = CString::new(capability.encoder_name)
        .map_err(|_| EncodeError::Adapter("video encoder name contains a NUL byte".into()))?;
    AVCodec::find_encoder_by_name(&name).ok_or_else(|| unavailable_codec_error(codec, Vec::new()))
}

/// What one sink needs to open its video encoder.
pub(crate) struct VideoEncoderOpenRequest<'a> {
    /// Output format the encoder is configured for.
    pub config: &'a VideoEncodeConfig,
    /// Concrete encoder the caller pinned, if any.
    pub requested_name: Option<&'a str>,
    /// Software worker budget, asked for only when a CPU candidate opens.
    pub threads: SoftwareEncoderThreads<'a>,
    /// Whether the container wants the extradata in the stream header.
    pub global_header: bool,
    /// Coded width, already range-checked by the caller.
    pub width: i32,
    /// Coded height, already range-checked by the caller.
    pub height: i32,
}

/// The worker count a software video encoder opens with, decided only when one opens.
///
/// The decode/encode CPU split depends on whether the encoder runs on the CPU, which is known only
/// after a candidate opens: a hardware encoder may fail and fall through to a software one, which
/// must not inherit the hardware split's zero share. Asked at most once.
pub(crate) struct SoftwareEncoderThreads<'a> {
    decide: &'a dyn Fn(Option<usize>) -> i64,
    decided: std::cell::OnceCell<i64>,
}

impl<'a> SoftwareEncoderThreads<'a> {
    /// Defers the count to `decide`, which receives the encoder's own count when the picture
    /// fixes it ([`crate::CodecThreadRequest::encoder_fixed_threads`]).
    pub(crate) fn new(decide: &'a dyn Fn(Option<usize>) -> i64) -> Self {
        Self {
            decide,
            decided: std::cell::OnceCell::new(),
        }
    }

    /// The decided count for an encoder that runs as many workers as it is given.
    pub(crate) fn get(&self) -> i64 {
        self.get_fixed(None)
    }

    /// The decided count, for an encoder that runs `fixed` workers when it does.
    pub(crate) fn get_fixed(&self, fixed: Option<usize>) -> i64 {
        *self.decided.get_or_init(|| (self.decide)(fixed).max(1))
    }
}

/// Opens the first candidate encoder this machine can actually run.
///
/// A registered encoder may still fail `avcodec_open2` (no MFT, no device). Each rejection is
/// recorded with a typed reason so the run reports which encoder it ended up on.
///
/// # Errors
/// Returns the last candidate's failure, prefixed with every earlier one, when none opens.
pub(crate) fn open_video_encoder(
    request: &VideoEncoderOpenRequest<'_>,
) -> Result<
    (
        crate::video_backend::VideoEncoderBackend,
        NativeVideoEncoderSelection,
    ),
    EncodeError,
> {
    let (candidates, refused) =
        video_encoder_candidates_and_refusals(request.config.codec, request.requested_name)?;
    open_first_video_encoder(request, candidates).map_err(|error| match error {
        // Probe refusals come first; a busy one among them decides the class.
        EncodeError::EncodersRefused {
            message,
            refusals: opened,
        } if !refused.is_empty() => EncodeError::EncodersRefused {
            message: format!("{}; {message}", refusal_list(&refused)),
            refusals: refused.into_iter().chain(opened).collect(),
        },
        error => error,
    })
}

/// An opened encoder, how it runs the rate control, and its CPU workers.
struct OpenedVideoEncoder {
    backend: crate::video_backend::VideoEncoderBackend,
    rate_control: crate::AppliedRateControl,
    quantizer_refused: Option<String>,
    software_threads: Option<i64>,
    pixels: alpha2_media::PixelLayout,
}

/// Opens the first of `candidates` that runs, recording why each earlier one did not.
pub(crate) fn open_first_video_encoder(
    request: &VideoEncoderOpenRequest<'_>,
    candidates: Vec<VideoEncoderChoice>,
) -> Result<
    (
        crate::video_backend::VideoEncoderBackend,
        NativeVideoEncoderSelection,
    ),
    EncodeError,
> {
    let mut rejected = Vec::new();
    let last = candidates.len().saturating_sub(1);
    for (index, candidate) in candidates.into_iter().enumerate() {
        let name = candidate.name();
        let class = candidate.class();
        let mark = alpha2_ffmpeg_bridge::log_mark();
        let opened = match &candidate {
            VideoEncoderChoice::Ffmpeg(candidate) => {
                open_one_video_encoder(request, candidate, &name).map(
                    |(encoder, rate_control, quantizer_refused, pixels)| OpenedVideoEncoder {
                        backend: crate::video_backend::VideoEncoderBackend::Ffmpeg(encoder),
                        rate_control,
                        quantizer_refused,
                        software_threads: (!candidate.on_hardware).then(|| request.threads.get()),
                        pixels,
                    },
                )
            }
            VideoEncoderChoice::OpenH264(library) => open_openh264(request, library),
        };
        match opened {
            Ok(opened) => {
                return Ok((
                    opened.backend,
                    NativeVideoEncoderSelection {
                        encoder_name: name,
                        class,
                        hardware_accelerated: candidate.on_hardware(),
                        rejected,
                        rate_control: opened.rate_control,
                        quantizer_refused: opened.quantizer_refused,
                        software_threads: opened.software_threads,
                        pixels: opened.pixels,
                    },
                ));
            }
            Err((reason, error)) => {
                let (kind, detail) = rejection_kind(&name, reason, &error, mark);
                if index == last {
                    let earlier = rejected
                        .iter()
                        .map(|fallback: &NativeVideoEncoderFallback| {
                            format!(
                                "{} {} ({}): {}",
                                fallback.encoder_name,
                                mode_label(fallback.hardware_accelerated),
                                fallback.reason.as_str(),
                                fallback.detail
                            )
                        })
                        .collect::<Vec<_>>();
                    let prefix = if earlier.is_empty() {
                        String::new()
                    } else {
                        format!("after {}; ", earlier.join("; "))
                    };
                    let message = format!(
                        "{prefix}{name} {} ({}): {detail}",
                        mode_label(candidate.on_hardware()),
                        reason.as_str()
                    );
                    let refusals = rejected
                        .iter()
                        .map(NativeVideoEncoderFallback::refusal)
                        .chain([EncoderRefusal {
                            encoder: name,
                            hardware_accelerated: candidate.on_hardware(),
                            kind,
                            reason: detail,
                        }])
                        .collect();
                    return Err(EncodeError::EncodersRefused { message, refusals });
                }
                alpha2_os::diagnostic!(
                    "alpha2 native export: video encoder {name} {} ({}) rejected: {detail}",
                    mode_label(candidate.on_hardware()),
                    reason.as_str()
                );
                rejected.push(NativeVideoEncoderFallback {
                    encoder_name: name,
                    hardware_accelerated: candidate.on_hardware(),
                    reason,
                    kind,
                    detail,
                });
            }
        }
    }
    Err(unavailable_codec_error(request.config.codec, Vec::new()))
}

/// What a rejection of `name` is evidence of, and its detail. Open failures are classified from the
/// lines logged since `mark`.
fn rejection_kind(
    name: &str,
    reason: NativeEncoderRejection,
    error: &EncodeError,
    mark: alpha2_ffmpeg_bridge::LogMark,
) -> (OpenFailureKind, String) {
    match reason {
        NativeEncoderRejection::OpenFailed => {
            let lines = alpha2_ffmpeg_bridge::log_since(mark, &[name]);
            let detail = error.to_string();
            (
                classify_open_failure(name, &detail, &lines),
                format!("{detail}{}", alpha2_ffmpeg_bridge::log_detail(&lines)),
            )
        }
        NativeEncoderRejection::Dimensions
        | NativeEncoderRejection::OptionsIgnored
        | NativeEncoderRejection::PixelFormat => (OpenFailureKind::Unsuitable, error.to_string()),
    }
}

/// Opens `OpenH264` on constant QP when the rate control has a quantizer, else on bitrate.
///
/// Its thread count comes from the picture height (`openh264::encoder::slice_count`). The software
/// budget is still decided here because the rest of the export splits the CPU from it.
fn open_openh264(
    request: &VideoEncoderOpenRequest<'_>,
    library: &crate::OpenH264Library,
) -> Result<OpenedVideoEncoder, (NativeEncoderRejection, EncodeError)> {
    let config = request.config;
    if !config.pixels.is_yuv420p() {
        return Err((
            NativeEncoderRejection::PixelFormat,
            EncodeError::Adapter(format!(
                "OpenH264 writes 8-bit 4:2:0 only, not {}",
                config.pixels.ffmpeg_name()
            )),
        ));
    }
    validate_encoder_dimensions(crate::OPENH264_ENCODER_NAME, config.width, config.height)
        .map_err(|error| (NativeEncoderRejection::Dimensions, error))?;
    let open_failed = |message: String| {
        (
            NativeEncoderRejection::OpenFailed,
            EncodeError::Adapter(message),
        )
    };
    if !request.global_header {
        return Err(open_failed(
            "OpenH264 writes its parameter sets into the stream header, which this container \
             lacks"
                .to_owned(),
        ));
    }
    let (rate, rate_control) = crate::rate_control::openh264_rate(config.rate_control);
    let time_base = encoder_time_base(config.frame_rate, None)
        .map_err(|error| (NativeEncoderRejection::OpenFailed, error))?;
    // OpenH264 runs one thread per slice whatever it is offered; the split reserves that count.
    let _ = request
        .threads
        .get_fixed(Some(usize::from(crate::openh264::thread_count(
            crate::openh264::slice_count(config.height),
        ))));
    let encoder = crate::openh264::OpenH264Encoder::open(
        library,
        &crate::openh264::OpenH264Settings {
            width: config.width,
            height: config.height,
            frame_rate: config.frame_rate,
            color: config.color,
            sample_aspect_ratio: config.display.sample_aspect_ratio,
            rate,
            keyframe_interval: u32::try_from(keyframe_interval(config.frame_rate)).unwrap_or(1),
        },
        time_base,
    )
    .map_err(open_failed)?;
    let threads = i64::from(encoder.threads());
    Ok(OpenedVideoEncoder {
        backend: crate::video_backend::VideoEncoderBackend::OpenH264(Box::new(encoder)),
        rate_control,
        quantizer_refused: None,
        software_threads: Some(threads),
        pixels: alpha2_media::PixelLayout::YUV420P,
    })
}

/// An opened context, its rate control, why a quantizer was refused, and the pictures it takes.
type OpenedContext = (
    EncoderContext,
    crate::AppliedRateControl,
    Option<String>,
    alpha2_media::PixelLayout,
);

/// Opens `candidate` on the quantizer when the encoder accepts it, otherwise on bitrate.
///
/// A refused quantizer is not a refused encoder: the export continues on
/// the bitrate and reports it.
fn open_one_video_encoder(
    request: &VideoEncoderOpenRequest<'_>,
    candidate: &VideoEncoderCandidate,
    name: &str,
) -> Result<OpenedContext, (NativeEncoderRejection, EncodeError)> {
    let pixels = encoder_layout(&candidate.encoder, request.config.pixels).ok_or_else(|| {
        (
            NativeEncoderRejection::PixelFormat,
            EncodeError::Adapter(format!(
                "{name} does not take {} pictures",
                request.config.pixels.ffmpeg_name()
            )),
        )
    })?;
    validate_encoder_dimensions(name, request.config.width, request.config.height)
        .map_err(|error| (NativeEncoderRejection::Dimensions, error))?;
    let refused = match crate::rate_control::quantizer_request(name, request.config.rate_control) {
        None => None,
        Some(Err(reason)) => Some(reason),
        Some(Ok(quantized)) => {
            match open_video_encoder_context(request, candidate, name, Some(&quantized), pixels) {
                Ok(encoder) => {
                    let applied = crate::rate_control::applied_of_quantizer_request(name);
                    return Ok((encoder, applied, None, pixels));
                }
                Err((_, error)) => Some(format!("{} refused: {error}", quantized.describe())),
            }
        }
    };
    if let Some(reason) = &refused {
        alpha2_os::diagnostic!("alpha2 native export: {name} runs the bitrate fallback: {reason}");
    }
    open_video_encoder_context(request, candidate, name, None, pixels)
        .map(|encoder| (encoder, crate::AppliedRateControl::Bitrate, refused, pixels))
}

fn open_video_encoder_context(
    request: &VideoEncoderOpenRequest<'_>,
    candidate: &VideoEncoderCandidate,
    name: &str,
    quantized: Option<&crate::rate_control::MechanismRequest>,
    pixels: alpha2_media::PixelLayout,
) -> Result<EncoderContext, (NativeEncoderRejection, EncodeError)> {
    let mut encoder = EncoderContext::from(AVCodecContext::new(&candidate.encoder));
    encoder.set_width(request.width);
    encoder.set_height(request.height);
    encoder.set_pix_fmt(crate::convert::pixel_format(pixels));
    let rate = request.config.frame_rate;
    let time_base = encoder_time_base(rate, max_time_base_denominator(candidate.encoder.id))
        .map_err(|error| (NativeEncoderRejection::OpenFailed, error))?;
    encoder.set_time_base(time_base);
    // Square pixels are the default; only another ratio is written.
    let sar = request.config.display.sample_aspect_ratio;
    if sar != alpha2_media::SampleAspectRatio::default() {
        encoder.set_sample_aspect_ratio(rsmpeg::avutil::ra(
            i32::try_from(sar.num).unwrap_or(1),
            i32::try_from(sar.den).unwrap_or(1),
        ));
    }
    encoder.set_framerate(rsmpeg::avutil::ra(
        i32::try_from(rate.numerator()).unwrap_or(i32::MAX),
        i32::try_from(rate.denominator()).unwrap_or(1),
    ));
    encoder.set_bit_rate(quantized.map_or_else(
        || i64::try_from(request.config.rate_control.bitrate).unwrap_or(i64::MAX),
        |quantized| quantized.bit_rate,
    ));
    encoder.set_gop_size(keyframe_interval(request.config.frame_rate));
    encoder.set_max_b_frames(encoder_max_b_frames(name));
    if request.global_header {
        let flags = encoder.flags | i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0);
        encoder.set_flags(flags);
    }
    // Hardware candidates never decide the software split.
    let threads = if candidate.on_hardware {
        0
    } else {
        request.threads.get()
    };
    let mut base = video_encoder_options(
        request.config.color,
        &candidate.encoder,
        candidate.on_hardware,
        threads,
        request.config.width,
    );
    if candidate.codec() == NativeVideoCodec::ProRes {
        for (key, value) in crate::convert::prores_options(pixels) {
            base = crate::convert::set_encoder_option(base, key, &value);
        }
    }
    let options = match quantized {
        Some(quantized) => quantized.dictionary(base),
        None => crate::rate_control::rate_control_options(name, request.config.rate_control, base),
    };
    let unused =
        alpha2_ffmpeg_bridge::open_codec_context(&mut encoder, options).map_err(|error| {
            let hint = match quantized {
                Some(_) => "",
                None => crate::rate_control::constant_rate_hint(name, request.config.rate_control),
            };
            let error = match encode_adapter_error(error) {
                EncodeError::Adapter(message) if !hint.is_empty() => {
                    EncodeError::Adapter(format!("{message}{hint}"))
                }
                error => error,
            };
            (NativeEncoderRejection::OpenFailed, error)
        })?;
    reject_unused_encode_options(unused)
        .map_err(|error| (NativeEncoderRejection::OptionsIgnored, error))?;
    Ok(encoder)
}

/// The largest time base denominator encoders of `codec` can write, if limited. MPEG-4 Part 2 and
/// H.263 store it in 16 bits, so 1001/120000 (119.88 fps) is refused.
pub(crate) const fn max_time_base_denominator(codec: ffi::AVCodecID) -> Option<u32> {
    match codec {
        ffi::AV_CODEC_ID_MPEG4 | ffi::AV_CODEC_ID_H263 | ffi::AV_CODEC_ID_H263P => Some(65_535),
        _ => None,
    }
}

/// [`alpha2_host_core::encode_policy::encoder_time_base`] as an `FFmpeg` rational.
///
/// # Errors
/// For a rate whose terms do not fit an `FFmpeg` rational.
pub(crate) fn encoder_time_base(
    rate: alpha2_time::FrameRate,
    limit: Option<u32>,
) -> Result<AVRational, EncodeError> {
    alpha2_host_core::encode_policy::encoder_time_base(rate, limit)
        .map(|(num, den)| rsmpeg::avutil::ra(num, den))
}

pub(crate) fn encoder_max_b_frames(name: &str) -> i32 {
    // Keep VideoToolbox's default of no reordering: forced B-frames are device-dependent and can
    // produce invalid packet timestamps.
    if name.ends_with("_videotoolbox") {
        0
    } else {
        2
    }
}

/// The smallest even width and height encoder `name` accepts.
#[must_use]
pub fn encoder_minimum_dimensions(name: &str) -> (u32, u32) {
    // NVIDIA AV1 requires at least 145x128, its H.264/HEVC encoders 145x49; sizes stay even.
    if name == "av1_nvenc" {
        (146, 128)
    } else if name.ends_with("_nvenc") {
        (146, 50)
    } else {
        (2, 2)
    }
}

pub(crate) fn validate_encoder_dimensions(
    encoder_name: &str,
    width: u32,
    height: u32,
) -> Result<(), EncodeError> {
    let (minimum_width, minimum_height) = encoder_minimum_dimensions(encoder_name);
    if width < minimum_width || height < minimum_height {
        Err(EncodeError::Adapter(format!(
            "encoder {encoder_name} requires at least {minimum_width}x{minimum_height}, received {width}x{height}"
        )))
    } else {
        Ok(())
    }
}

/// One hardware configuration a video encoder declares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeEncoderHardwareConfig {
    /// Hardware pixel format this configuration carries frames in.
    pub pixel_format: String,
    /// `FFmpeg` hardware device type backing it.
    pub device_type: String,
    /// `AV_CODEC_HW_CONFIG_METHOD_*` bits.
    pub methods: i32,
    /// Whether the encoder takes its frames through an `AVHWFramesContext`.
    pub needs_hardware_frames: bool,
}

/// What one registered video encoder declares, and what happened when alpha2 opened it.
///
/// Shows registration, advertisement and instantiation side by side, per platform.
#[derive(Clone, Debug, Eq, PartialEq)]
// Independent observations of one encoder, reported side by side, not a state.
#[allow(clippy::struct_excessive_bools)]
pub struct NativeVideoEncoderProbe {
    /// Logical codec this encoder implements.
    pub codec: NativeVideoCodec,
    /// `FFmpeg` short name.
    pub encoder_name: String,
    /// Class taken from the encoder's own capability bits.
    pub class: NativeEncoderClass,
    /// Whether this row runs the encoder on the GPU.
    pub hardware_accelerated: bool,
    /// Raw `AV_CODEC_CAP_*` bits, so an unexpected class can be traced.
    pub capability_bits: u32,
    /// Pixel formats the encoder declares, or empty when it declares none.
    pub pixel_formats: Vec<String>,
    /// Hardware configurations the encoder declares.
    pub hardware_configs: Vec<NativeEncoderHardwareConfig>,
    /// Whether `AV_PIX_FMT_YUV420P` is among the declared formats.
    pub accepts_yuv420p: bool,
    /// Whether every declared format is a hardware one (why `h264_vulkan` is not advertised).
    pub requires_hardware_frames: bool,
    /// Whether [`codec_capabilities`] advertises it for the YUV420P sink.
    pub advertised: bool,
    /// Smallest frame the encoder accepts, as alpha2 records it.
    pub minimum_size: (u32, u32),
    /// `None` when a software YUV420P frame produced a packet; otherwise why not.
    pub software_open_error: Option<String>,
    /// Packets drained from that attempt.
    pub software_packets: u32,
    /// Which rate-control mechanisms opened, and with what options.
    pub rate_control: Vec<NativeRateControlProbe>,
}

/// Whether one encoder opened with one rate-control mechanism.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRateControlProbe {
    /// The mechanism asked for.
    pub mechanism: crate::RateControlMechanism,
    /// The options that carried it, or `None` where alpha2 knows no spelling for this family.
    pub options: Option<String>,
    /// Packets drained when it opened, used every option and encoded a frame; otherwise why not.
    pub outcome: Result<u32, String>,
    /// The device a hardware-frames encoder was probed on: a Vulkan device index, or for
    /// `*_d3d12va` the answering adapter (`DX12 adapter <index> (<name>)`).
    pub device: Option<String>,
}

/// A quantizer every encoder of `codec` accepts: `crf` 0-63 for
/// `libvpx-vp9`/`libsvtav1`, QP 0-255 for hardware.
pub(crate) const fn probe_quantizer(codec: NativeVideoCodec, software: bool) -> u32 {
    match codec {
        NativeVideoCodec::H264 | NativeVideoCodec::Hevc => 26,
        NativeVideoCodec::Av1 | NativeVideoCodec::Vp9 => {
            if software {
                32
            } else {
                100
            }
        }
        NativeVideoCodec::Mpeg4Part2 => 4,
        // Never asked: `ProRes` has no quantizer mode alpha2 opens.
        NativeVideoCodec::ProRes => 0,
    }
}

/// Opens every registered video encoder and reports what it actually does.
///
/// Each candidate is opened at `width`x`height` and fed one YUV420P frame, so the report tells
/// registered, advertised and ran apart. A hybrid encoder has a row per mode.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn probe_video_encoders(width: u32, height: u32) -> Vec<NativeVideoEncoderProbe> {
    let advertised = codec_capabilities();
    [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Vp9,
        NativeVideoCodec::Av1,
        NativeVideoCodec::Mpeg4Part2,
        NativeVideoCodec::ProRes,
    ]
    .into_iter()
    .flat_map(|codec| {
        registered_video_encoders(codec)
            .into_iter()
            .map(move |candidate| (codec, candidate))
    })
    .map(|(codec, candidate)| {
        let encoder = &candidate.encoder;
        let encoder_name = candidate.name();
        let pixel_formats = encoder.pix_fmts().map_or_else(Vec::new, |formats| {
            formats
                .iter()
                .map(|format| {
                    alpha2_ffmpeg_bridge::pixel_format_name(*format)
                        .unwrap_or_else(|| "unknown".to_owned())
                })
                .collect()
        });
        let hardware_configs = (0..)
            .map_while(|index| encoder.hw_config(index))
            .map(|config| NativeEncoderHardwareConfig {
                pixel_format: alpha2_ffmpeg_bridge::pixel_format_name(config.pix_fmt)
                    .unwrap_or_else(|| "unknown".to_owned()),
                device_type: alpha2_ffmpeg_bridge::hardware_device_type_name(config.device_type)
                    .unwrap_or_else(|| "none".to_owned()),
                methods: config.methods,
                needs_hardware_frames: crate::decode::has_hw_config_method(
                    config.methods,
                    ffi::AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX,
                ),
            })
            .collect::<Vec<_>>();
        let accepts_yuv420p = encoder
            .pix_fmts()
            .is_none_or(|formats| formats.contains(&ffi::AV_PIX_FMT_YUV420P));
        let (software_open_error, software_packets) =
            probe_software_encode(&candidate, &encoder_name, width, height, None);
        let requires_hardware_frames = !accepts_yuv420p
            && hardware_configs
                .iter()
                .any(|config| config.needs_hardware_frames);
        let rate_control = crate::RateControlMechanism::ALL
            .into_iter()
            .map(|mechanism| {
                let request = crate::rate_control::mechanism_request(
                    &encoder_name,
                    mechanism,
                    2_000_000,
                    // D3D12VA takes 0-52 for every codec, AV1 included.
                    if encoder_name.ends_with("_d3d12va") {
                        26
                    } else {
                        probe_quantizer(
                            codec,
                            encoder_class(encoder) == NativeEncoderClass::Software,
                        )
                    },
                );
                let mut device = None;
                let outcome = match &request {
                    None => Err("no known option for this encoder family".to_owned()),
                    Some(request) if requires_hardware_frames => probe_hardware_frames_encode(
                        encoder,
                        &encoder_name,
                        (width, height),
                        mechanism,
                        request,
                    )
                    .map(|(packets, on)| {
                        device = Some(on);
                        packets
                    }),
                    Some(request) => {
                        match probe_software_encode(
                            &candidate,
                            &encoder_name,
                            width,
                            height,
                            Some(request),
                        ) {
                            (None, packets) => Ok(packets),
                            (Some(error), _) => Err(error),
                        }
                    }
                };
                NativeRateControlProbe {
                    mechanism,
                    options: request
                        .as_ref()
                        .map(crate::rate_control::MechanismRequest::describe),
                    outcome,
                    device,
                }
            })
            .collect();
        NativeVideoEncoderProbe {
            codec,
            class: encoder_class(encoder),
            hardware_accelerated: candidate.on_hardware,
            capability_bits: u32::try_from(encoder.capabilities).unwrap_or(0),
            advertised: advertised.video.iter().any(|capability| {
                capability.codec == codec
                    && capability.encoder_name == encoder_name
                    && capability.hardware_accelerated == candidate.on_hardware
            }),
            minimum_size: encoder_minimum_dimensions(&encoder_name),
            requires_hardware_frames,
            accepts_yuv420p,
            pixel_formats,
            hardware_configs,
            encoder_name,
            software_open_error,
            software_packets,
            rate_control,
        }
    })
    .collect()
}

/// Opens an encoder that only takes hardware frames, with `request`.
///
/// For `*_vulkan`, `FFmpeg` creates its own device with whatever video-encode queue is offered.
/// D3D12VA rows open on the first DX12 adapter whose D3D12 Video Encode reports the encoder; a mode
/// the device does not list is refused with its reason.
///
/// Returns the packets and the answering device (Vulkan index, or DX12 adapter index and name).
#[allow(unused_variables)]
fn probe_hardware_frames_encode(
    encoder: &AVCodecRef<'static>,
    name: &str,
    (width, height): (u32, u32),
    mechanism: crate::RateControlMechanism,
    request: &crate::rate_control::MechanismRequest,
) -> Result<(u32, String), String> {
    #[cfg(all(target_os = "linux", feature = "hwencode"))]
    if name.ends_with("_vulkan") {
        return crate::hwencode::probe_vulkan_rate_control(encoder, width, height, request);
    }
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    if name.ends_with("_d3d12va") {
        return crate::hwencode::probe_d3d12va_rate_control(
            name, width, height, mechanism, request,
        );
    }
    Err("takes hardware frames this probe cannot supply".to_owned())
}

/// Opens `candidate` and feeds it one software YUV420P frame.
fn probe_software_encode(
    candidate: &VideoEncoderCandidate,
    name: &str,
    width: u32,
    height: u32,
    request: Option<&crate::rate_control::MechanismRequest>,
) -> (Option<String>, u32) {
    let Ok(coded_width) = i32::try_from(width) else {
        return (Some("width is out of range".to_owned()), 0);
    };
    let Ok(coded_height) = i32::try_from(height) else {
        return (Some("height is out of range".to_owned()), 0);
    };
    if let Err(error) = validate_encoder_dimensions(name, width, height) {
        return (Some(error.to_string()), 0);
    }
    let encoder = &candidate.encoder;
    let base = candidate.codec().base_layout();
    let format =
        encoder_layout(encoder, base).map_or(ffi::AV_PIX_FMT_YUV420P, crate::convert::pixel_format);
    let mut context = EncoderContext::from(AVCodecContext::new(encoder));
    context.set_width(coded_width);
    context.set_height(coded_height);
    context.set_pix_fmt(format);
    let time_base = rsmpeg::avutil::ra(1, 30);
    context.set_time_base(time_base);
    context.set_framerate(rsmpeg::avutil::ra(30, 1));
    context.set_bit_rate(request.map_or(2_000_000, |request| request.bit_rate));
    context.set_gop_size(12);
    context.set_max_b_frames(encoder_max_b_frames(name));
    let options = video_encoder_options(
        ColorMetadata::EXPORT,
        encoder,
        candidate.on_hardware,
        1,
        width,
    );
    let options = match request {
        Some(request) => request.dictionary(options),
        None => options,
    };
    match alpha2_ffmpeg_bridge::open_codec_context(&mut context, options) {
        Err(error) => return (Some(error.to_string()), 0),
        Ok(unused) => {
            if let Err(error) = reject_unused_encode_options(unused) {
                return (Some(error.to_string()), 0);
            }
        }
    }
    let Some(image) = rsmpeg::avutil::AVImage::new(format, coded_width, coded_height, 1) else {
        return (Some("cannot allocate a probe frame".to_owned()), 0);
    };
    let mut frame = rsmpeg::avutil::AVFrameWithImage::new(image);
    frame.set_pts(0);
    frame.set_time_base(time_base);
    if let Err(error) = context.send_frame(Some(&frame)) {
        return (Some(error.to_string()), 0);
    }
    if let Err(error) = context.send_frame(None) {
        return (Some(error.to_string()), 0);
    }
    let mut packets = 0;
    loop {
        match context.receive_packet() {
            Ok(_) => packets += 1,
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(error) => return (Some(error.to_string()), packets),
        }
    }
    if packets == 0 {
        return (
            Some("opened but produced no packet from a software frame".to_owned()),
            0,
        );
    }
    (None, packets)
}
