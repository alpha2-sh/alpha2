//! Codec capability discovery and encoder selection.

use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

use crate::convert::{encode_adapter_error, reject_unused_encode_options, video_encoder_options};
use crate::video_encode::VideoEncodeConfig;
use alpha2_ffmpeg_bridge::EncoderContext;
use alpha2_media::{ColorMetadata, EncodeError};
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
    /// Resolved audio bitrate in bits per second; the host resolves the level.
    pub audio_bit_rate: u32,
}

/// Video codecs implemented by the native encode sinks.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeVideoCodec {
    /// H.264/AVC, preferred for broad browser, Windows, and Apple playback.
    H264,
    /// H.265/HEVC.
    Hevc,
    /// VP9, when the linked `FFmpeg` build provides a VP9 encoder.
    Vp9,
    /// AV1, when the linked `FFmpeg` build provides an AV1 encoder.
    Av1,
    /// MPEG-4 Part 2, retained for compatibility with existing alpha2 hosts.
    Mpeg4Part2,
    /// Apple `ProRes`, through `prores_ks`: 422 HQ, or 4444 with alpha.
    ProRes,
}

impl NativeVideoCodec {
    /// Stable identifier suitable for an application protocol or settings file.
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

    /// The pictures an encoder of this codec must take to be offered at all:
    /// 10-bit 4:2:2 for `ProRes`, 8-bit 4:2:0 for every other codec.
    #[must_use]
    pub const fn base_layout(self) -> alpha2_media::PixelLayout {
        match self {
            Self::ProRes => alpha2_media::PixelLayout {
                chroma: alpha2_media::Chroma::Yuv422,
                bit_depth: alpha2_media::BitDepth::Ten,
                alpha: false,
                planes: alpha2_media::PlaneLayout::Planar,
            },
            _ => alpha2_media::PixelLayout::YUV420P,
        }
    }

    /// The engine's codec this encoder family implements.
    ///
    /// The rate mapping is keyed on [`alpha2_media::VideoCodec`] alone, so
    /// there is one front door to it; this is the native side's way in.
    #[must_use]
    pub const fn model(self) -> alpha2_media::VideoCodec {
        match self {
            Self::H264 => alpha2_media::VideoCodec::H264,
            Self::Hevc => alpha2_media::VideoCodec::Hevc,
            Self::Vp9 => alpha2_media::VideoCodec::Vp9,
            Self::Av1 => alpha2_media::VideoCodec::Av1,
            Self::Mpeg4Part2 => alpha2_media::VideoCodec::Mpeg4Part2,
            Self::ProRes => alpha2_media::VideoCodec::ProRes,
        }
    }

    /// The native codec for the engine's `codec`.
    #[must_use]
    pub const fn from_model(codec: alpha2_media::VideoCodec) -> Self {
        match codec {
            alpha2_media::VideoCodec::H264 => Self::H264,
            alpha2_media::VideoCodec::Hevc => Self::Hevc,
            alpha2_media::VideoCodec::Vp9 => Self::Vp9,
            alpha2_media::VideoCodec::Av1 => Self::Av1,
            alpha2_media::VideoCodec::Mpeg4Part2 => Self::Mpeg4Part2,
            alpha2_media::VideoCodec::ProRes => Self::ProRes,
        }
    }

    pub(crate) const fn codec_id(self) -> ffi::AVCodecID {
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

/// Audio codecs implemented by the native encode sinks.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeAudioCodec {
    /// AAC-LC compatible audio, through `FFmpeg`'s `aac`.
    Aac,
    /// Opus, through `libopus`.
    Opus,
}

impl NativeAudioCodec {
    /// Every codec, in the order the engine lists them.
    pub const ALL: [Self; 2] = [Self::Aac, Self::Opus];

    /// Stable identifier suitable for an application protocol or settings file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }

    /// The engine's codec this encoder family implements.
    #[must_use]
    pub const fn model(self) -> alpha2_media::AudioCodec {
        match self {
            Self::Aac => alpha2_media::AudioCodec::Aac,
            Self::Opus => alpha2_media::AudioCodec::Opus,
        }
    }

    /// The native codec for the engine's `codec`.
    #[must_use]
    pub const fn from_model(codec: alpha2_media::AudioCodec) -> Self {
        match codec {
            alpha2_media::AudioCodec::Aac => Self::Aac,
            alpha2_media::AudioCodec::Opus => Self::Opus,
        }
    }

