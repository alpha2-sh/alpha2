//! Native export frame contract shared by software and hardware decoders.

use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;

use alpha2_media::{
    CancelToken, ChromaLocation, ColorMetadata, DecodeError, DecodeEvent, DecodeRequest,
    DecodedVideoFrame, Decoder, Orientation, PixelFormat, Rect, SampleAspectRatio, StreamInfo,
};
use alpha2_time::MediaTime;

use crate::NativeDecoder;
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
use crate::convert::timestamp;
use crate::decode::DecodeStageTimings;
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
use alpha2_native_gpu::SharedD3D12Context;
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
use alpha2_native_gpu::SharedMetalContext;
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
use alpha2_native_gpu::SharedVulkanContext;
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
use rsmpeg::{avutil::AVFrame, ffi};

/// Why a video source is decoded in software instead of on the GPU.
///
/// Recorded per source so a run can report what prevented the hardware path
/// rather than leaving an unexplained software fallback.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HardwareDecodeFallback {
    /// No shared GPU decode context was created for this run.
    ContextUnavailable,
    /// The codec exposes no hardware configuration for this backend.
    NoHardwareConfig,
    /// The stream declares a color space the hardware path does not present.
    UnsupportedColor,
    /// The stream's chroma subsampling or bit depth has no resident path on
    /// this device, such as 4:2:2, 4:4:4, or 10-bit without P010 support.
    UnsupportedProfile,
    /// The decoder produced a surface format that cannot be imported.
    UnsupportedSurfaceFormat,
    /// The container or stream could not be opened at all.
    SourceUnavailable,
    /// The hardware decoder rejected this stream while opening.
    DecoderOpenFailed,
    /// Reverse playback keeps this cursor on the software decoder.
    ReverseWindow,
    /// The stream's SPS crops its left or top edge, which this backend's
    /// surfaces do not show: `VideoToolbox` hands back the coded picture's
    /// top-left corner with the crop cleared. A D3D12VA picture that reaches
    /// past its surface is refused the same way.
    UnsupportedCrop,
    /// The renderer could not import a decoded surface, so its pixels were
    /// downloaded and uploaded from the CPU instead.
    ImportFailed,
    /// The stream carries alpha, which a hardware session would drop: VP9's
    /// block additions, an HEVC auxiliary layer, or an alpha plane.
    AlphaChannel,
}

impl HardwareDecodeFallback {
    /// Stable key used when reporting fallback counts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextUnavailable => "contextUnavailable",
            Self::NoHardwareConfig => "noHardwareConfig",
            Self::UnsupportedColor => "unsupportedColor",
            Self::UnsupportedProfile => "unsupportedProfile",
            Self::UnsupportedSurfaceFormat => "unsupportedSurfaceFormat",
            Self::SourceUnavailable => "sourceUnavailable",
            Self::DecoderOpenFailed => "decoderOpenFailed",
            Self::ReverseWindow => "reverseWindow",
            Self::UnsupportedCrop => "unsupportedCrop",
            Self::ImportFailed => "importFailed",
            Self::AlphaChannel => "alphaChannel",
        }
    }
}

/// A hardware decode session that could not be opened, with its typed reason.
#[derive(Debug)]
pub struct HardwareOpenError {
    /// Why the hardware path is unavailable for this source.
    pub reason: HardwareDecodeFallback,
    /// The underlying decode failure.
    pub source: DecodeError,
}

impl HardwareOpenError {
    /// Pairs a decode failure with the reason a caller should record.
    #[must_use]
    pub const fn new(reason: HardwareDecodeFallback, source: DecodeError) -> Self {
        Self { reason, source }
    }
}

impl std::fmt::Display for HardwareOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {:?}", self.reason.as_str(), self.source)
    }
}

impl std::error::Error for HardwareOpenError {}

/// One backend's hardware-decoder negotiation parameters.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
pub(crate) struct HardwareDecoderSpec<'a> {
    /// `FFmpeg` hardware device type to match in the codec's configurations.
    pub device_type: ffi::AVHWDeviceType,
    /// Opaque hardware pixel format the decoder must produce.
    pub pixel_format: ffi::AVPixelFormat,
    /// Backend name used in diagnostics.
    pub backend: &'static str,
    /// Whether the configuration must accept a shared device context.
    pub require_device_context: bool,
    /// Whether the renderer can present 10-bit planes from this backend.
    pub ten_bit: bool,
    /// Whether this backend's frames keep an SPS's left and top crop. `FFmpeg`'s
    /// `VideoToolbox` hwaccel clears every frame's crop, trusting the session
    /// to apply it, but the session only shortens the picture to the visible
    /// size from the right and bottom (H.264) or rescales it (HEVC), so a
    /// left or top crop would show the wrong pixels with nothing to say so.
    /// `FFmpeg`'s D3D12VA hwaccel keeps the crop; 8.1.3 makes its surfaces
    /// and the decoder's output the visible size (`avctx->width`), which would
    /// lose the visible columns and rows past a leading crop, but the session
    /// gives it surfaces of the coded size (`alpha2_d3d12_get_format`).
    pub keeps_leading_crop: bool,
    /// The codecs the device decodes in hardware, by `FFmpeg` name, when the
    /// device lists them; a stream of another codec is refused before a
    /// session opens. `VideoToolbox` needs this: `FFmpeg` asks it for HEVC
    /// only with `EnableHardwareAcceleratedVideoDecoder`, which lets a Mac
    /// without the hardware decoder run the session in software.
    pub hardware_codecs: Option<&'a [&'static str]>,
    /// Callback that selects `pixel_format` during negotiation.
    pub get_format: unsafe extern "C" fn(
        *mut ffi::AVCodecContext,
        *const ffi::AVPixelFormat,
    ) -> ffi::AVPixelFormat,
}

