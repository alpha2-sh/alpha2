//! `FFmpeg` `VideoToolbox` frames through the `Metal` interop, and the hardware codec list.

use std::{ffi::c_void, sync::Arc};

use alpha2_ffmpeg_bridge::{BridgeError, NativeGpuBackend, VideoToolboxFrameLease};

use super::{
    ImportedMetalFrame, MetalEncoderPlanes, MetalImportError, SharedMetalContext, SharedMetalError,
    pixel_buffer_surface_id,
};

unsafe extern "C" {
    fn alpha2_native_gpu_vt_hardware_decodes(codec_type: u32) -> i32;
}

/// A codec `VideoToolbox` may decode in hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoToolboxCodec {
    /// H.264 (`avc1`).
    H264,
    /// HEVC (`hvc1`).
    Hevc,
    /// VP9 (`vp09`), through the supplemental decoder.
    Vp9,
    /// AV1 (`av01`), through the supplemental decoder.
    Av1,
}

impl VideoToolboxCodec {
    /// Every codec, in the order a device's list names them.
    const ALL: [Self; 4] = [Self::H264, Self::Hevc, Self::Vp9, Self::Av1];

    /// The codec's `FFmpeg` name, as conform and `capabilities.decode` spell it.
    const fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }

    /// The `CMVideoCodecType` four-character code.
    const fn four_cc(self) -> u32 {
        u32::from_be_bytes(*match self {
            Self::H264 => b"avc1",
            Self::Hevc => b"hvc1",
            Self::Vp9 => b"vp09",
            Self::Av1 => b"av01",
        })
    }
}

/// Whether `VideoToolbox` decodes `codec` in hardware on this machine, per
/// `VTIsHardwareDecodeSupported` after registering the supplemental decoders (per codec, not
/// per profile).
#[must_use]
pub fn videotoolbox_hardware_decodes(codec: VideoToolboxCodec) -> bool {
    // SAFETY: takes a plain integer and calls two VideoToolbox queries.
    unsafe { alpha2_native_gpu_vt_hardware_decodes(codec.four_cc()) != 0 }
}

/// The codecs `VideoToolbox` decodes in hardware, by `FFmpeg` name, asked once per process.
pub(super) fn hardware_decode_codecs() -> &'static [&'static str] {
    static CODECS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    CODECS.get_or_init(|| {
        VideoToolboxCodec::ALL
            .into_iter()
            .filter(|codec| videotoolbox_hardware_decodes(*codec))
            .map(VideoToolboxCodec::name)
            .collect()
    })
}

/// The system-wide `IOSurface` ID behind a `VideoToolbox` frame's pixel buffer, if any. It stays
/// the same when `CoreVideo` rewraps the surface, so counting IDs counts decoder surfaces.
#[must_use]
pub fn videotoolbox_surface_id(frame: &VideoToolboxFrameLease) -> Option<u32> {
    let pixel_buffer = usize::try_from(frame.info().ok()?.pixel_buffer).ok()? as *mut c_void;
    // SAFETY: the lease holds the frame and its pixel buffer for the call.
    unsafe { pixel_buffer_surface_id(pixel_buffer) }
}

fn bridge_error(error: MetalImportError) -> BridgeError {
    match error {
        MetalImportError::Unsupported(reason) => {
            BridgeError::UnsupportedSurface(format!("VideoToolbox frame {reason}"))
        }
        MetalImportError::NoSixteenBitPlanes => {
            BridgeError::InvalidInput("renderer does not expose 16-bit plane textures")
        }
        MetalImportError::Helper {
            operation,
            step,
            status,
        } => crate::status::helper_failure(NativeGpuBackend::Metal, operation, step, status),
        MetalImportError::Missing(what) => BridgeError::InvalidInput(what),
        MetalImportError::NotRenderTarget { .. } => {
            BridgeError::UnsupportedSurface(error.to_string())
        }
        MetalImportError::NotMetal => BridgeError::InvalidInput("renderer is not using Metal"),
    }
}

impl SharedMetalContext {
    /// The codecs `VideoToolbox` decodes in hardware here, by `FFmpeg` name (`h264`, `hevc`,
    /// `vp9`, `av1`), per codec, not per profile.
    #[must_use]
    pub fn decode_codecs(&self) -> &'static [&'static str] {
        hardware_decode_codecs()
    }

    /// Imports a `VideoToolbox` hardware frame's pixel buffer, as
    /// [`Self::import_pixel_buffer`] does; the lease keeps `frame`.
    ///
    /// # Errors
    /// Returns an error for unsupported or non-`IOSurface` buffers, or failed `Metal` wrapping.
    pub fn import_surface(
        &self,
        frame: Arc<VideoToolboxFrameLease>,
    ) -> Result<ImportedMetalFrame, BridgeError> {
        let decoded = frame.info()?;
        let pixel_buffer = usize::try_from(decoded.pixel_buffer)
            .map_err(|_| BridgeError::InvalidInput("invalid CoreVideo pointer"))?
            as *mut c_void;
        // SAFETY: the retained frame owns `pixel_buffer`, and the decoder does not reuse it while
        // the lease holds the frame.
        unsafe { self.import_pixel_buffer(pixel_buffer, Box::new(frame)) }.map_err(bridge_error)
    }

    /// Wraps the planes of an encoder-input `VideoToolbox` frame's pixel buffer as render
    /// targets on the renderer's device.
    ///
    /// # Errors
    /// Returns an error for a frame that is not a `VideoToolbox` hardware frame, or whose buffer
    /// is not `IOSurface`-backed 8-bit video-range NV12 that `Metal` can alias.
    pub fn encoder_planes(
        &self,
        frame: &rsmpeg::avutil::AVFrame,
    ) -> Result<MetalEncoderPlanes, SharedMetalError> {
        let info = alpha2_ffmpeg_bridge::videotoolbox_frame_info(frame)
            .map_err(|error| SharedMetalError(format!("encoder frame: {error}")))?;
        let pixel_buffer = usize::try_from(info.pixel_buffer)
            .ok()
            .filter(|pointer| *pointer != 0)
            .ok_or_else(|| SharedMetalError("encoder frame has no pixel buffer".into()))?
            as *mut c_void;
        // SAFETY: `frame` holds the pixel buffer for the call.
        unsafe { self.pixel_buffer_planes(pixel_buffer) }
            .map_err(|error| SharedMetalError(error.to_string()))
    }
}