    /// The one encoder alpha2 opens for the codec: `FFmpeg`'s `aac`, and the
    /// reference `libopus` for Opus. `FFmpeg`'s own `opus` encoder is
    /// experimental and is never a candidate.
    pub(crate) const fn encoder_name(self) -> &'static std::ffi::CStr {
        match self {
            Self::Aac => c"aac",
            Self::Opus => c"libopus",
        }
    }
}

/// The encoder this runtime opens for `codec`, or `None` when it links none,
/// or only one marked experimental.
pub(crate) fn audio_encoder(codec: NativeAudioCodec) -> Option<AVCodecRef<'static>> {
    AVCodec::find_encoder_by_name(codec.encoder_name()).filter(|encoder| {
        u32::try_from(encoder.capabilities).unwrap_or(0) & ffi::AV_CODEC_CAP_EXPERIMENTAL == 0
    })
}

/// How a concrete encoder executes, as the linked runtime declares it.
///
/// Taken from capability bits rather than the name suffix:
/// `AV_CODEC_CAP_HARDWARE` and `AV_CODEC_CAP_HYBRID` are set by the encoder
/// itself, so a build that adds an encoder alpha2 has never heard of is still
/// classified correctly, and a hybrid encoder is not described as though it
/// were one or the other.
/// The one exception is a short list of encoders the pinned runtime
/// misdeclares (`declared_encoder_class`).
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeEncoderClass {
    /// Runs on the CPU and draws on the software codec thread budget.
    Software,
    /// Runs on a hardware engine and ignores the thread budget.
    Hardware,
    /// Can do either. `MediaFoundation`'s `_mf` encoders are the shipped case:
    /// `hw_encoding=0` enumerates a synchronous software transform and
    /// `hw_encoding=1` an asynchronous hardware one.
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

    /// The ways alpha2 runs an encoder of this class, most preferred first:
    /// `true` on the GPU, `false` on the CPU. A hybrid encoder runs on the
    /// CPU unless `hybrid_hardware` asks for the GPU, and then still has the
    /// CPU behind it, because a machine without a hardware transform cannot
    /// run the GPU mode at all.
    #[must_use]
    pub const fn modes(self, hybrid_hardware: bool) -> &'static [bool] {
        match self {
            Self::Hardware => &[true],
            Self::Hybrid if hybrid_hardware => &[true, false],
            Self::Software | Self::Hybrid => &[false],
        }
    }
}

/// One way to run a registered encoder: the encoder, and whether alpha2
/// configures it to encode on the GPU. Only a hybrid encoder has two.
pub(crate) struct VideoEncoderCandidate {
    pub(crate) encoder: AVCodecRef<'static>,
    pub(crate) on_hardware: bool,
}

impl VideoEncoderCandidate {
    pub(crate) fn name(&self) -> String {
        self.encoder.name().to_string_lossy().into_owned()
    }

    /// The codec this encoder writes.
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
/// `h264_mf` and its siblings default to `hw_encoding=0`, which enumerates
/// `MFT_ENUM_FLAG_SYNCMFT` (the synchronous *software* transform category),
/// so the shipped default is a CPU encoder that correctly draws on the codec
/// thread budget. `ALPHA2_VIDEO_ENCODER_HYBRID=hardware` tries `hw_encoding=1`
/// first, which asks `MediaFoundation` for a hardware MFT, and keeps the
/// software transform of the same encoder behind it for the SKUs and VMs
/// that have none: `FFmpeg` does not fall back from one to the other itself.
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
    /// The encoder takes none of the pixel formats the output can be
    /// written in.
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
    /// Whether the candidate was configured to encode on the GPU. A hybrid
    /// encoder can be tried on the GPU and then on the CPU.
    pub hardware_accelerated: bool,
    /// Typed reason it was rejected.
    pub reason: NativeEncoderRejection,
    /// The runtime's own message, kept verbatim.
    pub detail: String,
}