/// Plane organization of a native export frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NativePlaneLayout {
    /// Owned CPU YCbCr planes.
    CpuYuv(alpha2_media::YuvFormat),
    /// Owned CPU RGBA pixels, 8 bits each, straight alpha
    /// ([`PixelFormat::Rgba8`]).
    CpuRgba8,
    /// Owned CPU premultiplied linear-light RGBA, a half float each
    /// ([`PixelFormat::LinearRgbaF16`]).
    CpuLinearRgbaF16,
    /// Three imported R8 GPU planes.
    GpuI420,
    /// Imported R8 luma and RG8 chroma planes.
    GpuNv12,
    /// Imported R16 luma and RG16 chroma planes holding 10 significant bits.
    GpuP010,
}

impl NativePlaneLayout {
    /// Significant bits per component for this layout.
    #[must_use]
    pub const fn bit_depth(self) -> u8 {
        match self {
            Self::CpuRgba8 | Self::GpuI420 | Self::GpuNv12 => 8,
            Self::CpuLinearRgbaF16 => 16,
            Self::CpuYuv(format) => format.bit_depth,
            Self::GpuP010 => 10,
        }
    }

    /// Bytes one coded pixel occupies across every plane of this layout.
    #[cfg_attr(
        not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )),
        allow(dead_code)
    )]
    const fn bytes_per_pixel_numerator(self) -> u64 {
        match self {
            Self::CpuRgba8 => 8,
            Self::CpuLinearRgbaF16 => 16,
            Self::GpuI420 | Self::GpuNv12 => 3,
            Self::GpuP010 => 6,
            // Luma, then two chroma planes sharing 4 >> (shift x + shift y) of its area, in half
            // bytes per sample.
            Self::CpuYuv(format) => {
                format.bytes_per_sample() as u64
                    * (2 + (4 >> (format.chroma_shift_x + format.chroma_shift_y)))
            }
        }
    }
}

/// Maps a hardware surface's software pixel format to its plane layout.
///
/// Returning `None` keeps an unsupported surface from being presented as if it
/// were 4:2:0, so the caller reports a typed fallback instead.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
pub(crate) const fn surface_plane_layout(format: ffi::AVPixelFormat) -> Option<NativePlaneLayout> {
    match format {
        ffi::AV_PIX_FMT_NV12 => Some(NativePlaneLayout::GpuNv12),
        ffi::AV_PIX_FMT_P010LE => Some(NativePlaneLayout::GpuP010),
        _ => None,
    }
}

/// The visible rectangle of a hardware frame of `width` x `height` with
/// `crop` (left, top, right, bottom) still on it, or `None` when that
/// rectangle reaches past the `surface_width` x `surface_height` texture the
/// frame was decoded into.
///
/// `FFmpeg` applies only the right and bottom crop to a hardware frame's size
/// and leaves the left and top crop on the frame, so the rectangle ends where
/// the frame does. A surface smaller than that never held the picture's last
/// columns or rows: `FFmpeg` 8.1.3 alone makes D3D12VA surfaces the visible
/// size, which is too small for a left or top crop. The session gives the
/// decoder surfaces of the coded size (`alpha2_d3d12_get_format`), so this
/// guards against a decoder that makes them smaller all the same.
#[cfg_attr(
    not(all(target_os = "windows", feature = "hwdecode")),
    allow(dead_code)
)]
pub(crate) fn visible_rect_within_surface(
    width: u32,
    height: u32,
    [left, top, right, bottom]: [usize; 4],
    surface_width: u32,
    surface_height: u32,
) -> Option<Rect> {
    let x = u32::try_from(left).ok()?;
    let y = u32::try_from(top).ok()?;
    let visible = Rect {
        x,
        y,
        width: width
            .checked_sub(x)?
            .checked_sub(u32::try_from(right).ok()?)?,
        height: height
            .checked_sub(y)?
            .checked_sub(u32::try_from(bottom).ok()?)?,
    };
    (u64::from(x) + u64::from(visible.width) <= u64::from(surface_width)
        && u64::from(y) + u64::from(visible.height) <= u64::from(surface_height))
    .then_some(visible)
}

/// Platform-neutral metadata carried identically by CPU and hardware frames.
///
/// It says nothing of the memory behind the picture: a hardware surface may
/// be larger than its frame (a 1080p H.264 surface is 1088 rows), by an
/// amount each platform decides, and an importer reports the extent it
/// samples itself.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeVideoFrameMetadata {
    /// Presentation timestamp.
    pub pts: MediaTime,
    /// Decode timestamp when available.
    pub dts: Option<MediaTime>,
    /// Nominal duration when available.
    pub duration: Option<MediaTime>,
    /// Visible crop.
    pub visible_rect: Rect,
    /// How the picture is shown upright.
    pub orientation: Orientation,
    /// Pixel aspect ratio.
    pub sample_aspect_ratio: SampleAspectRatio,
    /// Color declaration.
    pub color: Option<ColorMetadata>,
    /// Chroma sample position.
    pub chroma_location: ChromaLocation,
    /// Significant component bits.
    pub bit_depth: u8,
    /// Memory plane organization.
    pub plane_layout: NativePlaneLayout,
    /// GPU device identity, zero for CPU frames.
    pub device_identity: u64,
    /// Decoder or surface-pool generation.
    pub decoder_generation: u64,
    /// Physical picture identity within the generation.
    pub picture_identity: u64,
}