/// The encoder an export actually opened, and what it passed over.
///
/// Advertisement is not instantiation: a platform can register an encoder it
/// cannot open on this machine. Every export reports what it opened rather
/// than letting the capability list imply it.
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
    /// `FFmpeg` workers the encoder was opened with when it runs on the CPU;
    /// `None` on the GPU, which takes no share of the software budget.
    pub software_threads: Option<i64>,
    /// The pictures it takes: the output's layout, or its `P010` form for
    /// an encoder that takes 10-bit 4:2:0 only as that.
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
    /// Whether this entry runs the encoder on the GPU. A hybrid encoder is
    /// listed once per way it runs here ([`hybrid_encoders_use_hardware`]).
    pub hardware_accelerated: bool,
    /// Whether this is the default encoder for its logical codec.
    pub preferred: bool,
    /// Smallest even width accepted by this concrete encoder.
    pub minimum_width: u32,
    /// Smallest even height accepted by this concrete encoder.
    pub minimum_height: u32,
    /// The library behind an encoder that is not `FFmpeg`'s, which the user
    /// installed: `None` for `FFmpeg`'s own.
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
/// Which containers offer which codec is the engine's policy
/// ([`alpha2_media::Container`]), the same on every runtime; this says which
/// of those codecs this runtime can encode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCodecCapabilities {
    /// Video encoders accepted by [`NativeVideoEncodeSink`](crate::NativeVideoEncodeSink) and
    /// [`NativeAvEncodeSink`](crate::NativeAvEncodeSink).
    pub video: Vec<NativeVideoEncoderCapability>,
    /// Audio encoders accepted by [`NativeAvEncodeSink`](crate::NativeAvEncodeSink) and the
    /// GPU-resident and packet-copy exports.
    pub audio: Vec<NativeAudioEncoderCapability>,
}

/// Reports the encoders usable by alpha2's native sinks: the linked
/// `FFmpeg`'s, and `OpenH264` when the user installed it.
///
/// A logical codec may have multiple entries, and a hybrid encoder one per
/// way it runs. Hardware entries appear first when they accept the native
/// YUV420P path and their required device is present, then software ones in
/// `FFmpeg`'s order, then `OpenH264`; exactly one entry per codec is marked
/// preferred.
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
    .flat_map(video_encoder_capabilities)
    .collect();
    NativeCodecCapabilities {
        video,
        audio: audio_encoder_capabilities(),
    }
}

/// Whether one of `capabilities`' encoders of `codec` takes `layout`'s
/// pictures, or their `P010` form: whether this runtime can write the codec
/// so.
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

/// The audio encoders [`codec_capabilities`] lists, without looking at any
/// video encoder.
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

/// The encoders usable for `codec` alone, as [`codec_capabilities`] lists
/// them.
///
/// An export needs only its own codec's, and an encoder whose registration
/// does not prove it runs here is opened to find out
/// ([`encoder_runs_here`]): asking for one codec opens only that codec's.
#[must_use]
pub fn video_encoder_capabilities(codec: NativeVideoCodec) -> Vec<NativeVideoEncoderCapability> {
    available_video_encoders(codec)
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
        .collect()
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

/// Every encoder for `codec` this machine runs, most preferred first:
/// `FFmpeg`'s, then `OpenH264` for H.264 when it loaded. `OpenH264` comes last
/// because `libx264`, where a runtime has it, compresses better.
pub(crate) fn available_video_encoders(codec: NativeVideoCodec) -> Vec<VideoEncoderChoice> {
    let mut choices = registered_video_encoders(codec)
        .into_iter()
        .filter(encoder_is_compatible)
        .map(VideoEncoderChoice::Ffmpeg)
        .collect::<Vec<_>>();
    if codec == NativeVideoCodec::H264
        && let Some(library) = crate::openh264_status().library()
    {
        choices.push(VideoEncoderChoice::OpenH264(std::sync::Arc::clone(library)));
    }
    choices
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
    /// This is registration only. Whether a device, profile, and surface
    /// format actually work is decided when a stream is opened, and every
    /// export reports which path its frames took.
    pub hardware_config: bool,
    /// The decoder a software session opens for the codec, or `None` when
    /// only a hardware session can decode it (AV1 on a runtime without
    /// libdav1d).
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

/// Test builds only: the encoders `ALPHA2_TEST_SKIP_ENCODERS` (names, comma separated) hides
/// from this run, as though the runtime registered none of them. The valgrind run sets it for
/// `libsvtav1`, whose own code reads memory it never wrote; no production build reads it.
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
                // `prores_ks` is the `ProRes` encoder whose 4444 alpha and vendor options alpha2
                // sets; `FFmpeg`'s other two take neither the same way.
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

/// Classifies `encoder` from the capability bits it declares
/// ([`declared_encoder_class`]).
pub(crate) fn encoder_class(encoder: &AVCodec) -> NativeEncoderClass {
    let name = encoder.name().to_string_lossy();
    declared_encoder_class(&name, u32::try_from(encoder.capabilities).unwrap_or(0))
}

/// Encoders the pinned runtime registers as software although they run only
/// on a hardware engine.
///
/// `FFmpeg` 8.0 and 8.1 declare `h264_videotoolbox` with neither
/// `AV_CODEC_CAP_HARDWARE` nor `AV_CODEC_CAP_HYBRID`, unlike
/// `hevc_videotoolbox`, yet opens it with `allow_sw=0`
/// (`RequireHardwareAcceleratedVideoEncoder`), so it runs on the Mac's
/// hardware encoder or not at all. An entry applies only while the runtime
/// declares neither bit: a runtime that declares one is taken at its word,
/// and the entry can go once no supported runtime needs it.
const MISDECLARED_HARDWARE_ENCODERS: &[&str] = &["h264_videotoolbox"];

/// The class of the encoder `name` that declares `capabilities`: the bits it
/// declares, except that an encoder in [`MISDECLARED_HARDWARE_ENCODERS`]
/// declaring neither is `Hardware`.
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

pub(crate) fn encoder_is_compatible(candidate: &VideoEncoderCandidate) -> bool {
    let name = candidate.name();
    registration_is_compatible(candidate, &name) && encoder_runs_here(candidate, &name)
}

/// What the registration alone rules out: an encoder the sink cannot feed
/// its codec's base pictures ([`NativeVideoCodec::base_layout`]: YUV420P but
/// for `ProRes`), and a hardware one whose device has no node here.
fn registration_is_compatible(candidate: &VideoEncoderCandidate, name: &str) -> bool {
    // The native sink submits software frames directly. Hardware wrappers requiring an
    // AVHWFramesContext must not be advertised.
    let codec = candidate.codec();
    if encoder_layout(&candidate.encoder, codec.base_layout()).is_none() {
        return false;
    }
    !candidate.on_hardware || hardware_device_node(name) != Some(false)
}

/// The pictures `encoder` takes for an output of `layout`: `layout` itself,
/// or its `P010` form for an encoder that takes 10-bit 4:2:0 only as that
/// (NVENC); `None` when it takes neither.
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

/// Whether the encoder `name`, run on the GPU when `on_hardware` is set, is
/// listed only after an open probe ([`encoder_runs_here`]): a hardware
/// encoder with no device node to stand for its device, NVENC, and an `_mf`
/// or `_videotoolbox` one either way it runs. A `VideoToolbox` encoder opens
/// only where the Mac has a hardware encoder, and a runtime may declare one
/// software (`FFmpeg` 8.1's `h264_videotoolbox`, which the class corrects),
/// so the name keeps it probed whatever its class. An NVIDIA device node
/// says nothing about NVENC: an A100 or H100 has the node and no encoder.
pub(crate) fn needs_open_probe(on_hardware: bool, name: &str) -> bool {
    (on_hardware && (hardware_device_node(name).is_none() || name.ends_with("_nvenc")))
        || name.ends_with("_mf")
        || name.ends_with("_videotoolbox")
}

/// Whether `encoder` encodes a frame on this machine, for the encoders whose
/// registration does not say.
///
/// Registration says an encoder was compiled in, not that this machine runs
/// it. `MediaFoundation` registers every `_mf` encoder whether or not Windows
/// has a transform that runs it: on one runner `hevc_mf` found the HEVC
/// Video Extension, opened, and failed its first frame with `E_FAIL` in
/// every rate mode, and `av1_mf` found no transform at all. A hardware
/// encoder is the same claim about a device: `h264_videotoolbox` registers in
/// a headless macOS VM with no encoder to open, and NVENC on a Windows
/// machine without the driver. Advertised on registration alone, they would
/// be preferred and exports could only fall through. So an `_mf` or
/// `_videotoolbox` encoder either way it runs, and a hardware one with no
/// device node to stand for its device ([`hardware_device_node`]), is
/// listed only after it has encoded one 256x144 frame with the options an
/// export sends, once per process for each way it runs (a machine with no
/// hardware transform still runs the software one); a failure is logged once.
/// Any other software encoder is taken at its registration, and any failure
/// when an export opens a listed encoder falls through to the next candidate.
///
/// Probes run when an encoder is first asked about, not at startup: an
/// export asks only about its own codec's encoders, and a GPU-resident one
/// about none. Each NVENC probe makes a CUDA context, which took about 0.4 s
/// on an RTX 4090.
fn encoder_runs_here(candidate: &VideoEncoderCandidate, name: &str) -> bool {
    probe_outcome(candidate, name, true).unwrap_or(false)
}

/// Whether `candidate` runs here: `Some(true)` for an encoder that needs no
/// probe or passed one, `Some(false)` for one that failed it, and `None` for
/// one not probed yet when `probe` is unset.
fn probe_outcome(candidate: &VideoEncoderCandidate, name: &str, probe: bool) -> Option<bool> {
    static RUNS: LazyLock<Mutex<HashMap<(String, bool), bool>>> = LazyLock::new(Mutex::default);
    if !needs_open_probe(candidate.on_hardware, name) {
        return Some(true);
    }
    let key = (name.to_owned(), candidate.on_hardware);
    let mut runs = RUNS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(runs_here) = runs.get(&key) {
        return Some(*runs_here);
    }
    if !probe {
        return None;
    }
    let runs_here = match probe_software_encode(candidate, name, 256, 144, None).0 {
        None => true,
        Some(error) => {
            alpha2_os::diagnostic!(
                "alpha2 native: video encoder {name} {} is registered but did not encode a \
                 frame on this machine, so it is not offered that way: {error}",
                mode_label(candidate.on_hardware)
            );
            false
        }
    };
    runs.insert(key, runs_here);
    Some(runs_here)
}

/// The encoders [`codec_capabilities`] lists, found without opening any:
/// what it would list from what is known so far, and the encoders whose open
/// probe has not run yet, which it leaves out.
///
/// For a log line at startup, which must not pay for every probe
/// ([`encoder_runs_here`]).
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
                Some(true) => {
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
                Some(false) => {}
                None => unprobed.push(format!("{name} {}", mode_label(candidate.on_hardware))),
            }
        }
    }
    (listed, unprobed)
}