/// Opaque decoder surface retained until GPU completion.
pub trait HardwareFrameLease: Debug + Send + Sync + 'static {
    /// Approximate allocation size charged to the surface budget.
    fn retained_bytes(&self) -> u64;

    /// Allows the platform interop crate to recover its private lease type.
    fn as_any(&self) -> &dyn Any;

    /// Converts a retained lease for platform-specific import without exposing
    /// the backend type in the common frame enum.
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
impl HardwareFrameLease for alpha2_ffmpeg_bridge::VulkanFrameLease {
    fn retained_bytes(&self) -> u64 {
        self.info().map_or(0, |info| {
            let pixels = u64::try_from(info.width)
                .unwrap_or(0)
                .saturating_mul(u64::try_from(info.height).unwrap_or(0));
            // An unrecognized layout is charged as 8-bit 4:2:0; the frame is rejected before
            // import, so this only bounds the queue briefly.
            let numerator = surface_plane_layout(info.software_format)
                .unwrap_or(NativePlaneLayout::GpuNv12)
                .bytes_per_pixel_numerator();
            pixels.saturating_mul(numerator).div_ceil(2)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
impl HardwareFrameLease for alpha2_ffmpeg_bridge::VideoToolboxFrameLease {
    fn retained_bytes(&self) -> u64 {
        self.info().map_or(0, |info| {
            let pixels = u64::try_from(info.width)
                .unwrap_or(0)
                .saturating_mul(u64::try_from(info.height).unwrap_or(0));
            // An unrecognized layout is charged as 8-bit 4:2:0; the frame is rejected before
            // import, so this only bounds the queue briefly.
            let numerator = surface_plane_layout(info.software_format)
                .unwrap_or(NativePlaneLayout::GpuNv12)
                .bytes_per_pixel_numerator();
            pixels.saturating_mul(numerator).div_ceil(2)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
impl HardwareFrameLease for alpha2_ffmpeg_bridge::D3D12FrameLease {
    fn retained_bytes(&self) -> u64 {
        self.info().map_or(0, |info| {
            let pixels = u64::try_from(info.width)
                .unwrap_or(0)
                .saturating_mul(u64::try_from(info.height).unwrap_or(0));
            // An unrecognized layout is charged as 8-bit 4:2:0; the frame is rejected before
            // import, so this only bounds the queue briefly.
            let numerator = surface_plane_layout(info.software_format)
                .unwrap_or(NativePlaneLayout::GpuNv12)
                .bytes_per_pixel_numerator();
            pixels.saturating_mul(numerator).div_ceil(2)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// One decoded export frame with either owned CPU pixels or an opaque surface.
#[derive(Debug)]
pub enum NativeVideoFrame {
    /// Software-decoded frame.
    Cpu(DecodedVideoFrame),
    /// Hardware-decoded frame whose platform details remain behind its lease.
    Hardware {
        /// Presentation and cache identity.
        metadata: NativeVideoFrameMetadata,
        /// Ref-counted decoder-surface ownership.
        surface: Arc<dyn HardwareFrameLease>,
    },
}

impl NativeVideoFrame {
    /// Presentation timestamp.
    #[must_use]
    pub fn pts(&self) -> MediaTime {
        match self {
            Self::Cpu(frame) => frame.pts,
            Self::Hardware { metadata, .. } => metadata.pts,
        }
    }

    /// Nominal frame duration when known.
    #[must_use]
    pub fn duration(&self) -> Option<MediaTime> {
        match self {
            Self::Cpu(frame) => frame.duration,
            Self::Hardware { metadata, .. } => metadata.duration,
        }
    }

    /// Returns metadata without exposing the backing representation.
    #[must_use]
    pub fn metadata(&self) -> NativeVideoFrameMetadata {
        match self {
            Self::Cpu(frame) => metadata_from_cpu(frame),
            Self::Hardware { metadata, .. } => metadata.clone(),
        }
    }

    /// Whether this frame is backed by a decoder surface rather than CPU pixels.
    #[must_use]
    pub const fn is_hardware(&self) -> bool {
        matches!(self, Self::Hardware { .. })
    }

    /// Returns the software frame when this is the CPU variant.
    #[must_use]
    pub fn as_cpu(&self) -> Option<&DecodedVideoFrame> {
        match self {
            Self::Cpu(frame) => Some(frame),
            Self::Hardware { .. } => None,
        }
    }

    /// A CPU copy of this frame: the frame itself when it already is one, or
    /// the decoder surface's pixels downloaded through `FFmpeg`.
    ///
    /// This is the fallback for a surface the renderer could not import, so
    /// the frame still reaches the export instead of failing it. Timing and
    /// presentation come from this frame's metadata; the pixels go through
    /// the same conversion a software decoder's frames do.
    ///
    /// # Errors
    /// Returns a native decode error when the surface cannot be downloaded or
    /// converted.
    pub fn to_cpu(&self) -> Result<DecodedVideoFrame, DecodeError> {
        let (metadata, surface) = match self {
            Self::Cpu(frame) => return Ok(frame.clone()),
            Self::Hardware { metadata, surface } => (metadata, surface),
        };
        let mut software = download_surface(surface.as_ref())?;
        // In tick units so the timestamp cannot fall out of range; the metadata replaces it below.
        software.set_pts(metadata.pts.ticks());
        let mut scaler = crate::convert::FrameScaler::default();
        // The surface's own siting replaces the conversion's below.
        let mut frame = crate::convert::convert_video_frame(
            &software,
            rsmpeg::ffi::AV_CODEC_ID_NONE,
            crate::convert::tick_base(),
            metadata.color.unwrap_or_default(),
            &mut scaler,
        )?;
        frame.pts = metadata.pts;
        frame.duration = metadata.duration;
        frame.orientation = metadata.orientation;
        frame.sample_aspect_ratio = metadata.sample_aspect_ratio;
        frame.chroma_location = metadata.chroma_location;
        // The conversion keeps the given color unless it repacked RGB under the identity matrix; a
        // surface without one stays without one.
        if metadata.color.is_none() {
            frame.color = None;
        }
        Ok(frame)
    }

    /// Approximate retained bytes for separate CPU and surface budgets.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        match self {
            Self::Cpu(frame) => frame.planes.iter().fold(0_u64, |total, plane| {
                total.saturating_add(u64::try_from(plane.bytes.len()).unwrap_or(u64::MAX))
            }),
            Self::Hardware { surface, .. } => surface.retained_bytes(),
        }
    }
}

/// Downloads a decoder surface of whichever platform lease this build knows.
#[cfg_attr(
    not(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )),
    allow(clippy::unnecessary_wraps)
)]
fn download_surface(
    surface: &dyn HardwareFrameLease,
) -> Result<alpha2_ffmpeg_bridge::DecodedFrame, DecodeError> {
    let any = surface.as_any();
    let downloaded: Option<
        Result<alpha2_ffmpeg_bridge::DecodedFrame, alpha2_ffmpeg_bridge::BridgeError>,
    > = None;
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    let downloaded = downloaded.or_else(|| {
        any.downcast_ref::<alpha2_ffmpeg_bridge::VulkanFrameLease>()
            .map(alpha2_ffmpeg_bridge::VulkanFrameLease::download)
    });
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    let downloaded = downloaded.or_else(|| {
        any.downcast_ref::<alpha2_ffmpeg_bridge::VideoToolboxFrameLease>()
            .map(alpha2_ffmpeg_bridge::VideoToolboxFrameLease::download)
    });
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    let downloaded = downloaded.or_else(|| {
        any.downcast_ref::<alpha2_ffmpeg_bridge::D3D12FrameLease>()
            .map(alpha2_ffmpeg_bridge::D3D12FrameLease::download)
    });
    let _ = any;
    downloaded
        .ok_or_else(|| {
            DecodeError::Unsupported("this hardware surface cannot be downloaded".into())
        })?
        .map_err(|error| DecodeError::Adapter(error.to_string()))
}

/// Export decoder producing [`NativeVideoFrame`]s from one `FFmpeg` session.
///
/// Software and hardware sessions share the same demux, seek, and timing
/// logic ([`NativeDecoder`]); a hardware backend changes only where the
/// frame's pixels live.
pub struct NativeExportDecoder {
    backend: ExportDecoderBackend,
    /// Hardware frames this session has produced.
    hardware_frames: u64,
    /// Why the hardware path gave up, once it has.
    hardware_failure: Option<HardwareDecodeFallback>,
}

enum ExportDecoderBackend {
    Software(NativeDecoder),
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    Vulkan {
        decoder: NativeDecoder,
        generation: u64,
    },
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    VideoToolbox {
        decoder: NativeDecoder,
        generation: u64,
        device_identity: u64,
    },
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    D3D12 {
        decoder: NativeDecoder,
        generation: u64,
        device_identity: u64,
    },
}

impl NativeExportDecoder {
    /// Takes the decode-stage spans accumulated since the last call.
    ///
    /// A hardware session reports no conversion time because it produces a
    /// surface rather than pixels; its cost is in `receive_frame`.
    pub fn take_stage_timings(&mut self) -> DecodeStageTimings {
        match &mut self.backend {
            ExportDecoderBackend::Software(decoder) => decoder.take_stage_timings(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { decoder, .. } => decoder.take_stage_timings(),
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { decoder, .. } => decoder.take_stage_timings(),
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { decoder, .. } => decoder.take_stage_timings(),
        }
    }

    /// Whether this session currently produces device-resident frames.
    #[must_use]
    pub const fn is_hardware(&self) -> bool {
        match &self.backend {
            ExportDecoderBackend::Software(_) => false,
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { .. } => true,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { .. } => true,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { .. } => true,
        }
    }

    /// Opens the selected video stream using the software implementation.
    ///
    /// # Errors
    /// Returns a native decode error if the input or selected stream cannot be opened.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DecodeError> {
        NativeDecoder::open(path).map(|decoder| Self {
            backend: ExportDecoderBackend::Software(decoder),
            hardware_frames: 0,
            hardware_failure: None,
        })
    }

    /// Opens with an explicit codec worker allocation.
    ///
    /// # Errors
    /// Returns a native decode error for invalid worker counts or stream setup failures.
    pub fn open_with_threads(
        path: impl AsRef<std::path::Path>,
        threads: i64,
    ) -> Result<Self, DecodeError> {
        NativeDecoder::open_with_threads(path, threads).map(|decoder| Self {
            backend: ExportDecoderBackend::Software(decoder),
            hardware_frames: 0,
            hardware_failure: None,
        })
    }

    /// Opens a Vulkan hardware decoder on the renderer's logical device.
    ///
    /// # Errors
    /// Returns a native decode error if the stream lacks a compatible Vulkan
    /// hardware configuration or the decoder cannot use the shared device.
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    pub fn open_vulkan(
        path: impl AsRef<std::path::Path>,
        threads: i64,
        context: &SharedVulkanContext,
        generation: u64,
    ) -> Result<Self, HardwareOpenError> {
        let spec = HardwareDecoderSpec {
            device_type: ffi::AV_HWDEVICE_TYPE_VULKAN,
            pixel_format: ffi::AV_PIX_FMT_VULKAN,
            backend: "Vulkan",
            require_device_context: true,
            ten_bit: context.supports_ten_bit(),
            keeps_leading_crop: true,
            // The device was created with the decode extensions it lists; FFmpeg would open a
            // session for any other codec and fail on its first packet.
            hardware_codecs: Some(context.decode_codecs()),
            get_format: alpha2_ffmpeg_bridge::vulkan_get_format_callback(),
        };
        NativeDecoder::open_hardware(path, threads, context.ffmpeg_device(), &spec).map(|decoder| {
            Self {
                backend: ExportDecoderBackend::Vulkan {
                    decoder,
                    generation,
                },
                hardware_frames: 0,
                hardware_failure: None,
            }
        })
    }

    /// Opens a `VideoToolbox` hardware decoder producing `CoreVideo` surfaces.
    ///
    /// # Errors
    /// Returns a native decode error if the selected stream cannot negotiate
    /// `AV_PIX_FMT_VIDEOTOOLBOX` or create a hardware decoder session.
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    pub fn open_videotoolbox(
        path: impl AsRef<std::path::Path>,
        threads: i64,
        context: &SharedMetalContext,
        generation: u64,
    ) -> Result<Self, HardwareOpenError> {
        // VideoToolbox allocates its own decode session rather than adopting the renderer's device;
        // Metal interop happens at import time.
        let mark = alpha2_ffmpeg_bridge::log_mark();
        let device = rsmpeg::avutil::AVHWDeviceContext::create(
            ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            None,
            None,
            0,
        )
        .map_err(crate::convert::adapter_error)
        .map_err(|error| {
            HardwareOpenError::new(HardwareDecodeFallback::DecoderOpenFailed, error)
        })?;
        let spec = HardwareDecoderSpec {
            device_type: ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            pixel_format: ffi::AV_PIX_FMT_VIDEOTOOLBOX,
            backend: "VideoToolbox",
            require_device_context: false,
            ten_bit: context.supports_ten_bit(),
            keeps_leading_crop: false,
            hardware_codecs: Some(context.decode_codecs()),
            get_format: alpha2_ffmpeg_bridge::videotoolbox_get_format_callback(),
        };
        NativeDecoder::open_hardware(path, threads, device, &spec)
            .map(|decoder| Self {
                backend: ExportDecoderBackend::VideoToolbox {
                    decoder,
                    generation,
                    device_identity: context.identity().device,
                },
                hardware_frames: 0,
                hardware_failure: None,
            })
            .map_err(|mut error| {
                error.source = with_videotoolbox_log(error.source, mark);
                error
            })
    }

    /// Opens a D3D12VA hardware decoder on the renderer's D3D12 device.
    ///
    /// # Errors
    /// Returns a native decode error if the stream lacks a compatible D3D12VA
    /// configuration or the shared device cannot create a decode session.
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    pub fn open_d3d12(
        path: impl AsRef<std::path::Path>,
        threads: i64,
        context: &SharedD3D12Context,
        generation: u64,
    ) -> Result<Self, HardwareOpenError> {
        let spec = HardwareDecoderSpec {
            device_type: ffi::AV_HWDEVICE_TYPE_D3D12VA,
            pixel_format: ffi::AV_PIX_FMT_D3D12,
            backend: "D3D12VA",
            require_device_context: true,
            ten_bit: context.supports_ten_bit(),
            keeps_leading_crop: true,
            hardware_codecs: None,
            get_format: alpha2_ffmpeg_bridge::d3d12_get_format_callback(),
        };
        NativeDecoder::open_hardware(path, threads, context.ffmpeg_device(), &spec).map(|decoder| {
            Self {
                backend: ExportDecoderBackend::D3D12 {
                    decoder,
                    generation,
                    device_identity: context.identity().device,
                },
                hardware_frames: 0,
                hardware_failure: None,
            }
        })
    }

    /// Stream descriptions from the common decode session.
    #[must_use]
    pub fn streams(&self) -> &[StreamInfo] {
        match &self.backend {
            ExportDecoderBackend::Software(decoder) => decoder.streams(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { decoder, .. } => decoder.streams(),
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { decoder, .. } => decoder.streams(),
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { decoder, .. } => decoder.streams(),
        }
    }

    /// What the decoder leaves out of its stream ([`crate::NativeDecoder::dropped_layer`]); a
    /// hardware session is never opened for a stream with alpha, so it drops nothing.
    #[must_use]
    pub fn dropped_layer(&self) -> Option<&crate::DroppedLayer> {
        match &self.backend {
            ExportDecoderBackend::Software(decoder) => decoder.dropped_layer(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { .. } => None,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { .. } => None,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { .. } => None,
        }
    }

    /// Effective end of the selected video source.
    #[must_use]
    pub fn video_duration(&self) -> Option<MediaTime> {
        match &self.backend {
            ExportDecoderBackend::Software(decoder) => decoder.video_duration(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { decoder, .. } => decoder.video_duration(),
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { decoder, .. } => decoder.video_duration(),
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { decoder, .. } => decoder.video_duration(),
        }
    }

    /// Resets codec and demux state.
    ///
    /// # Errors
    /// Returns a native decode error if the codec state cannot be reset.
    pub fn flush(&mut self) -> Result<(), DecodeError> {
        match &mut self.backend {
            ExportDecoderBackend::Software(decoder) => decoder.flush(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { decoder, .. } => decoder.flush(),
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { decoder, .. } => decoder.flush(),
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { decoder, .. } => decoder.flush(),
        }
    }

    /// Seeks and returns the selected native frame.
    ///
    /// # Errors
    /// Returns a native decode error for cancellation, seek, decode, or timestamp failures.
    pub fn seek_video(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<NativeVideoFrame, DecodeError> {
        let result = self.seek_video_on_backend(request, cancel);
        self.observe(result, |_| true)
    }

    /// Decodes the next native frame, or `None` at end of stream.
    ///
    /// # Errors
    /// Returns a native decode error for cancellation or codec failures.
    pub fn decode_video_next(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<NativeVideoFrame>, DecodeError> {
        let result = self.decode_video_next_on_backend(cancel);
        self.observe(result, Option::is_some)
    }

    /// When the frame [`Self::decode_video_next`] returns next is
    /// presented, or `None` at the end of the stream.
    ///
    /// The frame is decoded now and kept for that call, so a caller can find
    /// the frame presented at a time (the last one at or before its
    /// [`Self::presentation_deadline`]) without consuming the one after.
    ///
    /// # Errors
    /// Returns a native decode error for cancellation or codec failures.
    pub fn peek_video_pts(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<MediaTime>, DecodeError> {
        let result = self.native_decoder().peek_raw_pts(cancel);
        self.observe(result, |_| false)
    }

    /// Configures corruption handling on the selected native backend.
    pub fn set_decode_policy(&mut self, policy: crate::DecodePolicy) {
        self.native_decoder().set_decode_policy(policy);
    }

    /// Takes the color a project declares for this source, which every frame
    /// then carries ([`NativeDecoder::declare_color`]).
    ///
    /// # Errors
    /// Returns [`HardwareDecodeFallback::UnsupportedColor`] for a hardware
    /// session declared HDR: the hardware path presents SDR only, as it
    /// refuses a stream tagged HDR when it opens.
    pub fn with_declared_color(
        mut self,
        color: Option<ColorMetadata>,
    ) -> Result<Self, HardwareOpenError> {
        if self.is_hardware() && color.is_some_and(ColorMetadata::is_hdr) {
            return Err(HardwareOpenError::new(
                HardwareDecodeFallback::UnsupportedColor,
                DecodeError::Unsupported("hardware export currently supports SDR only".into()),
            ));
        }
        self.native_decoder().declare_color(color);
        Ok(self)
    }

    /// Reports damage before any recovery or terminal error.
    pub fn set_decode_reporter(
        &mut self,
        reporter: impl Fn(crate::DecodeDiagnostic) + Send + Sync + 'static,
    ) {
        self.native_decoder().set_decode_reporter(reporter);
    }

    /// Cumulative corruption diagnostics for this session.
    pub fn decode_diagnostics(&mut self) -> &crate::DecodeDiagnostics {
        self.native_decoder().decode_diagnostics()
    }

    /// The latest timestamp a frame of this stream may carry and still be
    /// the one shown at `time` ([`NativeDecoder::presentation_deadline`]).
    #[must_use]
    pub fn presentation_deadline(&self, time: MediaTime) -> MediaTime {
        match &self.backend {
            ExportDecoderBackend::Software(decoder) => decoder,
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { decoder, .. } => decoder,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { decoder, .. } => decoder,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { decoder, .. } => decoder,
        }
        .presentation_deadline(time)
    }

    /// The `FFmpeg` session behind whichever backend decodes.
    fn native_decoder(&mut self) -> &mut NativeDecoder {
        match &mut self.backend {
            ExportDecoderBackend::Software(decoder) => decoder,
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan { decoder, .. } => decoder,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox { decoder, .. } => decoder,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 { decoder, .. } => decoder,
        }
    }

    /// Why this session's hardware path gave up, once a call has returned
    /// [`DecodeError::Unsupported`] for it.
    #[must_use]
    pub const fn hardware_failure(&self) -> Option<HardwareDecodeFallback> {
        self.hardware_failure
    }

    /// Classifies a hardware session's failures so a caller can fall back.
    ///
    /// `FFmpeg` initializes a hardware decoder lazily, in `get_format` for the
    /// first packet, so a device that cannot decode this profile or codec
    /// (VP9 on a device without VP9 decode, 10-bit H.264 on NVDEC) fails
    /// there with a generic codec error rather than in `avcodec_open2`. Before
    /// the session has produced a frame, such an error is reported as
    /// [`DecodeError::Unsupported`] with [`HardwareDecodeFallback::DecoderOpenFailed`],
    /// which a caller can answer by reopening the source in software. After
    /// the first frame, a codec error is a real failure and stays one.
    fn observe<T>(
        &mut self,
        result: Result<T, DecodeError>,
        produced: impl Fn(&T) -> bool,
    ) -> Result<T, DecodeError> {
        if !self.is_hardware() {
            return result;
        }
        match result {
            Ok(value) => {
                if produced(&value) {
                    self.hardware_frames = self.hardware_frames.saturating_add(1);
                }
                Ok(value)
            }
            Err(DecodeError::Adapter(message)) if self.hardware_frames == 0 => {
                self.hardware_failure = Some(HardwareDecodeFallback::DecoderOpenFailed);
                Err(DecodeError::Unsupported(format!(
                    "the hardware decoder failed before its first frame: {message}"
                )))
            }
            Err(DecodeError::Unsupported(message)) => {
                self.hardware_failure
                    .get_or_insert(HardwareDecodeFallback::UnsupportedSurfaceFormat);
                Err(DecodeError::Unsupported(message))
            }
            Err(error) => Err(error),
        }
    }

    fn seek_video_on_backend(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<NativeVideoFrame, DecodeError> {
        match &mut self.backend {
            ExportDecoderBackend::Software(decoder) => match decoder.seek(request, cancel)?.event {
                DecodeEvent::Video(frame) => Ok(NativeVideoFrame::Cpu(frame)),
                _ => Err(DecodeError::Adapter(
                    "video decoder returned a non-video event".into(),
                )),
            },
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan {
                decoder,
                generation,
            } => {
                let frame = decoder.seek_raw(request.time, cancel)?;
                hardware_frame(decoder, frame.into_frame(), *generation)
            }
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox {
                decoder,
                generation,
                device_identity,
            } => {
                let mark = alpha2_ffmpeg_bridge::log_mark();
                let frame = decoder
                    .seek_raw(request.time, cancel)
                    .map_err(|error| with_videotoolbox_log(error, mark))?;
                videotoolbox_frame(decoder, frame.into_frame(), *generation, *device_identity)
            }
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 {
                decoder,
                generation,
                device_identity,
            } => {
                let frame = decoder.seek_raw(request.time, cancel)?;
                d3d12_frame(
                    decoder,
                    frame.into_frame(),
                    *generation,
                    *device_identity,
                    &mut self.hardware_failure,
                )
            }
        }
    }

    fn decode_video_next_on_backend(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<NativeVideoFrame>, DecodeError> {
        match &mut self.backend {
            ExportDecoderBackend::Software(decoder) => match decoder.decode_next(cancel)? {
                DecodeEvent::Video(frame) => Ok(Some(NativeVideoFrame::Cpu(frame))),
                DecodeEvent::EndOfStream => Ok(None),
                _ => Err(DecodeError::Adapter(
                    "video decoder returned a non-video event".into(),
                )),
            },
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            ExportDecoderBackend::Vulkan {
                decoder,
                generation,
            } => decoder
                .decode_raw_next(cancel)?
                .map(|frame| hardware_frame(decoder, frame.into_frame(), *generation))
                .transpose(),
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            ExportDecoderBackend::VideoToolbox {
                decoder,
                generation,
                device_identity,
            } => {
                let mark = alpha2_ffmpeg_bridge::log_mark();
                decoder
                    .decode_raw_next(cancel)
                    .map_err(|error| with_videotoolbox_log(error, mark))?
                    .map(|frame| {
                        videotoolbox_frame(
                            decoder,
                            frame.into_frame(),
                            *generation,
                            *device_identity,
                        )
                    })
                    .transpose()
            }
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            ExportDecoderBackend::D3D12 {
                decoder,
                generation,
                device_identity,
            } => decoder
                .decode_raw_next(cancel)?
                .map(|frame| {
                    d3d12_frame(
                        decoder,
                        frame.into_frame(),
                        *generation,
                        *device_identity,
                        &mut self.hardware_failure,
                    )
                })
                .transpose(),
        }
    }
}

/// A decoded D3D12VA frame as a [`NativeVideoFrame`]. A picture that reaches
/// past its surface ([`visible_rect_within_surface`]) is refused as
/// [`DecodeError::Unsupported`] with `failure` set to
/// [`HardwareDecodeFallback::UnsupportedCrop`], so the source goes on in
/// software: neither an import nor a download of that surface can show the
/// columns or rows it never held.
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
fn d3d12_frame(
    decoder: &NativeDecoder,
    frame: AVFrame,
    generation: u64,
    device_identity: u64,
    failure: &mut Option<HardwareDecodeFallback>,
) -> Result<NativeVideoFrame, DecodeError> {
    let info = alpha2_ffmpeg_bridge::d3d12_frame_info(&frame)
        .map_err(|error| DecodeError::Adapter(error.to_string()))?;
    let plane_layout = surface_plane_layout(info.software_format).ok_or_else(|| {
        DecodeError::Unsupported(format!(
            "D3D12VA output {} is not an importable 4:2:0 surface",
            info.software_format
        ))
    })?;
    let width = u32::try_from(frame.width).map_err(|_| DecodeError::OutOfRange)?;
    let height = u32::try_from(frame.height).map_err(|_| DecodeError::OutOfRange)?;
    let crop = [
        frame.crop_left,
        frame.crop_top,
        frame.crop_right,
        frame.crop_bottom,
    ];
    let Some(visible_rect) =
        visible_rect_within_surface(width, height, crop, info.surface_width, info.surface_height)
    else {
        *failure = Some(HardwareDecodeFallback::UnsupportedCrop);
        return Err(DecodeError::Unsupported(format!(
            "D3D12VA decoded a {width}x{height} picture cropped {} columns from the left and {} rows from the top into a {}x{} surface, which does not hold it",
            frame.crop_left, frame.crop_top, info.surface_width, info.surface_height
        )));
    };
    let raw_pts = if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
        frame.pts
    } else {
        frame.best_effort_timestamp
    };
    let pts = timestamp(raw_pts, decoder.time_base).ok_or(DecodeError::OutOfRange)?;
    let metadata = NativeVideoFrameMetadata {
        pts,
        dts: None,
        duration: timestamp(frame.duration, decoder.time_base),
        visible_rect,
        orientation: decoder.orientation,
        sample_aspect_ratio: decoder.picture_sample_aspect_ratio(&frame),
        color: decoder.picture_color(&frame),
        chroma_location: decoder.picture_chroma_location(&frame),
        bit_depth: plane_layout.bit_depth(),
        plane_layout,
        device_identity,
        decoder_generation: generation,
        picture_identity: info.resource
            ^ info.fence_value
            ^ u64::from_ne_bytes(pts.ticks().to_ne_bytes()),
    };
    Ok(NativeVideoFrame::Hardware {
        metadata,
        surface: Arc::new(alpha2_ffmpeg_bridge::D3D12FrameLease::new(frame)),
    })
}

/// The logging classes of a `VideoToolbox` decode: the decoder (`FFmpeg`'s
/// hwaccel logs the session's `OSStatus` through the codec context, named
/// after the codec) and the hardware device and frames contexts.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
const VIDEOTOOLBOX_DECODE_LOG_CLASSES: &[&str] = &[
    "h264",
    "hevc",
    "vp9",
    "av1",
    "AVHWDeviceContext",
    "AVHWFramesContext",
];

/// `error` with the `VideoToolbox` lines `FFmpeg` logged since `mark`
/// appended, so the `OSStatus` the session failed with reaches the typed
/// error. Errors without a message are returned as they are.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
fn with_videotoolbox_log(error: DecodeError, mark: alpha2_ffmpeg_bridge::LogMark) -> DecodeError {
    let detail = alpha2_ffmpeg_bridge::log_detail(&alpha2_ffmpeg_bridge::log_since(
        mark,
        VIDEOTOOLBOX_DECODE_LOG_CLASSES,
    ));
    if detail.is_empty() {
        return error;
    }
    match error {
        DecodeError::Adapter(message) => DecodeError::Adapter(format!("{message}{detail}")),
        DecodeError::Unsupported(message) => DecodeError::Unsupported(format!("{message}{detail}")),
        DecodeError::Gpu { fault, message } => DecodeError::Gpu {
            fault,
            message: format!("{message}{detail}"),
        },
        other => other,
    }
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
fn videotoolbox_frame(
    decoder: &NativeDecoder,
    frame: AVFrame,
    generation: u64,
    device_identity: u64,
) -> Result<NativeVideoFrame, DecodeError> {
    let info = alpha2_ffmpeg_bridge::videotoolbox_frame_info(&frame)
        .map_err(|error| DecodeError::Adapter(error.to_string()))?;
    let plane_layout = surface_plane_layout(info.software_format).ok_or_else(|| {
        DecodeError::Unsupported(format!(
            "VideoToolbox output {} is not an importable 4:2:0 surface",
            info.software_format
        ))
    })?;
    let width = u32::try_from(info.width).map_err(|_| DecodeError::OutOfRange)?;
    let height = u32::try_from(info.height).map_err(|_| DecodeError::OutOfRange)?;
    let raw_pts = if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
        frame.pts
    } else {
        frame.best_effort_timestamp
    };
    let pts = timestamp(raw_pts, decoder.time_base).ok_or(DecodeError::OutOfRange)?;
    let metadata = NativeVideoFrameMetadata {
        pts,
        dts: None,
        duration: timestamp(frame.duration, decoder.time_base),
        visible_rect: Rect {
            x: u32::try_from(frame.crop_left).unwrap_or(0),
            y: u32::try_from(frame.crop_top).unwrap_or(0),
            width: width.saturating_sub(
                u32::try_from(frame.crop_left.saturating_add(frame.crop_right)).unwrap_or(0),
            ),
            height: height.saturating_sub(
                u32::try_from(frame.crop_top.saturating_add(frame.crop_bottom)).unwrap_or(0),
            ),
        },
        orientation: decoder.orientation,
        sample_aspect_ratio: decoder.picture_sample_aspect_ratio(&frame),
        color: decoder.picture_color(&frame),
        chroma_location: decoder.picture_chroma_location(&frame),
        bit_depth: plane_layout.bit_depth(),
        plane_layout,
        device_identity,
        decoder_generation: generation,
        picture_identity: info.pixel_buffer ^ u64::from_ne_bytes(pts.ticks().to_ne_bytes()),
    };
    Ok(NativeVideoFrame::Hardware {
        metadata,
        surface: Arc::new(alpha2_ffmpeg_bridge::VideoToolboxFrameLease::new(frame)),
    })
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
fn hardware_frame(
    decoder: &NativeDecoder,
    frame: AVFrame,
    generation: u64,
) -> Result<NativeVideoFrame, DecodeError> {
    let info = alpha2_ffmpeg_bridge::vulkan_frame_info(&frame)
        .map_err(|error| DecodeError::Adapter(error.to_string()))?;
    let plane_layout = surface_plane_layout(info.software_format).ok_or_else(|| {
        DecodeError::Unsupported(format!(
            "Vulkan output {} is not an importable 4:2:0 surface",
            info.software_format
        ))
    })?;
    // For hardware formats `av_frame_apply_cropping` only subtracts the right and bottom crop; the
    // surface keeps the padded coded extent, so the visible rectangle sits inside it.
    let width = u32::try_from(frame.width).map_err(|_| DecodeError::OutOfRange)?;
    let height = u32::try_from(frame.height).map_err(|_| DecodeError::OutOfRange)?;
    let coded_width = u32::try_from(info.coded_width).map_err(|_| DecodeError::OutOfRange)?;
    let coded_height = u32::try_from(info.coded_height).map_err(|_| DecodeError::OutOfRange)?;
    if coded_width < width || coded_height < height {
        return Err(DecodeError::Adapter(
            "Vulkan frame is larger than its allocated surface".into(),
        ));
    }
    let raw_pts = if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
        frame.pts
    } else {
        frame.best_effort_timestamp
    };
    let pts = timestamp(raw_pts, decoder.time_base).ok_or(DecodeError::OutOfRange)?;
    let metadata = NativeVideoFrameMetadata {
        pts,
        dts: None,
        duration: timestamp(frame.duration, decoder.time_base),
        visible_rect: Rect {
            x: u32::try_from(frame.crop_left).unwrap_or(0),
            y: u32::try_from(frame.crop_top).unwrap_or(0),
            width: width.saturating_sub(
                u32::try_from(frame.crop_left.saturating_add(frame.crop_right)).unwrap_or(0),
            ),
            height: height.saturating_sub(
                u32::try_from(frame.crop_top.saturating_add(frame.crop_bottom)).unwrap_or(0),
            ),
        },
        orientation: decoder.orientation,
        sample_aspect_ratio: decoder.picture_sample_aspect_ratio(&frame),
        color: decoder.picture_color(&frame),
        chroma_location: decoder.picture_chroma_location(&frame),
        bit_depth: plane_layout.bit_depth(),
        plane_layout,
        device_identity: info.device,
        decoder_generation: generation,
        picture_identity: info.images[0] ^ u64::from_ne_bytes(pts.ticks().to_ne_bytes()),
    };
    Ok(NativeVideoFrame::Hardware {
        metadata,
        surface: Arc::new(alpha2_ffmpeg_bridge::VulkanFrameLease::new(frame)),
    })
}

fn metadata_from_cpu(frame: &DecodedVideoFrame) -> NativeVideoFrameMetadata {
    let plane_layout = match frame.pixel_format {
        PixelFormat::Yuv(format) => NativePlaneLayout::CpuYuv(format),
        PixelFormat::LinearRgbaF16 => NativePlaneLayout::CpuLinearRgbaF16,
        _ => NativePlaneLayout::CpuRgba8,
    };
    NativeVideoFrameMetadata {
        pts: frame.pts,
        dts: frame.dts,
        duration: frame.duration,
        visible_rect: frame.visible_rect,
        orientation: frame.orientation,
        sample_aspect_ratio: frame.sample_aspect_ratio,
        color: frame.color,
        chroma_location: frame.chroma_location,
        bit_depth: plane_layout.bit_depth(),
        plane_layout,
        device_identity: 0,
        decoder_generation: 0,
        picture_identity: u64::from_ne_bytes(frame.pts.ticks().to_ne_bytes()),
    }
}