/// How a log line names the way a candidate runs.
pub(crate) const fn mode_label(on_hardware: bool) -> &'static str {
    if on_hardware { "on GPU" } else { "on CPU" }
}

/// Whether the device a hardware encoder needs has a node here, for the
/// encoder families Linux knows a node for; `None` for the rest.
///
/// A node is the evidence Linux takes for those families instead of an open
/// probe ([`encoder_runs_here`]), except NVENC, whose node is only the first
/// requirement: without one, nothing is probed, so a machine without an
/// NVIDIA GPU pays nothing for the NVENC encoders the runtime registers. A
/// node whose device still does not open costs the export one failed open,
/// and the encoder it falls through to gets the thread split for its own
/// kind (software or hardware).
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

/// Off Linux there is no device node to look at: whether the device is there
/// is the open probe's to answer ([`encoder_runs_here`]).
#[cfg(not(target_os = "linux"))]
const fn hardware_device_node(_name: &str) -> Option<bool> {
    None
}

/// Why an export of `codec` has no encoder, and what to do about it.
///
/// "h264 encoder is unavailable" reads like a bug in alpha2. What decides is
/// the GPU-resident path, which says why it declined (`gpu_output_unavailable`,
/// when the caller knows), the linked runtime, which says what it offers
/// instead (`offered`, every codec's encoders), and for H.264 whether
/// `OpenH264` is installed. The message names all three and the ways out.
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

/// What a user can do on a machine that has no H.264 encoder for CPU
/// frames: install `OpenH264` (or turn it back on), bring an `FFmpeg` with
/// `libx264`, or export another codec.
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
        "{openh264}, use a thin runtime with an FFmpeg 8 that has libx264, or export VP9 (.webm) \
         or AV1 instead"
    )
}

/// [`no_video_encoder_message`] as an encode error, for a sink that does not
/// know why the GPU path was not taken.
fn unavailable_codec_error(codec: NativeVideoCodec) -> EncodeError {
    EncodeError::Adapter(no_video_encoder_message(
        codec,
        &codec_capabilities().video,
        None,
    ))
}

/// Encoders to try for `codec`, most preferred first.
///
/// A named request is that encoder alone: a caller that pinned an encoder
/// wants that encoder or an error, not a quiet substitution. A hybrid
/// encoder is still tried each way it runs here.
pub(crate) fn video_encoder_candidates(
    codec: NativeVideoCodec,
    requested_name: Option<&str>,
) -> Result<Vec<VideoEncoderChoice>, EncodeError> {
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
            Some(library) => Ok(vec![VideoEncoderChoice::OpenH264(std::sync::Arc::clone(
                library,
            ))]),
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
        return Ok(registered_video_encoders(codec)
            .into_iter()
            .filter(|candidate| candidate.encoder.as_ptr() == encoder.as_ptr())
            .filter(encoder_is_compatible)
            .map(VideoEncoderChoice::Ffmpeg)
            .collect());
    }
    let candidates = available_video_encoders(codec);
    if candidates.is_empty() {
        return Err(unavailable_codec_error(codec));
    }
    Ok(candidates)
}

/// The one encoder an export of `codec` opens without a fallback: the pinned
/// `requested_name`, which must implement `codec` and be advertised for the
/// YUV420P sink, else the codec's preferred advertised encoder.
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
        if !video_encoder_capabilities(codec)
            .iter()
            .any(|capability| capability.encoder_name == name)
        {
            return Err(EncodeError::Adapter(format!(
                "video encoder {name} is not compatible with the native encode path"
            )));
        }
        return Ok(encoder);
    }

    let capability = video_encoder_capabilities(codec)
        .into_iter()
        .find(|capability| capability.library.is_none())
        .ok_or_else(|| unavailable_codec_error(codec))?;
    let name = CString::new(capability.encoder_name)
        .map_err(|_| EncodeError::Adapter("video encoder name contains a NUL byte".into()))?;
    AVCodec::find_encoder_by_name(&name).ok_or_else(|| unavailable_codec_error(codec))
}

/// What one sink needs to open its video encoder.
pub(crate) struct VideoEncoderOpenRequest<'a> {
    /// Output format the encoder is configured for.
    pub config: &'a VideoEncodeConfig,
    /// Concrete encoder the caller pinned, if any.
    pub requested_name: Option<&'a str>,
    /// Software worker budget, asked for only when a candidate that runs on
    /// the CPU is opened; a hardware candidate takes none.
    pub threads: SoftwareEncoderThreads<'a>,
    /// Whether the container wants the extradata in the stream header.
    pub global_header: bool,
    /// Coded size, already range-checked by the caller.
    pub width: i32,
    /// Coded size, already range-checked by the caller.
    pub height: i32,
}

/// The worker count a software video encoder opens with, decided only when
/// one is opened.
///
/// An export divides its CPU between decode and the encoder, and which way it
/// divides depends on whether the encoder runs on the CPU at all. That is
/// known only once a candidate opens: the preferred one can be a hardware
/// encoder that fails `avcodec_open2` and falls through to a software one.
/// A count decided before the open would give that fallback the hardware
/// split's share of the encoder, which is none, and it would run on one
/// worker. The caller passes how to decide instead; it is asked at most once.
pub(crate) struct SoftwareEncoderThreads<'a> {
    decide: &'a dyn Fn() -> i64,
    decided: std::cell::OnceCell<i64>,
}

impl<'a> SoftwareEncoderThreads<'a> {
    /// The count `decide` returns, asked for when a software candidate opens.
    pub(crate) fn new(decide: &'a dyn Fn() -> i64) -> Self {
        Self {
            decide,
            decided: std::cell::OnceCell::new(),
        }
    }

    /// The decided count, at least one worker.
    pub(crate) fn get(&self) -> i64 {
        *self.decided.get_or_init(|| (self.decide)().max(1))
    }
}

/// Opens the first candidate encoder that this machine can actually run.
///
/// `avcodec_open2` is where a registered encoder becomes a running one, and
/// the two are not the same claim: a Windows SKU or VM without the
/// `MediaFoundation` H.264 MFT registers `h264_mf` and cannot open it, and a
/// headless container registers `h264_videotoolbox` and cannot open it. Each
/// rejection is recorded with a typed reason and carried on the sink so the
/// run can report which encoder it ended up on.
///
/// # Errors
/// Returns the last candidate's failure, prefixed with every earlier one, when
/// no candidate opens.
pub(crate) fn open_video_encoder(
    request: &VideoEncoderOpenRequest<'_>,
) -> Result<
    (
        crate::video_backend::VideoEncoderBackend,
        NativeVideoEncoderSelection,
    ),
    EncodeError,
> {
    let candidates = video_encoder_candidates(request.config.codec, request.requested_name)?;
    open_first_video_encoder(request, candidates)
}

/// An opened encoder, how it runs the rate control, and its CPU workers.
struct OpenedVideoEncoder {
    backend: crate::video_backend::VideoEncoderBackend,
    rate_control: crate::AppliedRateControl,
    quantizer_refused: Option<String>,
    software_threads: Option<i64>,
    pixels: alpha2_media::PixelLayout,
}

/// Opens the first of `candidates` that runs, recording why each one before
/// it did not; when none does, the error names every candidate's reason.
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
                let detail = error.to_string();
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
                    return Err(EncodeError::Adapter(format!(
                        "{prefix}{name} {} ({}): {detail}",
                        mode_label(candidate.on_hardware()),
                        reason.as_str()
                    )));
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
                    detail,
                });
            }
        }
    }
    Err(unavailable_codec_error(request.config.codec))
}

/// Opens `OpenH264` for `request`: on its constant QP when the rate control
/// has a quantizer, which `OpenH264` always honours, else on its bitrate.
///
/// Its thread count comes from the picture height
/// (`openh264::encoder::slice_count`), not from the CPU budget. The software
/// budget is still decided here, since the rest of the export divides the
/// CPU from that decision, but the encoder does not run with it.
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
    let _ = request.threads.get();
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

/// An opened encoder context, how it runs the rate control, why a quantizer
/// asked for was refused, and the pictures it takes.
type OpenedContext = (
    EncoderContext,
    crate::AppliedRateControl,
    Option<String>,
    alpha2_media::PixelLayout,
);

/// Opens `candidate`, on the rate control's quantizer when it has one and the
/// encoder accepts it, otherwise on its bitrate.
///
/// A refused quantizer is not a refused encoder: the bitrate is the rate
/// control's own fallback, so the export continues on it and the selection
/// says so rather than failing over to another candidate.
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
                    return Ok((encoder, crate::AppliedRateControl::Quantizer, None, pixels));
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
    // Square pixels are what an undeclared ratio means; only another is written.
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
    // A hardware candidate never asks, so an export whose GPU encoder opens never decides a
    // software split it would not use.
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

/// Frames between forced keyframes: two seconds of output.
///
/// libavcodec's own `gop_size` default is 12, which at 24 fps is a keyframe
/// every half second. Nothing in alpha2 seeks or thumbnails its own output, so the
/// interval is only a rate question. Measured at a fixed rate on the 4K
/// showcase, 12 s of the colour clip, PSNR-Y: going from 12 to two seconds
/// (48) gained 0.53-0.64 dB on `h264_vulkan` and 0.67-0.77 dB on
/// `h264_nvenc`; going on to ten seconds (240) gained a further 0.11-0.12 dB
/// on `h264_vulkan`. Two seconds is where nearly all of that is and a player
/// can still start or seek within two seconds of any point.
pub(crate) fn keyframe_interval(frame_rate: alpha2_time::FrameRate) -> i32 {
    let frames = (2 * u64::from(frame_rate.numerator()))
        .div_ceil(u64::from(frame_rate.denominator()))
        .max(1);
    i32::try_from(frames).unwrap_or(i32::MAX)
}

/// The largest time base denominator encoders of `codec` can write, `None`
/// for a codec without one. MPEG-4 Part 2 and H.263 store it as the 16-bit
/// `vop_time_increment_resolution`, so `FFmpeg`'s encoders refuse a time base
/// of 1001/120000 (119.88 fps).
pub(crate) const fn max_time_base_denominator(codec: ffi::AVCodecID) -> Option<u32> {
    match codec {
        ffi::AV_CODEC_ID_MPEG4 | ffi::AV_CODEC_ID_H263 | ffi::AV_CODEC_ID_H263P => Some(65_535),
        _ => None,
    }
}

/// The time base an encoder of frames at `rate` runs in: the frame length
/// itself (1001/30000 for 29.97 fps), when its denominator is within `limit`.
/// Otherwise 1/D for the D within `limit` that rounds frame times the least
/// (the largest error over every frame), so a rate a time base of this
/// encoder cannot hold keeps its frame count and frame times, each rounded
/// by under one unit. 119.88 fps (120000/1001) is 1/64000: frame times are
/// off by at most 7/960000 s.
///
/// # Errors
/// For a rate whose terms do not fit an `FFmpeg` rational.
pub(crate) fn encoder_time_base(
    rate: alpha2_time::FrameRate,
    limit: Option<u32>,
) -> Result<AVRational, EncodeError> {
    let out_of_range = || EncodeError::Adapter("frame rate is out of range".into());
    let (num, den) = (u64::from(rate.numerator()), u64::from(rate.denominator()));
    let exact = || {
        Ok(rsmpeg::avutil::ra(
            i32::try_from(den).map_err(|_| out_of_range())?,
            i32::try_from(num).map_err(|_| out_of_range())?,
        ))
    };
    let Some(limit) = limit.map(u64::from).filter(|limit| num > *limit) else {
        return exact();
    };
    // Frame k is at k*den/num s, k*den*D/num units of 1/D. Its fractional part is a multiple of
    // 1/q, q = num / gcd(num, den*D), so the largest rounding is floor(q/2)/q units,
    // floor(q/2)/(q*D) s.
    // A unit no longer than a frame keeps every frame its own time.
    let mut best: Option<(u64, u64, u64)> = None;
    for units in num.div_ceil(den).max(1)..=limit {
        let q = num / gcd_u64(num, den * units);
        let (error, scale) = (q / 2, q * units);
        if best.is_none_or(|(_, best_error, best_scale)| {
            u128::from(error) * u128::from(best_scale) < u128::from(best_error) * u128::from(scale)
        }) {
            best = Some((units, error, scale));
        }
    }
    let (units, _, _) = best.ok_or_else(out_of_range)?;
    Ok(rsmpeg::avutil::ra(
        1,
        i32::try_from(units).map_err(|_| out_of_range())?,
    ))
}

const fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// `max_b_frames` for encoder `name`.
pub(crate) fn encoder_max_b_frames(name: &str) -> i32 {
    // VideoToolbox's default is no frame reordering. Forcing B-frames makes support
    // device-dependent and can yield invalid packet timestamps on otherwise valid hardware
    // sessions, so preserve that native default.
    if name.ends_with("_videotoolbox") {
        0
    } else {
        2
    }
}

/// The smallest even width and height encoder `name` accepts.
pub(crate) fn encoder_minimum_dimensions(name: &str) -> (u32, u32) {
    // NVIDIA AV1 requires at least 145x128. Its H.264/HEVC encoders reject dimensions below 145x49.
    // alpha2 exposes even 4:2:0 dimensions.
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

/// What one registered video encoder declares, and what happened when alpha2
/// opened it on this machine.
///
/// Registration, advertisement and instantiation are three different claims,
/// and only the third is the one an export depends on. This probe makes all
/// three observable in one place, per platform, without an export.
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
    /// Whether every declared format is a hardware one, so a software frame
    /// can never be sent to it. This is what keeps `h264_vulkan` unadvertised.
    pub requires_hardware_frames: bool,
    /// Whether [`codec_capabilities`] advertises it for the YUV420P sink.
    pub advertised: bool,
    /// Smallest frame the encoder accepts, as alpha2 records it.
    pub minimum_size: (u32, u32),
    /// `None` when the encoder opened and produced at least one packet from a
    /// software YUV420P frame; otherwise why it did not.
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
    /// The options that carried it, or `None` where alpha2 knows no spelling for
    /// it on this encoder's family.
    pub options: Option<String>,
    /// Packets drained when it opened, consumed every option and encoded a
    /// frame; otherwise why not.
    pub outcome: Result<u32, String>,
    /// The device a hardware-frames encoder was probed on: a Vulkan device
    /// index for `*_vulkan`, and for `*_d3d12va` the DX12 adapter that
    /// answered (`DX12 adapter <index> (<name>)`).
    pub device: Option<String>,
}

/// A quantizer every encoder of `codec` accepts, for a probe open: the
/// software VP9 and AV1 encoders (`libvpx-vp9`, `libsvtav1`) take `crf` on
/// 0-63, the hardware ones a QP on 0-255.
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
/// The name of an encoder says nothing about whether this machine can run it:
/// a Windows SKU without the `MediaFoundation` H.264 MFT registers `h264_mf`
/// and cannot open it, and `h264_vulkan` is registered on every Linux build
/// and cannot take a software frame at all. Each candidate is opened at
/// `width`x`height` and fed one YUV420P frame, so the report distinguishes
/// "registered", "advertised" and "ran". A hybrid encoder is a row for each
/// way it runs here.
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
/// `h264_vulkan` is the shipped Linux encoder and cannot take a software
/// frame at all, so its rate-control surface is only observable through a
/// device. `FFmpeg` creates its own Vulkan device here, with whatever
/// video-encode queue the physical device offers, the same capability the
/// export's shared device is checked for.
///
/// The D3D12VA encoders are Windows' GPU-path encoders and take D3D12
/// frames only. Their rows open on a DX12 adapter's device, the first one
/// whose D3D12 Video Encode reports the encoder, and a mode that device does
/// not list is refused with its reason rather than opened.
///
/// Returns the packets and the device that answered: a Vulkan device's
/// index, or a DX12 adapter's index and name.
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
