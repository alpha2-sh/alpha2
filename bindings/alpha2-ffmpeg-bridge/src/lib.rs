//! The `unsafe` boundary between the native binding and `FFmpeg`.
//!
//! It holds what rsmpeg's safe surface lacks: frame, plane and packet copies,
//! codec-open and side-data helpers, container I/O, and the C shims that read
//! hardware frames (Vulkan, D3D12, `VideoToolbox`). Keeping these operations in
//! one private crate lets the rest of the native binding retain the
//! workspace-wide `unsafe_code = "forbid"` policy.

use std::{error::Error, ffi::CStr, fmt, ptr, slice};

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
use std::ffi::CString;

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdevice"
))]
use std::sync::Mutex;
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
use std::sync::MutexGuard;

pub use alpha2_media::GpuFault;
use rsmpeg::{
    avcodec::{AVCodecContext, AVCodecParameters},
    avutil::AVFrame,
    error::RsmpegError,
    ffi,
    swscale::SwsContext,
};

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdevice"))]
use rsmpeg::avutil::AVHWDeviceContext;

mod container;
mod log;

pub use container::{
    CreateOutputError, DemuxInput, OpenInputError, OutputFile, PathError, file_url, open_input,
};
pub use log::{LogLine, LogMark, install_log_capture, log_detail, log_mark, log_since};

/// The fixed-width ABI shared with the C shims, generated from their headers
/// under `include/` by the build script.
///
/// Every target generates every platform's declarations, with bindgen's
/// compile-time layout assertions. Those come from the same header as the
/// struct, so they agree with any edit to it: they only check that rustc lays
/// the struct out as clang does for the target being built. The structs whose
/// shims only Windows or macOS compiles are also pinned by hand below, and
/// their conversions to the public frame records compile on every target.
#[allow(
    dead_code,
    non_camel_case_types,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::undocumented_unsafe_blocks
)]
mod interop {
    include!(concat!(env!("OUT_DIR"), "/interop.rs"));
}

// The layouts of the interop structs that only the D3D12 and VideoToolbox
// shims and their Rust callers use, which no Linux build compiles. A header
// edit that moves, resizes or renames one of their fields fails here on every
// target, so it is made on purpose, together with those consumers.
const _: () = {
    use interop::{Alpha2D3D12FrameInfo as D3D12, Alpha2VideoToolboxFrameInfo as VideoToolbox};
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<D3D12>() == 64 && align_of::<D3D12>() == 8);
    assert!(offset_of!(D3D12, resource) == 0);
    assert!(offset_of!(D3D12, fence) == 8);
    assert!(offset_of!(D3D12, fence_value) == 16);
    assert!(offset_of!(D3D12, device) == 24);
    assert!(offset_of!(D3D12, dxgi_format) == 32);
    assert!(offset_of!(D3D12, width) == 36);
    assert!(offset_of!(D3D12, height) == 40);
    assert!(offset_of!(D3D12, hardware_format) == 44);
    assert!(offset_of!(D3D12, software_format) == 48);
    assert!(offset_of!(D3D12, surface_width) == 52);
    assert!(offset_of!(D3D12, surface_height) == 56);
    assert!(size_of::<VideoToolbox>() == 24 && align_of::<VideoToolbox>() == 8);
    assert!(offset_of!(VideoToolbox, pixel_buffer) == 0);
    assert!(offset_of!(VideoToolbox, width) == 8);
    assert!(offset_of!(VideoToolbox, height) == 12);
    assert!(offset_of!(VideoToolbox, hardware_format) == 16);
    assert!(offset_of!(VideoToolbox, software_format) == 20);
};

/// Copies a hardware frame's pixels into a new software frame in its frames
/// context's `sw_format`, keeping timestamps, crop, and color properties.
///
/// This is the fallback for a decoded surface the renderer could not import:
/// `av_hwframe_transfer_data` synchronizes with the decoder the same way an
/// `FFmpeg` download does, so the pixels are the decoder's own.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdevice"
))]
// A frames context's buffer data is the `AVHWFramesContext` FFmpeg allocated, aligned for it.
#[allow(clippy::cast_ptr_alignment)]
fn download_hardware_frame(frame: &AVFrame) -> Result<AVFrame, BridgeError> {
    if frame.hw_frames_ctx.is_null() {
        return Err(BridgeError::InvalidInput(
            "hardware frame has no frames context",
        ));
    }
    // SAFETY: a non-null `hw_frames_ctx` is a reference the frame holds to its live
    // `AVHWFramesContext`; only its size is read.
    let written = unsafe {
        let frames = (*frame.hw_frames_ctx).data.cast::<ffi::AVHWFramesContext>();
        ((*frames).width, (*frames).height)
    };
    let mut software = AVFrame::new();
    software
        .hwframe_transfer_data(frame)
        .map_err(BridgeError::Ffmpeg)?;
    // FFmpeg allocates the copy with padded rows (`av_frame_get_buffer(.., 0)`, not zeroed) and the
    // VideoToolbox and D3D12 transfers write only the frames context's width of each: the rest of
    // every row was never written, though `DecodedFrame::yuv_planes` shares whole rows of a
    // download.
    zero_row_padding(&mut software, written.0, written.1)?;
    // SAFETY: both frames are live and exclusively borrowed or shared for the call;
    // `av_frame_copy_props` only reads `frame` and writes metadata into `software`, whose buffers
    // the transfer above already allocated.
    let copied = unsafe { ffi::av_frame_copy_props(software.as_mut_ptr(), frame.as_ptr()) };
    if copied < 0 {
        return Err(BridgeError::FfmpegCode(copied));
    }
    Ok(software)
}

/// Writes zero over each row of `frame` past the first `width` pixels, up to
/// its linesize, for the first `height` rows of each plane: the bytes a copy
/// that wrote only that much left as the allocator gave them.
///
/// `frame` must be the only owner of its buffers, as a frame `FFmpeg` has just
/// allocated is.
///
/// # Errors
/// Returns an error for a format without a descriptor, a negative size, or a
/// plane whose rows lie outside the buffers the frame references.
#[cfg_attr(not(feature = "hwdevice"), allow(dead_code))]
fn zero_row_padding(frame: &mut AVFrame, width: i32, height: i32) -> Result<(), BridgeError> {
    let format = frame.format;
    // SAFETY: av_pix_fmt_desc_get only reads FFmpeg's static descriptor table and returns null for
    // an unknown format.
    let descriptor = unsafe { ffi::av_pix_fmt_desc_get(format).as_ref() }
        .ok_or(BridgeError::InvalidInput("pixel format has no descriptor"))?;
    if width < 0 || height < 0 {
        return Err(BridgeError::InvalidInput("negative frame size"));
    }
    // SAFETY: as above.
    let planes = unsafe { ffi::av_pix_fmt_count_planes(format) };
    for plane in 0..usize::try_from(planes).unwrap_or(0).min(4) {
        let plane_index = i32::try_from(plane).unwrap_or(0);
        // SAFETY: av_image_get_linesize only reads the descriptor table.
        let written = unsafe { ffi::av_image_get_linesize(format, width, plane_index) };
        let (Ok(written), Ok(stride)) = (
            usize::try_from(written),
            usize::try_from(frame.linesize[plane]),
        ) else {
            continue;
        };
        let data = frame.data[plane];
        if data.is_null() || stride <= written {
            continue;
        }
        let shift = if plane == 1 || plane == 2 {
            descriptor.log2_chroma_h
        } else {
            0
        };
        let rows = usize::try_from(-((-height) >> shift)).unwrap_or(0);
        let start = data as usize;
        let end = stride
            .checked_mul(rows)
            .and_then(|len| start.checked_add(len))
            .ok_or(BridgeError::InvalidInput("frame plane size overflow"))?;
        let inside = frame.buf.iter().any(|buffer| {
            // SAFETY: a non-null entry of `AVFrame::buf` points at a live `AVBufferRef` for as long
            // as the frame holds it.
            unsafe { buffer.as_ref() }.is_some_and(|buffer| {
                let buffer_start = buffer.data as usize;
                buffer_start <= start && buffer_start.saturating_add(buffer.size) >= end
            })
        });
        if !inside {
            return Err(BridgeError::InvalidInput(
                "frame plane lies outside its buffers",
            ));
        }
        for row in 0..rows {
            // SAFETY: the check above put every row of this plane inside a buffer the frame
            // references, and the caller holds the only reference to it.
            unsafe { ptr::write_bytes(data.add(row * stride + written), 0, stride - written) };
        }
    }
    Ok(())
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
use interop::Alpha2D3D12FrameInfo as D3D12FrameDescriptor;

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
unsafe extern "C" {
    fn alpha2_d3d12_device_init(
        context: *mut ffi::AVHWDeviceContext,
        device: *mut std::ffi::c_void,
    ) -> i32;
    fn alpha2_d3d12_get_format(
        context: *mut ffi::AVCodecContext,
        formats: *const ffi::AVPixelFormat,
    ) -> ffi::AVPixelFormat;
    fn alpha2_d3d12_frame_info(
        frame: *const ffi::AVFrame,
        output: *mut D3D12FrameDescriptor,
    ) -> i32;
    fn alpha2_d3d12_frame_set_fence_value(frame: *mut ffi::AVFrame, value: u64) -> i32;
    fn alpha2_d3d12_frame_fence_value(frame: *const ffi::AVFrame, output: *mut u64) -> i32;
    fn alpha2_d3d12_frames_configure(
        frames: *mut ffi::AVBufferRef,
        dxgi_format: u32,
        resource_flags: u32,
    ) -> i32;
}

/// `DXGI_FORMAT_NV12`, the format of a D3D12VA encoder-input texture.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub const DXGI_FORMAT_NV12: u32 = 103;

/// Describes a D3D12VA frames context's textures before it allocates any:
/// `dxgi_format`, `resource_flags` (`D3D12_RESOURCE_FLAGS`), no heap flags,
/// and one texture per frame, never a texture array.
///
/// An encoder-input pool is one texture per frame because each frame carries
/// its own fence, which the encoder waits on and a copy into the frame
/// signals, and because a copy addresses the frame's planes as subresources 0
/// and 1 of its own texture. `FFmpeg` 8.1 renamed and added the fields this
/// sets without a new major version, so they are written by the bridge's
/// shim, compiled against the headers it links.
///
/// This must be called between `av_hwframe_ctx_alloc` and
/// `av_hwframe_ctx_init`; afterwards the textures already exist.
///
/// # Errors
/// Returns an error when the context is not a D3D12VA one.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub fn d3d12_frames_configure(
    frames: &mut rsmpeg::avutil::AVHWFramesContext,
    dxgi_format: u32,
    resource_flags: u32,
) -> Result<(), BridgeError> {
    // SAFETY: `frames` is a live frames context. The shim checks that its device is a D3D12VA one
    // before treating `hwctx` as D3D12VA's, and only writes its description fields.
    let status =
        unsafe { alpha2_d3d12_frames_configure(frames.as_mut_ptr(), dxgi_format, resource_flags) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    Ok(())
}

/// D3D12 resource and decode fence retained by one hardware frame.
///
/// Defined on every target, with its conversion from the shim's record, so
/// that a change to that record's fields fails to compile everywhere, not
/// only on Windows, where the shim and its callers are built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct D3D12FrameInfo {
    /// Borrowed `ID3D12Resource` pointer.
    pub resource: u64,
    /// Borrowed decode `ID3D12Fence` pointer.
    pub fence: u64,
    /// Fence value signaled after decode completes.
    pub fence_value: u64,
    /// `ID3D12Device` identity that allocated the resource.
    pub device: u64,
    /// Native `DXGI_FORMAT` value.
    pub dxgi_format: u32,
    /// The frame's width: its picture with the right crop applied and the
    /// left crop still on the frame.
    pub width: i32,
    /// The frame's height, with the bottom crop applied and the top crop
    /// still on the frame.
    pub height: i32,
    /// Software pixel format represented by the hardware frame.
    pub software_format: i32,
    /// Width of the texture the frame was decoded into.
    pub surface_width: u32,
    /// Height of the texture the frame was decoded into.
    pub surface_height: u32,
}

impl D3D12FrameInfo {
    #[cfg_attr(
        not(all(target_os = "windows", feature = "hwdevice")),
        allow(dead_code)
    )]
    const fn from_descriptor(descriptor: &interop::Alpha2D3D12FrameInfo) -> Self {
        Self {
            resource: descriptor.resource,
            fence: descriptor.fence,
            fence_value: descriptor.fence_value,
            device: descriptor.device,
            dxgi_format: descriptor.dxgi_format,
            width: descriptor.width,
            height: descriptor.height,
            software_format: descriptor.software_format,
            surface_width: descriptor.surface_width,
            surface_height: descriptor.surface_height,
        }
    }
}

/// Ref-counted ownership of one opaque D3D12 `AVFrame`.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub struct D3D12FrameLease {
    frame: Mutex<AVFrame>,
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
impl fmt::Debug for D3D12FrameLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("D3D12FrameLease")
            .finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
impl D3D12FrameLease {
    /// Takes ownership of one decoded `AV_PIX_FMT_D3D12` frame.
    #[must_use]
    pub fn new(frame: AVFrame) -> Self {
        Self {
            frame: Mutex::new(frame),
        }
    }

    /// Copies this frame's pixels to the CPU, for a surface that could not be
    /// imported.
    ///
    /// Every byte of the copy's rows, up to its linesizes, is written (the
    /// padding past the pixels is zero), so its planes are shared.
    ///
    /// # Errors
    /// Returns [`BridgeError`] when `FFmpeg` cannot transfer the frame.
    pub fn download(&self) -> Result<DecodedFrame, BridgeError> {
        let frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        download_hardware_frame(&frame).map(|frame| DecodedFrame {
            frame,
            written: true,
        })
    }

    /// Returns the retained frame's resource and synchronization descriptor.
    ///
    /// # Errors
    /// Returns an error when this is not a valid D3D12 hardware frame.
    pub fn info(&self) -> Result<D3D12FrameInfo, BridgeError> {
        let frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        d3d12_frame_info(&frame)
    }
}

/// Initializes an `FFmpeg` D3D12VA device context over the renderer's device.
///
/// # Safety
/// `device` must be the address of a live `ID3D12Device`. The shim adds its
/// own COM reference, which the context releases when it is freed, so the
/// caller's may be released afterwards.
///
/// # Errors
/// Returns an error if the pointer is null or `FFmpeg` rejects the device.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe fn create_d3d12_device_context(device: u64) -> Result<AVHWDeviceContext, BridgeError> {
    let device = usize::try_from(device)
        .map_err(|_| BridgeError::InvalidInput("invalid D3D12 device pointer"))?
        as *mut std::ffi::c_void;
    if device.is_null() {
        return Err(BridgeError::InvalidInput("D3D12 device is null"));
    }
    let mut context = alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_D3D12VA, "D3D12")?;
    let device_context = context.data.cast::<ffi::AVHWDeviceContext>();
    // SAFETY: the allocated context is writable, the caller guarantees `device` is a live
    // `ID3D12Device`, and the shim takes its own COM reference.
    let result = unsafe { alpha2_d3d12_device_init(device_context, device) };
    if result < 0 {
        return Err(BridgeError::FfmpegCode(result));
    }
    context.init().map_err(BridgeError::Ffmpeg)?;
    Ok(context)
}

/// Returns the pixel-format selector used by D3D12VA decoders.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
#[must_use]
pub fn d3d12_get_format_callback()
-> unsafe extern "C" fn(*mut ffi::AVCodecContext, *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    alpha2_d3d12_get_format
}

/// Reads one retained D3D12 frame descriptor.
///
/// # Errors
/// Returns an error if the frame is not a valid D3D12VA frame.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub fn d3d12_frame_info(frame: &AVFrame) -> Result<D3D12FrameInfo, BridgeError> {
    let mut descriptor = D3D12FrameDescriptor::default();
    // SAFETY: the frame remains live for the duration of the descriptor copy.
    let result = unsafe { alpha2_d3d12_frame_info(frame.as_ptr(), &raw mut descriptor) };
    if result != 0 {
        return Err(BridgeError::FfmpegCode(result));
    }
    Ok(D3D12FrameInfo::from_descriptor(&descriptor))
}

/// The value `FFmpeg` waits for on a D3D12VA frame's fence before it next
/// uses the frame: `AVD3D12VAFrame.sync_ctx.fence_value`.
///
/// # Errors
/// Returns an error if the frame is not a valid D3D12VA frame.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub fn d3d12_frame_fence_value(frame: &AVFrame) -> Result<u64, BridgeError> {
    let mut value = 0;
    // SAFETY: the frame is live for the call and the output is a writable local.
    let result = unsafe { alpha2_d3d12_frame_fence_value(frame.as_ptr(), &raw mut value) };
    if result != 0 {
        return Err(BridgeError::FfmpegCode(result));
    }
    Ok(value)
}

/// One encoder-pool D3D12VA frame held open for a native GPU write.
///
/// The counterpart of Vulkan's `VulkanEncodeFrameAccess`. `FFmpeg`'s D3D12VA
/// encoder waits on the frame's own fence for the value its
/// `sync_ctx.fence_value` holds before it reads the frame, so a copy into the
/// frame must leave that value describing what it signals, and only once it is
/// on the queue and fenced: [`Self::commit`] writes it, and a guard dropped
/// without one leaves the frame as `begin` found it.
///
/// The value is written through the bridge's shim, compiled against the
/// `FFmpeg` headers it links, because 8.1 moved the sync context within
/// `AVD3D12VAFrame` without a new major version.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub struct D3D12EncodeFrameAccess<'a> {
    frame: &'a mut AVFrame,
    info: D3D12FrameInfo,
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
impl fmt::Debug for D3D12EncodeFrameAccess<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("D3D12EncodeFrameAccess")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "windows", feature = "hwdevice"))]
impl<'a> D3D12EncodeFrameAccess<'a> {
    /// Opens one handoff on an encoder-pool frame.
    ///
    /// # Errors
    /// Returns an error when the frame is not a usable D3D12VA frame.
    pub fn begin(frame: &'a mut AVFrame) -> Result<Self, BridgeError> {
        let info = d3d12_frame_info(frame)?;
        Ok(Self { frame, info })
    }

    /// The frame's texture, fence and the fence value it holds now, as
    /// `begin` read them.
    #[must_use]
    pub const fn info(&self) -> &D3D12FrameInfo {
        &self.info
    }

    /// Retains the frame's buffers and hardware pool beyond this access guard.
    ///
    /// # Errors
    /// Returns an allocation error if `FFmpeg` cannot create the reference.
    pub fn retain_frame(&self) -> Result<AVFrame, BridgeError> {
        // SAFETY: the frame is live; av_frame_clone creates an independent AVFrame with buffer
        // refs.
        let raw = unsafe { ffi::av_frame_clone(self.frame.as_ptr()) };
        let raw = std::ptr::NonNull::new(raw)
            .ok_or(BridgeError::InvalidInput("cannot retain encode frame"))?;
        // SAFETY: the newly allocated frame is uniquely owned by this value.
        Ok(unsafe { AVFrame::from_raw(raw) })
    }

    /// Records `fence_value` as the value the frame's next user waits for:
    /// the one a submitted, fenced write into it signals on its fence.
    ///
    /// # Errors
    /// Returns an error if the frame is no longer a valid D3D12VA frame,
    /// which `begin` has already checked.
    pub fn commit(&mut self, fence_value: u64) -> Result<(), BridgeError> {
        // SAFETY: the frame is exclusively borrowed by this guard; the shim checks it is a
        // D3D12VA frame before writing its descriptor.
        let result =
            unsafe { alpha2_d3d12_frame_set_fence_value(self.frame.as_mut_ptr(), fence_value) };
        if result != 0 {
            return Err(BridgeError::FfmpegCode(result));
        }
        self.info.fence_value = fence_value;
        Ok(())
    }
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
use interop::Alpha2VideoToolboxFrameInfo as VideoToolboxFrameDescriptor;

// The VideoToolbox shim reads only FFmpeg's own structures, so Linux builds and tests it too; only
// macOS has frames for it.
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
#[cfg_attr(target_os = "linux", allow(dead_code))]
unsafe extern "C" {
    fn alpha2_videotoolbox_get_format(
        context: *mut ffi::AVCodecContext,
        formats: *const ffi::AVPixelFormat,
    ) -> ffi::AVPixelFormat;
    fn alpha2_videotoolbox_frame_info(
        frame: *const ffi::AVFrame,
        output: *mut VideoToolboxFrameDescriptor,
    ) -> i32;
}

/// `CoreVideo` pixel-buffer identity retained by one `VideoToolbox` frame.
///
/// Defined on every target, like [`D3D12FrameInfo`], so that a change to the
/// shim's record fails to compile everywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoToolboxFrameInfo {
    /// Borrowed `CVPixelBufferRef`, valid while the frame lease is retained.
    pub pixel_buffer: u64,
    /// Coded width reported by `FFmpeg`.
    pub width: i32,
    /// Coded height reported by `FFmpeg`.
    pub height: i32,
    /// Sample layout the frames context allocated, such as NV12 or P010.
    pub software_format: i32,
}

impl VideoToolboxFrameInfo {
    #[cfg_attr(
        not(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice")),
        allow(dead_code)
    )]
    const fn from_descriptor(descriptor: &interop::Alpha2VideoToolboxFrameInfo) -> Self {
        Self {
            pixel_buffer: descriptor.pixel_buffer,
            width: descriptor.width,
            height: descriptor.height,
            software_format: descriptor.software_format,
        }
    }
}

/// Reads one `VideoToolbox` frame's descriptor through the shim, refusing a
/// frame that is not one (`videotoolbox_shim.c`'s own check, not an `FFmpeg`
/// code).
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn read_videotoolbox_frame_info(frame: &AVFrame) -> Result<VideoToolboxFrameInfo, BridgeError> {
    let mut descriptor = VideoToolboxFrameDescriptor::default();
    // SAFETY: `frame` stays live throughout the synchronous descriptor copy; the shim checks the
    // format and `data[3]` before it reads them, and `hw_frames_ctx` before it reads through that.
    let result = unsafe { alpha2_videotoolbox_frame_info(frame.as_ptr(), &raw mut descriptor) };
    if result != 0 {
        return Err(BridgeError::InvalidInput(
            "frame is not a VideoToolbox frame with a pixel buffer",
        ));
    }
    Ok(VideoToolboxFrameInfo::from_descriptor(&descriptor))
}

/// Ref-counted ownership of one opaque `VideoToolbox` `AVFrame`.
#[cfg(all(target_os = "macos", feature = "hwdevice"))]
pub struct VideoToolboxFrameLease {
    frame: Mutex<AVFrame>,
}

#[cfg(all(target_os = "macos", feature = "hwdevice"))]
impl fmt::Debug for VideoToolboxFrameLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VideoToolboxFrameLease")
            .finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "macos", feature = "hwdevice"))]
impl VideoToolboxFrameLease {
    /// Takes ownership of one decoded `AV_PIX_FMT_VIDEOTOOLBOX` frame.
    #[must_use]
    pub fn new(frame: AVFrame) -> Self {
        Self {
            frame: Mutex::new(frame),
        }
    }

    /// Copies this frame's pixels to the CPU, for a surface that could not be
    /// imported.
    ///
    /// Every byte of the copy's rows, up to its linesizes, is written (the
    /// padding past the pixels is zero), so its planes are shared.
    ///
    /// # Errors
    /// Returns [`BridgeError`] when `FFmpeg` cannot transfer the frame.
    pub fn download(&self) -> Result<DecodedFrame, BridgeError> {
        let frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        download_hardware_frame(&frame).map(|frame| DecodedFrame {
            frame,
            written: true,
        })
    }

    /// Returns the retained frame's `CoreVideo` surface identity.
    ///
    /// # Errors
    /// Returns an error when the frame is not a `VideoToolbox` hardware frame.
    pub fn info(&self) -> Result<VideoToolboxFrameInfo, BridgeError> {
        let frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        read_videotoolbox_frame_info(&frame)
    }
}

/// Returns the pixel-format selector used by `VideoToolbox` decoders.
#[cfg(all(target_os = "macos", feature = "hwdevice"))]
#[must_use]
pub fn videotoolbox_get_format_callback()
-> unsafe extern "C" fn(*mut ffi::AVCodecContext, *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    alpha2_videotoolbox_get_format
}

/// Reads the `CoreVideo` surface identity without taking ownership of the frame.
///
/// # Errors
/// Returns an error when the frame is not a `VideoToolbox` hardware frame.
#[cfg(all(target_os = "macos", feature = "hwdevice"))]
pub fn videotoolbox_frame_info(frame: &AVFrame) -> Result<VideoToolboxFrameInfo, BridgeError> {
    read_videotoolbox_frame_info(frame)
}

/// Maximum number of Vulkan images described by an `FFmpeg` hardware frame.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub const MAX_VULKAN_PLANES: usize = interop::ALPHA2_MAX_VULKAN_PLANES as usize;

/// Raw shared-device description used to initialize `FFmpeg`'s Vulkan context.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[derive(Clone, Debug)]
pub struct VulkanDeviceInfo {
    /// Raw `VkInstance`.
    pub instance: u64,
    /// Raw `VkPhysicalDevice`.
    pub physical_device: u64,
    /// Raw `VkDevice`.
    pub device: u64,
    /// Queue family used by wgpu and native barriers.
    pub graphics_queue_family: u32,
    /// Enabled graphics queues in that family.
    pub graphics_queue_count: u32,
    /// Dedicated video decode queue family.
    pub decode_queue_family: u32,
    /// Enabled decode queues in that family.
    pub decode_queue_count: u32,
    /// Vulkan video codec-operation flags.
    pub decode_video_caps: u32,
    /// Dedicated video encode queue family. Ignored when the count is zero.
    pub encode_queue_family: u32,
    /// Enabled encode queues in that family, or zero when this device has none.
    pub encode_queue_count: u32,
    /// Vulkan video encode codec-operation flags.
    pub encode_video_caps: u32,
    /// Enabled core feature bits required by `FFmpeg`'s context contract.
    pub core_features: [u32; 4],
    /// Whether `synchronization2` is enabled on the device.
    pub synchronization2: u32,
    /// Whether `samplerYcbcrConversion` is enabled on the device.
    pub sampler_ycbcr_conversion: u32,
    /// Whether `videoMaintenance1` is enabled on the device.
    pub video_maintenance1: u32,
    /// Whether `videoDecodeVP9` (`VK_KHR_video_decode_vp9`) is enabled on the device.
    pub video_decode_vp9: u32,
    /// Enabled instance extension names.
    pub instance_extensions: Vec<String>,
    /// Enabled device extension names.
    pub device_extensions: Vec<String>,
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
use interop::Alpha2VulkanDeviceInfo as VulkanDeviceDescriptor;

/// Serializes one shared Vulkan queue family across `FFmpeg` and the renderer.
///
/// The bridge installs its own recursive queue locks on the device context, so
/// this handle takes the same lock `FFmpeg` takes around its own submissions.
/// It is only made over a context [`create_vulkan_device_context`] set up:
/// any other has no such lock, and one of another device type keeps other
/// data where the shim keeps its locks.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub struct VulkanQueueLock {
    device: AVHWDeviceContext,
    queue_family: u32,
}

// SAFETY: after construction the retained device context is only read, to reach the context pointer
// the shim's lock and unlock take. Those functions are internally synchronized by the recursive
// mutex they own, so concurrent calls from several threads are what this type exists to allow. The
// context reference itself is dropped once, by this value's single owner.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
unsafe impl Sync for VulkanQueueLock {}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl fmt::Debug for VulkanQueueLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanQueueLock")
            .field("queue_family", &self.queue_family)
            .finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl VulkanQueueLock {
    /// Locks submissions to `queue_family` on `device`.
    ///
    /// # Errors
    /// Returns an input error unless `device` is a context
    /// [`create_vulkan_device_context`] initialized and `queue_family` is one
    /// it keeps a lock for. Locking anything else would serialize nothing.
    #[allow(clippy::cast_ptr_alignment)]
    pub fn new(device: AVHWDeviceContext, queue_family: u32) -> Result<Self, BridgeError> {
        // SAFETY: the context is live while `device` holds it; the shim only reads its type, its
        // free callback and the state it installed itself.
        let lockable = unsafe {
            alpha2_vulkan_queue_lockable(
                device.data.cast::<ffi::AVHWDeviceContext>().cast_const(),
                queue_family,
            )
        };
        if lockable == 0 {
            return Err(BridgeError::InvalidInput(
                "Vulkan queue lock needs a family of a device the bridge initialized",
            ));
        }
        Ok(Self {
            device,
            queue_family,
        })
    }

    /// Holds the shared queue lock until the returned guard drops, on this
    /// thread: the lock is a mutex its owning thread has to release.
    #[must_use]
    pub fn lock(&self) -> VulkanQueueGuard<'_> {
        // SAFETY: the guard releases this exactly once, on this thread, since it is neither Send
        // nor Sync.
        unsafe { self.raw_lock() };
        VulkanQueueGuard {
            lock: self,
            _owning_thread: std::marker::PhantomData,
        }
    }

    /// Acquires the shared queue lock without a guard, for an adapter to a
    /// lock/unlock protocol of its own.
    ///
    /// # Safety
    /// The calling thread must release it with exactly one
    /// [`Self::raw_unlock`], in the reverse order of every other hold it
    /// takes on this lock.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn raw_lock(&self) {
        // SAFETY: `new` checked the context is one the shim set up, with a lock for this family,
        // and the retained context keeps that lock alive.
        unsafe {
            alpha2_vulkan_queue_lock(
                self.device.data.cast::<ffi::AVHWDeviceContext>(),
                self.queue_family,
            );
        }
    }

    /// Releases a hold taken by [`Self::raw_lock`].
    ///
    /// # Safety
    /// The calling thread must hold the lock from a [`Self::raw_lock`] not
    /// yet released, and this must be the most recent hold it took. A mutex
    /// another thread holds cannot be released, and an unbalanced release
    /// lets two submissions reach the queue at once.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn raw_unlock(&self) {
        // SAFETY: the caller holds this lock on this thread, per the contract above.
        unsafe {
            alpha2_vulkan_queue_unlock(
                self.device.data.cast::<ffi::AVHWDeviceContext>(),
                self.queue_family,
            );
        }
    }
}

/// One hold of a [`VulkanQueueLock`], released when it drops.
///
/// It stays on the thread that took it: the lock is a mutex only its owning
/// thread can release.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[derive(Debug)]
pub struct VulkanQueueGuard<'a> {
    lock: &'a VulkanQueueLock,
    _owning_thread: std::marker::PhantomData<*const ()>,
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl Drop for VulkanQueueGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: `lock` took this hold on this thread (the guard is not Send), and guards drop in
        // reverse order of creation.
        unsafe { self.lock.raw_unlock() };
    }
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
unsafe extern "C" {
    fn alpha2_vulkan_queue_lockable(
        context: *const ffi::AVHWDeviceContext,
        queue_family: u32,
    ) -> i32;
    fn alpha2_vulkan_queue_lock(context: *mut ffi::AVHWDeviceContext, queue_family: u32);
    fn alpha2_vulkan_queue_unlock(context: *mut ffi::AVHWDeviceContext, queue_family: u32);
    fn alpha2_vulkan_device_init(
        context: *mut ffi::AVHWDeviceContext,
        info: *const VulkanDeviceDescriptor,
    ) -> i32;
}

/// Initializes an `FFmpeg` Vulkan device context over an application-owned device.
///
/// # Safety
/// `info.instance`, `info.physical_device`, and `info.device` must be live
/// handles of one Vulkan instance, physical device, and logical device, created
/// with the queues, features, and extensions `info` names, and must outlive the
/// returned context. `FFmpeg` loads entry points through them and submits to
/// their queues.
///
/// # Errors
/// Returns an input error for invalid extension names or an `FFmpeg` error when
/// the external device context cannot be initialized.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe fn create_vulkan_device_context(
    info: &VulkanDeviceInfo,
) -> Result<AVHWDeviceContext, BridgeError> {
    let instance = info
        .instance_extensions
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| BridgeError::InvalidInput("Vulkan extension contains NUL"))?;
    let device = info
        .device_extensions
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| BridgeError::InvalidInput("Vulkan extension contains NUL"))?;
    let instance_ptrs = instance.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    let device_ptrs = device.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    let raw = VulkanDeviceDescriptor {
        instance: info.instance,
        physical_device: info.physical_device,
        device: info.device,
        graphics_queue_family: info.graphics_queue_family,
        graphics_queue_count: info.graphics_queue_count,
        decode_queue_family: info.decode_queue_family,
        decode_queue_count: info.decode_queue_count,
        decode_video_caps: info.decode_video_caps,
        encode_queue_family: info.encode_queue_family,
        encode_queue_count: info.encode_queue_count,
        encode_video_caps: info.encode_video_caps,
        shader_image_gather_extended: info.core_features[0],
        fragment_stores_and_atomics: info.core_features[1],
        shader_int64: info.core_features[2],
        vertex_pipeline_stores_and_atomics: info.core_features[3],
        synchronization2: info.synchronization2,
        sampler_ycbcr_conversion: info.sampler_ycbcr_conversion,
        video_maintenance1: info.video_maintenance1,
        video_decode_vp9: info.video_decode_vp9,
        instance_extensions: instance_ptrs.as_ptr(),
        instance_extension_count: u32::try_from(instance_ptrs.len())
            .map_err(|_| BridgeError::InvalidInput("too many Vulkan extensions"))?,
        device_extensions: device_ptrs.as_ptr(),
        device_extension_count: u32::try_from(device_ptrs.len())
            .map_err(|_| BridgeError::InvalidInput("too many Vulkan extensions"))?,
    };
    let mut context = alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_VULKAN, "Vulkan")?;
    let device_context = context.data.cast::<ffi::AVHWDeviceContext>();
    // SAFETY: AVHWDeviceContext::alloc returned this writable context; the C shim copies extension
    // names before this call returns, and the caller guarantees the handles in `info` are live and
    // belong together.
    let result = unsafe { alpha2_vulkan_device_init(device_context, &raw const raw) };
    if result < 0 {
        return Err(BridgeError::FfmpegCode(result));
    }
    context.init().map_err(BridgeError::Ffmpeg)?;
    Ok(context)
}

/// Opaque Vulkan handles and synchronization state for one `FFmpeg` hardware frame.
///
/// Handles are diagnostic identities only. The caller must not destroy them and
/// must retain the source `AVFrame` for as long as it inspects or imports them.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanFrameInfo {
    /// `FFmpeg` hardware pixel format.
    pub hardware_format: i32,
    /// Software layout represented by the hardware frames context.
    pub software_format: i32,
    /// Visible frame width.
    pub width: i32,
    /// Visible frame height.
    pub height: i32,
    /// Allocated image width, which the codec pads to whole macroblocks.
    pub coded_width: i32,
    /// Allocated image height, which the codec pads to whole macroblocks.
    pub coded_height: i32,
    /// Number of non-null Vulkan images.
    pub plane_count: u32,
    /// Whether `FFmpeg` allocated the image with sampled-image usage.
    pub sampled: bool,
    /// `VkInstance` identity.
    pub instance: u64,
    /// `VkPhysicalDevice` identity.
    pub physical_device: u64,
    /// `VkDevice` identity.
    pub device: u64,
    /// Per-plane `VkImage` identities.
    pub images: [u64; MAX_VULKAN_PLANES],
    /// Per-plane Vulkan formats.
    pub formats: [u32; MAX_VULKAN_PLANES],
    /// Current per-plane image layouts.
    pub layouts: [u32; MAX_VULKAN_PLANES],
    /// Current per-plane access masks.
    pub access: [u32; MAX_VULKAN_PLANES],
    /// Per-plane timeline semaphore identities.
    pub semaphores: [u64; MAX_VULKAN_PLANES],
    /// Values that an importing submission must wait for.
    pub semaphore_values: [u64; MAX_VULKAN_PLANES],
    /// Current image queue-family ownership.
    pub queue_families: [u32; MAX_VULKAN_PLANES],
}

/// Vulkan image and synchronization state of one hardware frame, as the shim
/// reads it under the frame lock.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub type VulkanFrameDescriptor = interop::Alpha2VulkanFrameInfo;

/// Locked `FFmpeg` Vulkan surface state passed to the native GPU boundary.
///
/// A record on its own is inert: editing one changes no frame. The record a
/// [`VulkanFrameAccessGuard`] or [`VulkanEncodeFrameAccess`] writes back is
/// reachable only through their `unsafe` `access_mut`, because the layouts
/// and timeline values it holds must describe work actually submitted, which
/// nothing here can check. What a guard unlocks comes from its own copy of
/// the record `begin` returned, never from this one.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[repr(transparent)]
#[derive(Default)]
pub struct VulkanFrameAccess(interop::Alpha2VulkanFrameAccess);

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl VulkanFrameAccess {
    /// The frame's images and their synchronization state.
    #[must_use]
    pub const fn frame(&self) -> &VulkanFrameDescriptor {
        &self.0.frame
    }

    /// The frame's images and their synchronization state, to record the
    /// layouts and timeline values a submission leaves them in (see
    /// [`VulkanFrameAccessGuard::access_mut`] for what they must be).
    pub const fn frame_mut(&mut self) -> &mut VulkanFrameDescriptor {
        &mut self.0.frame
    }

    /// The frames context's `VkAllocationCallbacks`, or zero.
    #[must_use]
    pub const fn allocator(&self) -> u64 {
        self.0.allocator
    }

    /// The queue family the frame's images are owned by.
    #[must_use]
    pub const fn queue_family(&self) -> u32 {
        self.0.queue_family
    }

    /// Array layers of each image.
    #[must_use]
    pub const fn layer_count(&self) -> u32 {
        self.0.layer_count
    }
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
unsafe extern "C" {
    fn alpha2_vulkan_get_format(
        context: *mut ffi::AVCodecContext,
        formats: *const ffi::AVPixelFormat,
    ) -> ffi::AVPixelFormat;
    fn alpha2_vulkan_frame_info(
        frame: *const ffi::AVFrame,
        output: *mut VulkanFrameDescriptor,
    ) -> i32;
    fn alpha2_vulkan_frames_add_usage(frames: *mut ffi::AVBufferRef, usage: u32) -> i32;
    fn alpha2_vulkan_frames_usage(frames: *const ffi::AVBufferRef) -> u32;
    fn alpha2_vulkan_frame_access_begin(
        frame: *mut ffi::AVFrame,
        output: *mut VulkanFrameAccess,
    ) -> i32;
    fn alpha2_vulkan_frame_access_end(
        begun: *const interop::Alpha2VulkanFrameAccess,
        updated: *const VulkanFrameAccess,
    );
}

/// Why the Vulkan shim refused a frame (`alpha2_contexts` and the access and
/// info entry points in `vulkan_shim.c`): its own checks, not an `FFmpeg` code.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
const fn vulkan_frame_refusal(status: i32) -> BridgeError {
    BridgeError::InvalidInput(match status {
        -1 => "frame is not a Vulkan hardware frame",
        -2 => "Vulkan frame has no hardware frames or device context",
        -3 => "Vulkan frame has no images",
        -4 => "Vulkan frames context has no frame or queue locks, or no layers",
        -5 => "Vulkan device has no graphics or compute queue",
        _ => "Vulkan frame could not be read",
    })
}

/// Ref-counted ownership of an opaque `FFmpeg` Vulkan frame.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub struct VulkanFrameLease {
    frame: Mutex<AVFrame>,
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl fmt::Debug for VulkanFrameLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanFrameLease")
            .finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl VulkanFrameLease {
    /// Takes ownership of one decoded `AV_PIX_FMT_VULKAN` frame.
    #[must_use]
    pub fn new(frame: AVFrame) -> Self {
        Self {
            frame: Mutex::new(frame),
        }
    }

    /// Copies this frame's pixels to the CPU, for a surface that could not be
    /// imported.
    ///
    /// Every byte of the copy's rows, up to its linesizes, is written (the
    /// padding past the pixels is zero), so its planes are shared.
    ///
    /// # Errors
    /// Returns [`BridgeError`] when `FFmpeg` cannot transfer the frame.
    pub fn download(&self) -> Result<DecodedFrame, BridgeError> {
        let frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        download_hardware_frame(&frame).map(|frame| DecodedFrame {
            frame,
            written: true,
        })
    }

    /// Returns a locked snapshot of the frame's native image state.
    ///
    /// # Errors
    /// Returns [`BridgeError`] when the frame or its hardware context is invalid.
    pub fn info(&self) -> Result<VulkanFrameInfo, BridgeError> {
        let frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vulkan_frame_info(&frame)
    }

    /// Locks the `FFmpeg` frame and queue for one native GPU handoff.
    ///
    /// # Errors
    /// Returns [`BridgeError`] if the frame has no usable sampled images or queue.
    pub fn lock_access(&self) -> Result<VulkanFrameAccessGuard<'_>, BridgeError> {
        let mut frame = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut access = VulkanFrameAccess::default();
        // SAFETY: the mutex retains and exclusively borrows the frame until the matching end call
        // in the guard's Drop implementation.
        let result =
            unsafe { alpha2_vulkan_frame_access_begin(frame.as_mut_ptr(), &raw mut access) };
        if result != 0 {
            return Err(vulkan_frame_refusal(result));
        }
        Ok(VulkanFrameAccessGuard {
            _frame: frame,
            begun: access.0,
            access,
            committed: false,
        })
    }
}

/// Guard for one locked `FFmpeg` frame and queue handoff.
///
/// Safe code can read the frame's state but not change what is written back
/// to it: a timeline value nothing signals would hang every later user of
/// the frame, and a layout the image is not in makes its contents undefined.
///
/// ```compile_fail,E0133
/// # fn forge(lease: &alpha2_ffmpeg_bridge::VulkanFrameLease) {
/// let mut guard = lease.lock_access().unwrap();
/// guard.access_mut().frame_mut().semaphore_values[0] += 1000;
/// guard.commit();
/// # }
/// ```
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub struct VulkanFrameAccessGuard<'a> {
    // Holds the lease's mutex for as long as the `FFmpeg` locks are held.
    _frame: MutexGuard<'a, AVFrame>,
    // What `begin` locked, which the end call releases and checks the write-back against.
    begun: interop::Alpha2VulkanFrameAccess,
    access: VulkanFrameAccess,
    committed: bool,
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl VulkanFrameAccessGuard<'_> {
    /// The frame's state while its `FFmpeg` locks are held.
    #[must_use]
    pub const fn access(&self) -> &VulkanFrameAccess {
        &self.access
    }

    /// The record written back to the frame on [`Self::commit`], for a
    /// submission to update.
    ///
    /// The end call writes a plane back only while its image and semaphore
    /// are the ones `begin` read, and a timeline value only when it does not
    /// go below the frame's; the rest it cannot check.
    ///
    /// # Safety
    /// When the guard is committed, each plane's layout and access mask must
    /// be the ones the work submitted on it leaves the image in, and each
    /// timeline value one that work submitted before the guard drops
    /// signals: `FFmpeg` waits for it before its next use of the frame.
    #[must_use]
    pub const unsafe fn access_mut(&mut self) -> &mut VulkanFrameAccess {
        &mut self.access
    }

    /// Commits updated synchronization values when the guard is released.
    pub fn commit(&mut self) {
        self.committed = true;
    }
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl Drop for VulkanFrameAccessGuard<'_> {
    fn drop(&mut self) {
        let updated = if self.committed {
            &raw const self.access
        } else {
            ptr::null()
        };
        // SAFETY: `begun` is the record begin returned for this frame, on this thread (the guard
        // holds a MutexGuard, so it is not Send), and this is the unique matching end call before
        // the mutex guard is released. Whoever wrote `access` through `access_mut` vouched for it.
        unsafe { alpha2_vulkan_frame_access_end(&raw const self.begun, updated) };
    }
}

/// One encoder-pool frame held open for a native GPU write.
///
/// The decode path reaches its frames through a [`VulkanFrameLease`], which
/// owns the `AVFrame` behind a mutex because several decode threads share it.
/// An encoder-input frame has one owner (the export that took it from the
/// pool), so it is borrowed directly, and this guard exists only to pair
/// `frame_access_begin` with its `end`.
///
/// It stays on the thread that began it: `begin` takes `FFmpeg`'s frame mutex
/// and the bridge's queue mutex, which only the thread holding them can
/// release.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub struct VulkanEncodeFrameAccess<'a> {
    frame: &'a mut AVFrame,
    // What `begin` locked, which the end call releases and checks the write-back against.
    begun: interop::Alpha2VulkanFrameAccess,
    access: VulkanFrameAccess,
    committed: bool,
    _owning_thread: std::marker::PhantomData<*const ()>,
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl<'a> VulkanEncodeFrameAccess<'a> {
    /// Retains the frame's buffers and hardware pool beyond this access guard.
    ///
    /// # Errors
    /// Returns an allocation error if `FFmpeg` cannot create the reference.
    pub fn retain_frame(&self) -> Result<AVFrame, BridgeError> {
        // SAFETY: the frame is live and its access guard holds the locks; av_frame_clone creates an
        // independent AVFrame with buffer refs.
        let raw = unsafe { ffi::av_frame_clone(self.frame.as_ptr()) };
        let raw = std::ptr::NonNull::new(raw)
            .ok_or(BridgeError::InvalidInput("cannot retain encode frame"))?;
        // SAFETY: the newly allocated frame is uniquely owned by this value.
        Ok(unsafe { AVFrame::from_raw(raw) })
    }

    /// Opens one handoff on an encoder-pool frame.
    ///
    /// # Errors
    /// Returns an error when the frame is not a usable Vulkan frame.
    pub fn begin(frame: &'a mut AVFrame) -> Result<Self, BridgeError> {
        let mut access = VulkanFrameAccess::default();
        // SAFETY: `frame` is exclusively borrowed until this guard drops, and Drop is the unique
        // matching end call.
        let result =
            unsafe { alpha2_vulkan_frame_access_begin(frame.as_mut_ptr(), &raw mut access) };
        if result != 0 {
            return Err(vulkan_frame_refusal(result));
        }
        Ok(Self {
            frame,
            begun: access.0,
            access,
            committed: false,
            _owning_thread: std::marker::PhantomData,
        })
    }

    /// The frame's state while its `FFmpeg` locks are held.
    #[must_use]
    pub const fn access(&self) -> &VulkanFrameAccess {
        &self.access
    }

    /// The record written back to the frame on [`Self::commit`], for a
    /// submission to update; checked as [`VulkanFrameAccessGuard::access_mut`]
    /// says.
    ///
    /// # Safety
    /// As for [`VulkanFrameAccessGuard::access_mut`].
    #[must_use]
    pub const unsafe fn access_mut(&mut self) -> &mut VulkanFrameAccess {
        &mut self.access
    }

    /// Writes the updated layout and timeline values back when released.
    pub fn commit(&mut self) {
        self.committed = true;
    }
}

#[cfg(all(target_os = "linux", feature = "hwdevice"))]
impl Drop for VulkanEncodeFrameAccess<'_> {
    fn drop(&mut self) {
        let updated = if self.committed {
            &raw const self.access
        } else {
            ptr::null()
        };
        // SAFETY: `begun` is the record begin returned for this frame, on this thread (the guard
        // is not Send), and this is the unique matching end call. Whoever wrote `access` through
        // `access_mut` vouched for it.
        unsafe { alpha2_vulkan_frame_access_end(&raw const self.begun, updated) };
    }
}

/// Returns the callback used by a Vulkan hardware decoder to select its opaque format.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[must_use]
pub fn vulkan_get_format_callback()
-> unsafe extern "C" fn(*mut ffi::AVCodecContext, *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    alpha2_vulkan_get_format
}

/// Reads Vulkan image and synchronization metadata from an opaque hardware frame.
///
/// The C shim uses `FFmpeg`'s exact platform headers and takes the frame lock while
/// copying mutable layout and semaphore fields.
///
/// # Errors
/// Returns an error if the frame is not a valid Vulkan hardware frame.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub fn vulkan_frame_info(frame: &AVFrame) -> Result<VulkanFrameInfo, BridgeError> {
    let mut raw = VulkanFrameDescriptor::default();
    // SAFETY: `frame` is live for the duration of the call and `raw` is a correctly laid-out
    // writable result. The shim validates all FFmpeg-owned intermediate pointers before
    // dereferencing them.
    let result = unsafe { alpha2_vulkan_frame_info(frame.as_ptr(), &raw mut raw) };
    if result != 0 {
        return Err(vulkan_frame_refusal(result));
    }
    Ok(VulkanFrameInfo {
        hardware_format: raw.hardware_format,
        software_format: raw.software_format,
        width: raw.width,
        height: raw.height,
        coded_width: raw.coded_width,
        coded_height: raw.coded_height,
        plane_count: raw.plane_count,
        sampled: raw.sampled != 0,
        instance: raw.instance,
        physical_device: raw.physical_device,
        device: raw.device,
        images: raw.images,
        formats: raw.formats,
        layouts: raw.layouts,
        access: raw.access,
        semaphores: raw.semaphores,
        semaphore_values: raw.semaphore_values,
        queue_families: raw.queue_families,
    })
}

/// Whether the loaded libavutil was built with hardware device type `kind`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdevice"))]
fn hwdevice_available(kind: ffi::AVHWDeviceType) -> bool {
    let mut each = ffi::AV_HWDEVICE_TYPE_NONE;
    loop {
        // SAFETY: av_hwdevice_iterate_types only reads FFmpeg's static table of compiled-in types.
        each = unsafe { ffi::av_hwdevice_iterate_types(each) };
        if each == ffi::AV_HWDEVICE_TYPE_NONE {
            return false;
        }
        if each == kind {
            return true;
        }
    }
}

/// A new device context of type `kind` (`name` in errors). rsmpeg's `alloc`
/// unwraps a null context, which `FFmpeg` also returns for a type the loaded
/// libavutil lacks, so the type is checked first.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdevice"))]
fn alloc_hwdevice(
    kind: ffi::AVHWDeviceType,
    name: &'static str,
) -> Result<AVHWDeviceContext, BridgeError> {
    if !hwdevice_available(kind) {
        return Err(BridgeError::MissingHwDevice(name));
    }
    Ok(AVHWDeviceContext::alloc(kind))
}

/// Failure returned by an operation of this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum BridgeError {
    /// The caller supplied invalid dimensions or a mismatched byte count.
    InvalidInput(&'static str),
    /// `FFmpeg` could not allocate or prepare its destination frame.
    Ffmpeg(RsmpegError),
    /// An `FFmpeg` operation returned a negative `AVERROR` code. Only
    /// `FFmpeg`'s own codes: a platform GPU status is [`Self::NativeGpu`].
    FfmpegCode(i32),
    /// A platform GPU call a native interop helper made failed: which API,
    /// which step, and the status it returned.
    NativeGpu(NativeGpuFailure),
    /// The loaded `FFmpeg` was built without this hardware device type: which
    /// ones it has is a property of the runtime libraries, not of this build.
    MissingHwDevice(&'static str),
    /// A decoded surface the GPU interop does not take, described with what
    /// the surface is (its pixel format, size and planes) and why.
    UnsupportedSurface(String),
}

/// How a planar or semi-planar YCbCr pixel format stores its samples, as
/// [`yuv_layout`] reads it from `FFmpeg`'s descriptor.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct YuvPlaneLayout {
    /// Log2 of the horizontal chroma subsampling.
    pub chroma_shift_x: u8,
    /// Log2 of the vertical chroma subsampling.
    pub chroma_shift_y: u8,
    /// Significant bits per sample; above eight, samples are `u16`s.
    pub bit_depth: u8,
    /// Whether U and V interleave in the second plane.
    pub interleaved_chroma: bool,
    /// Whether samples sit in the high bits of their `u16`.
    pub msb_aligned: bool,
}

impl YuvPlaneLayout {
    /// Bytes per stored sample.
    #[must_use]
    pub const fn bytes_per_sample(self) -> u32 {
        if self.bit_depth > 8 { 2 } else { 1 }
    }
}

/// One plane of a decoded frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FramePlane {
    /// `stride * height` bytes, every one of them written: a view of the
    /// frame's own reference-counted buffer, or a tightly packed copy of its
    /// samples.
    pub bytes: bytes::Bytes,
    /// Bytes between rows, at least the row's samples.
    pub stride: u32,
    /// Sample positions per row; an interleaved chroma plane holds a U, V
    /// pair at each.
    pub width: u32,
    /// Rows.
    pub height: u32,
}

/// The planes of a YCbCr `AVFrame`: luma first, then chroma.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YuvPlanes {
    /// How the planes store their samples.
    pub layout: YuvPlaneLayout,
    /// Coded luma width.
    pub width: u32,
    /// Coded luma height.
    pub height: u32,
    /// Luma, then U and V, or one interleaved chroma plane.
    pub planes: Vec<FramePlane>,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(context) => {
                write!(formatter, "invalid media buffer input: {context}")
            }
            Self::Ffmpeg(error) => write!(formatter, "{error}"),
            // `FFmpeg`'s own words for the code, as `rsmpeg` writes every other `FFmpeg` failure.
            Self::FfmpegCode(code) => write!(formatter, "{}", RsmpegError::AVError(*code)),
            Self::NativeGpu(failure) => write!(formatter, "{failure}"),
            Self::MissingHwDevice(kind) => {
                write!(formatter, "the loaded FFmpeg has no {kind} hardware device")
            }
            Self::UnsupportedSurface(description) => formatter.write_str(description),
        }
    }
}

/// Applies explicit matrix and range coefficients to a scaling context.
///
/// Coefficient arguments are `SWS_CS_*` constants. RGB endpoints still need
/// an explicit full-range flag.
///
/// # Errors
/// Returns an error when `libswscale` rejects the configuration.
pub fn configure_colorspace(
    context: &mut SwsContext,
    source_coefficients: i32,
    source_full_range: bool,
    destination_coefficients: i32,
    destination_full_range: bool,
) -> Result<(), BridgeError> {
    // SAFETY: the context is live and exclusively borrowed; both coefficient pointers refer to
    // static arrays owned by `libswscale`.
    let result = unsafe {
        ffi::sws_setColorspaceDetails(
            context.as_mut_ptr(),
            ffi::sws_getCoefficients(source_coefficients),
            i32::from(source_full_range),
            ffi::sws_getCoefficients(destination_coefficients),
            i32::from(destination_full_range),
            0,
            1 << 16,
            1 << 16,
        )
    };
    if result < 0 {
        Err(BridgeError::FfmpegCode(result))
    } else {
        Ok(())
    }
}

/// Converts the whole of `frame` with `context` into `output`: `target`'s
/// planes one after another without row padding, the layout
/// `av_image_copy_to_buffer` writes at alignment 1, so the caller's buffer is
/// the only one the picture is written to.
///
/// # Errors
/// Returns an error when `output` is not exactly the size of that layout at
/// `frame`'s size, or when `libswscale` fails.
pub fn scale_frame_packed(
    context: &mut SwsContext,
    frame: &AVFrame,
    target: ffi::AVPixelFormat,
    output: &mut [u8],
) -> Result<(), BridgeError> {
    // SAFETY: a pure size computation over plain values.
    let size = unsafe { ffi::av_image_get_buffer_size(target, frame.width, frame.height, 1) };
    if size < 0 {
        return Err(BridgeError::FfmpegCode(size));
    }
    if usize::try_from(size).ok() != Some(output.len()) {
        return Err(BridgeError::InvalidInput(
            "packed output is not the size of the converted frame",
        ));
    }
    let mut data = [ptr::null_mut::<u8>(); 4];
    let mut linesize = [0; 4];
    // SAFETY: `output` holds exactly the bytes this layout needs, checked above, and the two arrays
    // have room for every plane `FFmpeg` describes.
    let filled = unsafe {
        ffi::av_image_fill_arrays(
            data.as_mut_ptr(),
            linesize.as_mut_ptr(),
            output.as_mut_ptr(),
            target,
            frame.width,
            frame.height,
            1,
        )
    };
    if filled < 0 {
        return Err(BridgeError::FfmpegCode(filled));
    }
    // SAFETY: the context is live and exclusively borrowed; the source planes belong to `frame`,
    // which outlives the call; every destination pointer lies inside `output`, exclusively borrowed
    // for the call.
    let rows = unsafe {
        ffi::sws_scale(
            context.as_mut_ptr(),
            frame.data.as_ptr().cast(),
            frame.linesize.as_ptr(),
            0,
            frame.height,
            data.as_ptr(),
            linesize.as_ptr(),
        )
    };
    if rows < 0 {
        Err(BridgeError::FfmpegCode(rows))
    } else {
        Ok(())
    }
}

impl Error for BridgeError {}

impl BridgeError {
    /// What a failed platform GPU call's status says about the device, when
    /// this is one and its status names a fault.
    #[must_use]
    pub const fn gpu_fault(&self) -> Option<GpuFault> {
        match self {
            Self::NativeGpu(failure) => failure.fault,
            _ => None,
        }
    }
}

/// The graphics API a native interop helper called.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NativeGpuBackend {
    /// Vulkan; the status is a `VkResult`.
    Vulkan,
    /// Direct3D 12; the status is an `HRESULT`.
    D3D12,
    /// Metal through `CoreVideo`; the status is a `CVReturn`.
    Metal,
}

impl NativeGpuBackend {
    /// The API's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vulkan => "Vulkan",
            Self::D3D12 => "D3D12",
            Self::Metal => "Metal",
        }
    }
}

/// A platform GPU call that failed inside a native interop helper.
///
/// `fault` is read from `status` by the helper's backend, which knows its
/// API's codes, so a caller classifies the failure by it rather than by the
/// message: the message names the operation, the step and the raw status,
/// never the fault.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NativeGpuFailure {
    /// The API the call belongs to.
    pub backend: NativeGpuBackend,
    /// What the helper was doing, such as `encode copy`.
    pub operation: &'static str,
    /// The step of it that failed, such as `queue submit`.
    pub step: &'static str,
    /// The status the call returned: a `VkResult`, an `HRESULT` (as its
    /// bits), or a `CVReturn`. Zero when the step is the helper's own check.
    pub status: i32,
    /// What the status is evidence of, when it names a fault.
    pub fault: Option<GpuFault>,
}

impl fmt::Display for NativeGpuFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            backend,
            operation,
            step,
            status,
            ..
        } = *self;
        let backend = backend.as_str();
        // A timeout carries the renderer's own timeout sentence, so a hung GPU reads the same
        // wherever it was waited on.
        if self.fault == Some(GpuFault::Timeout) {
            write!(
                formatter,
                "{backend} {operation} ({step}): {}",
                alpha2_media::GPU_TIMEOUT_MESSAGE
            )?;
        } else {
            write!(formatter, "{backend} {operation} failed at {step}")?;
        }
        if status == 0 {
            return Ok(());
        }
        match self.backend {
            NativeGpuBackend::Vulkan => write!(formatter, " (VkResult {status})"),
            NativeGpuBackend::D3D12 => write!(formatter, " (HRESULT {status:#010X})"),
            NativeGpuBackend::Metal => write!(formatter, " (CVReturn {status})"),
        }
    }
}

impl From<RsmpegError> for BridgeError {
    fn from(error: RsmpegError) -> Self {
        Self::Ffmpeg(error)
    }
}

/// Returns the linked `FFmpeg` runtime version string.
#[must_use]
pub fn runtime_version() -> String {
    // SAFETY: FFmpeg returns a process-lifetime, NUL-terminated string.
    unsafe { CStr::from_ptr(ffi::av_version_info()) }
        .to_string_lossy()
        .into_owned()
}

/// Returns the linked libavcodec configure string.
#[must_use]
pub fn runtime_configuration() -> String {
    // SAFETY: FFmpeg returns a process-lifetime, NUL-terminated string.
    unsafe { CStr::from_ptr(ffi::avcodec_configuration()) }
        .to_string_lossy()
        .into_owned()
}

/// Whether the linked muxer named `format` (`mp4`, `matroska`, ...) can
/// store `codec` at normal standard compliance; `None` when no such muxer is
/// linked.
#[must_use]
pub fn muxer_accepts_codec(format: &CStr, codec: ffi::AVCodecID) -> Option<bool> {
    // SAFETY: `format` is NUL-terminated; FFmpeg returns a registered, process-lifetime muxer
    // or null.
    let muxer = unsafe { ffi::av_guess_format(format.as_ptr(), ptr::null(), ptr::null()) };
    if muxer.is_null() {
        return None;
    }
    // SAFETY: `muxer` is the live registered muxer found above.
    let accepted = unsafe {
        ffi::avformat_query_codec(
            muxer,
            codec,
            i32::try_from(ffi::FF_COMPLIANCE_NORMAL).unwrap_or(0),
        )
    };
    Some(accepted == 1)
}

/// Chroma layout, component depth, and alpha of one pixel format.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SampleProfile {
    /// Whether the format is planar/semi-planar YCbCr with 4:2:0 subsampling.
    pub is_yuv_420: bool,
    /// Largest component depth in bits.
    pub bit_depth: u8,
    /// Whether the format carries an alpha component.
    pub has_alpha: bool,
    /// Whether the components are red, green, and blue (a palette's
    /// entries among them) rather than luma and chroma (or gray).
    pub is_rgb: bool,
}

/// Describes a pixel format, or `None` when the value has no descriptor.
///
/// A container may leave a stream's format unset until the first frame is
/// decoded, so callers treat `None` as "not yet known" rather than as a
/// rejection.
#[must_use]
pub fn sample_profile(format: ffi::AVPixelFormat) -> Option<SampleProfile> {
    if format == ffi::AV_PIX_FMT_NONE {
        return None;
    }
    // SAFETY: `av_pix_fmt_desc_get` accepts every enum value and returns either null or a pointer
    // into FFmpeg's process-lifetime descriptor table.
    let descriptor = unsafe { ffi::av_pix_fmt_desc_get(format).as_ref() }?;
    let components = usize::from(descriptor.nb_components.min(4));
    Some(SampleProfile {
        is_yuv_420: descriptor.log2_chroma_w == 1
            && descriptor.log2_chroma_h == 1
            && descriptor.nb_components >= 3
            && descriptor.flags & u64::from(ffi::AV_PIX_FMT_FLAG_RGB) == 0,
        bit_depth: descriptor.comp[..components]
            .iter()
            .map(|component| u8::try_from(component.depth).unwrap_or(u8::MAX))
            .max()
            .unwrap_or(8),
        has_alpha: descriptor.flags & u64::from(ffi::AV_PIX_FMT_FLAG_ALPHA) != 0,
        is_rgb: descriptor.flags & u64::from(ffi::AV_PIX_FMT_FLAG_RGB | ffi::AV_PIX_FMT_FLAG_PAL)
            != 0,
    })
}

/// Returns `FFmpeg`'s stable name for a pixel-format value.
#[must_use]
pub fn pixel_format_name(format: ffi::AVPixelFormat) -> Option<String> {
    // SAFETY: `av_get_pix_fmt_name` accepts every enum value and returns either null or a
    // process-lifetime, NUL-terminated string.
    let name = unsafe { ffi::av_get_pix_fmt_name(format) };
    if name.is_null() {
        None
    } else {
        // SAFETY: non-null return values obey the FFmpeg string contract above.
        Some(
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Opens `context` with `options`, returning what `FFmpeg` did not consume.
///
/// `avcodec_open2` frees the dictionary it is handed and writes the unconsumed
/// remainder back through the same pointer. It does this on its failure path
/// as well as its success path, at the shared `end:` label. rsmpeg 0.18's
/// `AVCodecContext::open` only accounts for that when the call succeeds: on
/// failure it drops the caller's `AVDictionary`, which frees memory `FFmpeg`
/// has already freed. That is reachable from the export path the moment a
/// candidate encoder fails to open, which is exactly what the open-time
/// fallback is for, and it aborts the process instead of trying the next
/// candidate. Observed as a segfault opening `h264_vulkan` with
/// `AV_PIX_FMT_YUV420P`; `avcodec_open2` itself returns `EINVAL` cleanly.
///
/// # Errors
/// Returns the `FFmpeg` status when the encoder or decoder cannot be opened.
pub fn open_codec_context(
    context: &mut rsmpeg::avcodec::AVCodecContext,
    options: rsmpeg::avutil::AVDictionary,
) -> Result<Option<rsmpeg::avutil::AVDictionary>, RsmpegError> {
    let _media_foundation = media_foundation_lock(context);
    // Ownership moves to FFmpeg here, before the call that frees it.
    let mut dictionary = options.into_raw().as_ptr();
    // SAFETY: `context` is a live codec context, and `dictionary` is a valid dictionary FFmpeg now
    // owns. FFmpeg replaces it with the remainder, which is either null or a dictionary this
    // function takes ownership of below.
    let status = unsafe {
        ffi::avcodec_open2(
            context.as_mut_ptr(),
            ptr::null_mut(),
            std::ptr::addr_of_mut!(dictionary),
        )
    };
    // SAFETY: a non-null remainder is a dictionary FFmpeg allocated and handed back through the
    // same pointer, so this is the only owner of it.
    let unused = ptr::NonNull::new(dictionary)
        .map(|dictionary| unsafe { rsmpeg::avutil::AVDictionary::from_raw(dictionary) });
    if status < 0 {
        return Err(RsmpegError::from(status));
    }
    Ok(unused)
}

/// Serializes opening and closing `FFmpeg`'s `MediaFoundation` (`_mf`)
/// encoders: the guard for `context` when it is one, `None` otherwise.
///
/// An `_mf` encoder calls `MFTEnumEx` when it opens and `MFShutdown` when it
/// closes, and those two deadlock against each other on different threads:
/// `MFShutdown` (in `RtwqShutdown`) holds the work-queue lock and waits on
/// mfplat's critical section, while `MFTEnumEx` holds that section and waits on
/// the work-queue lock. Observed as a hang between an export closing `h264_mf`
/// and the encoder probe opening `av1_mf`. Holding one extra `MFStartup` does
/// not avoid it: `MFShutdown` takes the lock whatever the count. So every open
/// ([`open_codec_context`]) and every close ([`EncoderContext`]) of one takes
/// this lock.
fn media_foundation_lock(
    context: &rsmpeg::avcodec::AVCodecContext,
) -> Option<std::sync::MutexGuard<'static, ()>> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if !cfg!(target_os = "windows") || context.codec.is_null() {
        return None;
    }
    // SAFETY: a non-null `codec` is one of FFmpeg's static codec descriptors, whose `name` is a
    // static, NUL-terminated string.
    let name = unsafe { CStr::from_ptr((*context.codec).name) };
    name.to_bytes().ends_with(b"_mf").then(|| {
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    })
}

/// An encoder context that is closed under the `MediaFoundation` lock when it
/// is one of the `_mf` encoders (see [`open_codec_context`]), and dropped as
/// usual otherwise. It reads and writes as the [`AVCodecContext`] it holds.
///
/// [`AVCodecContext`]: rsmpeg::avcodec::AVCodecContext
pub struct EncoderContext(Option<rsmpeg::avcodec::AVCodecContext>);

impl From<rsmpeg::avcodec::AVCodecContext> for EncoderContext {
    fn from(context: rsmpeg::avcodec::AVCodecContext) -> Self {
        Self(Some(context))
    }
}

impl std::ops::Deref for EncoderContext {
    type Target = rsmpeg::avcodec::AVCodecContext;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("taken only when dropped")
    }
}

impl std::ops::DerefMut for EncoderContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("taken only when dropped")
    }
}

impl Drop for EncoderContext {
    fn drop(&mut self) {
        if let Some(context) = self.0.take() {
            let _media_foundation = media_foundation_lock(&context);
            drop(context);
        }
    }
}

/// `VkImageUsageFlags` an encoder-input pool needs beyond `FFmpeg`'s own.
///
/// `TRANSFER_DST` is what the renderer copies into, `SAMPLED` is what lets
/// wgpu import the image for a read, and `TRANSFER_SRC` keeps a readback
/// possible for parity checks. `VIDEO_ENCODE_SRC` is the encoder's own.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub const VULKAN_ENCODE_INPUT_USAGE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_4000;

/// Adds `usage` to a Vulkan frames context before it allocates its pool.
///
/// `FFmpeg`'s Vulkan encoder requests no usage of its own. Left at zero,
/// `hwcontext_vulkan` picks one: whichever of `TRANSFER_DST`, `TRANSFER_SRC`,
/// `STORAGE` and `SAMPLED` the format supports, plus host transfer and
/// `VIDEO_ENCODE_SRC` when the device has them, and it then only takes a
/// format that supports `STORAGE`. A non-zero usage is used exactly as given:
/// `FFmpeg` 8.0.3 drops no flag of it, and every flag must be one the format
/// supports, or the pool's images are created with a usage they cannot have.
/// So an encoder-input pool asks for [`VULKAN_ENCODE_INPUT_USAGE`], what an
/// export does with its images and nothing else.
///
/// This must be called between `av_hwframe_ctx_alloc` and
/// `av_hwframe_ctx_init`; afterwards the images already exist.
///
/// # Errors
/// Returns an error when the context is not a Vulkan one.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub fn vulkan_frames_add_usage(
    frames: &mut rsmpeg::avutil::AVHWFramesContext,
    usage: u32,
) -> Result<(), BridgeError> {
    // SAFETY: `frames` is a live frames context. The shim checks that its device is a Vulkan one
    // before treating `hwctx` as Vulkan's, and only ORs into its `usage` field.
    let status = unsafe { alpha2_vulkan_frames_add_usage(frames.as_mut_ptr(), usage) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    Ok(())
}

/// Usage the pool was created with.
///
/// For a pool given a usage before its init, that usage: `hwcontext_vulkan`
/// keeps a non-zero usage as given and drops nothing from it (see
/// [`vulkan_frames_add_usage`]). For one left at zero, the usage `FFmpeg`
/// picked.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[must_use]
pub fn vulkan_frames_usage(frames: &rsmpeg::avutil::AVHWFramesContext) -> u32 {
    // SAFETY: `frames` is a live frames context; the shim checks its device type before reading
    // Vulkan's `hwctx`, and returns 0 for any other.
    unsafe { alpha2_vulkan_frames_usage(frames.as_ptr()) }
}

/// Copies one packet's payload out of `FFmpeg`'s buffer.
///
/// `AVPacket` exposes its bytes as a raw pointer and a length, which is the
/// kind of pair this crate exists to keep out of the rest of the binding.
#[must_use]
pub fn packet_bytes(packet: &rsmpeg::avcodec::AVPacket) -> Vec<u8> {
    let size = usize::try_from(packet.size).unwrap_or(0);
    if packet.data.is_null() || size == 0 {
        return Vec::new();
    }
    // SAFETY: a received packet owns `size` readable bytes at `data` for as long as the packet is
    // alive, and `packet` is borrowed for this call.
    unsafe { slice::from_raw_parts(packet.data.cast_const(), size) }.to_vec()
}

/// A copy of a stream's codec extradata: the sample description's parameter
/// sets for H.264 and HEVC.
///
/// It takes rsmpeg's parameters, which only `FFmpeg` or `unsafe` code builds
/// (`AVStream::codecpar`): a bare `AVCodecParameters` can carry a forged
/// extradata pointer.
///
/// ```compile_fail,E0308
/// let mut forged: rsmpeg::ffi::AVCodecParameters = *rsmpeg::avcodec::AVCodecParameters::new();
/// forged.extradata = 0x10 as *mut u8;
/// forged.extradata_size = 64;
/// alpha2_ffmpeg_bridge::codec_parameters_extradata(&forged);
/// ```
#[must_use]
pub fn codec_parameters_extradata(parameters: &rsmpeg::avcodec::AVCodecParameters) -> Vec<u8> {
    let size = usize::try_from(parameters.extradata_size).unwrap_or(0);
    if parameters.extradata.is_null() || size == 0 {
        return Vec::new();
    }
    // SAFETY: codec parameters own `extradata_size` readable bytes at `extradata` for as long as
    // they are borrowed, which covers this copy.
    unsafe { slice::from_raw_parts(parameters.extradata.cast_const(), size) }.to_vec()
}

/// Whether `packet` carries side data of `kind`, as a demuxer marks the
/// packet where a stream's sample description changes
/// (`AV_PKT_DATA_NEW_EXTRADATA`).
#[must_use]
pub fn packet_has_side_data(
    packet: &rsmpeg::avcodec::AVPacket,
    kind: ffi::AVPacketSideDataType,
) -> bool {
    let mut size = 0_usize;
    // SAFETY: `packet` is a live packet borrowed for the call; the returned pointer is only
    // compared with null.
    !unsafe { ffi::av_packet_get_side_data(packet.as_ptr(), kind, &raw mut size) }.is_null()
}

/// Takes `packet`'s side data of `kind` off it: a copy of its bytes, or
/// `None` when it carries none.
///
/// An encoder's parameter sets arrive as `AV_PKT_DATA_NEW_EXTRADATA` once
/// `extract_extradata` has cut them out of the bitstream. They belong in the
/// sample description, and the MP4 muxer reads a packet that still carries
/// them as a change of sample description, so they are taken rather than
/// copied.
#[must_use]
pub fn take_packet_side_data(
    packet: &mut rsmpeg::avcodec::AVPacket,
    kind: ffi::AVPacketSideDataType,
) -> Option<Vec<u8>> {
    let mut size = 0_usize;
    // SAFETY: `packet` is a live packet borrowed for the call; a non-null result points at `size`
    // bytes the packet owns.
    let data = unsafe { ffi::av_packet_get_side_data(packet.as_ptr(), kind, &raw mut size) };
    if data.is_null() {
        return None;
    }
    // SAFETY: as above; the bytes are copied before the entry is freed below.
    let bytes = unsafe { slice::from_raw_parts(data, size) }.to_vec();
    let packet = packet.as_mut_ptr();
    // SAFETY: `side_data` and `side_data_elems` are the packet's own array and count, which
    // `av_packet_side_data_remove` keeps consistent; it frees the entry's `av_malloc`ed data, which
    // is how a packet's side data is allocated.
    unsafe {
        ffi::av_packet_side_data_remove(
            (*packet).side_data,
            &raw mut (*packet).side_data_elems,
            kind,
        );
    }
    Some(bytes)
}

/// Gives `packet` side data of `kind` holding `data`, replacing any it had.
///
/// # Errors
/// When the side data cannot be allocated, or `data` is larger than a
/// packet's side data can be.
pub fn set_packet_side_data(
    packet: &mut rsmpeg::avcodec::AVPacket,
    kind: ffi::AVPacketSideDataType,
    data: &[u8],
) -> Result<(), BridgeError> {
    let _ = take_packet_side_data(packet, kind);
    // SAFETY: `packet` is a live packet mutably borrowed for the call; `av_packet_new_side_data`
    // appends a zero-padded entry of `data.len()` bytes and returns null only when it cannot.
    let entry = unsafe { ffi::av_packet_new_side_data(packet.as_mut_ptr(), kind, data.len()) };
    if entry.is_null() {
        return Err(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)));
    }
    // SAFETY: the entry just made holds `data.len()` writable bytes.
    unsafe { ptr::copy_nonoverlapping(data.as_ptr(), entry, data.len()) };
    Ok(())
}

/// Replaces the extradata (the sample description's parameter sets, for
/// H.264 and HEVC) of `output`'s stream `index` with a copy of `data`, for a
/// muxer whose header is not written yet.
///
/// rsmpeg gives an output's existing streams out read-only, and a muxer
/// reads a stream's extradata into its sample description when the header
/// is written, so this is how a header that waits for an encoder's first
/// packet learns its parameter sets. `FFmpeg` owns extradata through
/// `av_malloc` with input padding after it, which a parser may read past
/// the end, so it cannot be handed a Rust buffer.
///
/// # Errors
/// When there is no stream `index`, or `data` is larger than extradata can
/// be or cannot be allocated.
pub fn set_output_stream_extradata(
    output: &mut rsmpeg::avformat::AVFormatContextOutput,
    index: usize,
    data: &[u8],
) -> Result<(), BridgeError> {
    let parameters = output
        .streams()
        .get(index)
        .ok_or(BridgeError::InvalidInput("the output has no such stream"))?
        .codecpar;
    let size = i32::try_from(data.len())
        .map_err(|_| BridgeError::InvalidInput("extradata is larger than FFmpeg holds"))?;
    let padding = usize::try_from(ffi::AV_INPUT_BUFFER_PADDING_SIZE).unwrap_or(64);
    // SAFETY: `av_mallocz` returns null or a zeroed buffer of the size asked for.
    let buffer = unsafe { ffi::av_mallocz(data.len() + padding) }.cast::<u8>();
    if buffer.is_null() {
        return Err(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)));
    }
    // SAFETY: `buffer` holds `data.len()` writable bytes and does not overlap `data`.
    unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len()) };
    // SAFETY: a stream's codec parameters are always allocated and owned by the stream, and the
    // output is mutably borrowed for the call. Their extradata is null or an `av_malloc`ed buffer
    // they own, which is freed and replaced by `buffer`, which they now own.
    unsafe {
        ffi::av_freep((&raw mut (*parameters).extradata).cast());
        (*parameters).extradata = buffer;
        (*parameters).extradata_size = size;
    }
    Ok(())
}

/// A packet holding a copy of `data`, every other field at its default.
///
/// An encoder that is not `FFmpeg`'s (`OpenH264`, loaded at run time) hands
/// its output over as bytes, and the muxer takes packets; rsmpeg can make an
/// empty packet but not give one a payload.
///
/// # Errors
/// When `data` is larger than a packet can be, or the packet cannot be
/// allocated.
pub fn packet_from_bytes(data: &[u8]) -> Result<rsmpeg::avcodec::AVPacket, BridgeError> {
    let size = i32::try_from(data.len())
        .map_err(|_| BridgeError::InvalidInput("a packet is larger than FFmpeg holds"))?;
    let mut packet = rsmpeg::avcodec::AVPacket::new();
    // SAFETY: `packet` is a freshly allocated, empty packet; `av_new_packet` gives it a
    // reference-counted, zero-padded buffer of `size` bytes or fails without touching it.
    let status = unsafe { ffi::av_new_packet(packet.as_mut_ptr(), size) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    if !data.is_empty() {
        // SAFETY: the packet's buffer holds `size` writable bytes and does not overlap `data`.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), packet.data, data.len()) };
    }
    Ok(packet)
}

/// Replaces the payload of `packet` with `data`, which is no longer than it,
/// keeping its timestamps, flags and side data.
///
/// # Errors
/// When `data` is longer than the payload, or the packet's buffer cannot be
/// made writable.
pub fn shrink_packet_to(
    packet: &mut rsmpeg::avcodec::AVPacket,
    data: &[u8],
) -> Result<(), BridgeError> {
    let size = i32::try_from(data.len())
        .ok()
        .filter(|size| *size <= packet.size)
        .ok_or(BridgeError::InvalidInput("a packet cannot grow in place"))?;
    // SAFETY: `packet` is a live packet; `av_packet_make_writable` gives it a buffer of its own
    // of at least `size` bytes, copying the payload when the buffer was shared.
    let status = unsafe { ffi::av_packet_make_writable(packet.as_mut_ptr()) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    if !data.is_empty() {
        // SAFETY: the packet's writable buffer holds at least `data.len()` bytes and does not
        // overlap `data`, which the caller owns.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), packet.data, data.len()) };
    }
    // SAFETY: `size` is not above the packet's size; FFmpeg zeroes the padding after it.
    unsafe { ffi::av_shrink_packet(packet.as_mut_ptr(), size) };
    Ok(())
}

/// What the codec parameters of a video stream no `FFmpeg` encoder produced
/// say: the fields an encoder's `avcodec_parameters_from_context` would
/// fill in for it.
#[derive(Clone, Copy, Debug)]
pub struct VideoStreamDescription<'a> {
    /// The codec.
    pub codec_id: ffi::AVCodecID,
    /// Coded width.
    pub width: i32,
    /// Coded height.
    pub height: i32,
    /// The pixel format the stream decodes to.
    pub pixel_format: ffi::AVPixelFormat,
    /// `AV_PROFILE_*` for the codec.
    pub profile: i32,
    /// The codec's level number.
    pub level: i32,
    /// Colour primaries tag.
    pub color_primaries: ffi::AVColorPrimaries,
    /// Transfer characteristics tag.
    pub color_trc: ffi::AVColorTransferCharacteristic,
    /// Matrix coefficients tag.
    pub color_space: ffi::AVColorSpace,
    /// Sample range.
    pub color_range: ffi::AVColorRange,
    /// Pixel aspect ratio; `0/1` when undeclared.
    pub sample_aspect_ratio: ffi::AVRational,
    /// Frames per second.
    pub frame_rate: ffi::AVRational,
    /// Average bitrate, `0` when unknown.
    pub bit_rate: i64,
    /// The sample description (`avcC` for H.264).
    pub extradata: &'a [u8],
}

/// Codec parameters for a video stream `description` describes.
///
/// # Errors
/// When the extradata is larger than `FFmpeg` holds or cannot be allocated.
pub fn video_codec_parameters(
    description: &VideoStreamDescription<'_>,
) -> Result<rsmpeg::avcodec::AVCodecParameters, BridgeError> {
    let extradata_size = i32::try_from(description.extradata.len())
        .map_err(|_| BridgeError::InvalidInput("extradata is larger than FFmpeg holds"))?;
    let mut parameters = rsmpeg::avcodec::AVCodecParameters::new();
    let raw = parameters.as_mut_ptr();
    // SAFETY: `raw` is the freshly allocated parameters, uniquely owned here; the fields written
    // are plain values.
    unsafe {
        (*raw).codec_type = ffi::AVMEDIA_TYPE_VIDEO;
        (*raw).codec_id = description.codec_id;
        (*raw).width = description.width;
        (*raw).height = description.height;
        (*raw).format = description.pixel_format;
        (*raw).profile = description.profile;
        (*raw).level = description.level;
        (*raw).color_primaries = description.color_primaries;
        (*raw).color_trc = description.color_trc;
        (*raw).color_space = description.color_space;
        (*raw).color_range = description.color_range;
        (*raw).sample_aspect_ratio = description.sample_aspect_ratio;
        (*raw).framerate = description.frame_rate;
        (*raw).bit_rate = description.bit_rate;
        (*raw).field_order = ffi::AV_FIELD_PROGRESSIVE;
    }
    if !description.extradata.is_empty() {
        let padding = usize::try_from(ffi::AV_INPUT_BUFFER_PADDING_SIZE).unwrap_or(64);
        // SAFETY: `av_mallocz` returns null or a zeroed buffer of the size asked for.
        let buffer = unsafe { ffi::av_mallocz(description.extradata.len() + padding) }.cast::<u8>();
        if buffer.is_null() {
            return Err(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)));
        }
        // SAFETY: `buffer` holds the extradata's length in writable bytes and does not overlap
        // it. The parameters had no extradata, and now own `buffer`, which they free with
        // `av_free` as they free any.
        unsafe {
            ptr::copy_nonoverlapping(
                description.extradata.as_ptr(),
                buffer,
                description.extradata.len(),
            );
            (*raw).extradata = buffer;
            (*raw).extradata_size = extradata_size;
        }
    }
    Ok(parameters)
}

/// Opens a bitstream filter chain, written in `FFmpeg`'s list syntax
/// (`name=option=value:option=value,name...`, as `ffmpeg -bsf` takes it), for
/// a stream that `parameters` describes and whose packets are timed in
/// `time_base`.
///
/// rsmpeg 0.18 wraps a single filter context, but it can neither parse a
/// list nor set a filter's private options, and every filter the exports
/// chain needs one (`remove=1`, a VUI field). A filter or an option the
/// loaded `FFmpeg` lacks fails here, before any packet.
///
/// # Errors
/// Returns the `FFmpeg` status when the list does not parse, names a filter
/// or option the runtime lacks, or a filter refuses the stream.
pub fn bitstream_filter_list(
    spec: &CStr,
    parameters: &AVCodecParameters,
    time_base: rsmpeg::avutil::AVRational,
) -> Result<rsmpeg::avcodec::AVBSFContext, BridgeError> {
    let mut raw = ptr::null_mut();
    // SAFETY: `spec` is NUL-terminated for the call; on success FFmpeg stores a context it
    // allocated in `raw`, which is taken over below.
    let status = unsafe { ffi::av_bsf_list_parse_str(spec.as_ptr(), &raw mut raw) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    let raw = ptr::NonNull::new(raw).ok_or(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)))?;
    // SAFETY: `raw` is a context `av_bsf_list_parse_str` just allocated and nothing else holds; the
    // wrapper frees it with `av_bsf_free`.
    let mut context = unsafe { rsmpeg::avcodec::AVBSFContextUninit::from_raw(raw) };
    // rsmpeg's own `set_par_in` panics when the copy cannot allocate.
    // SAFETY: `par_in` is the context's own allocated parameter set, and `parameters` is a live one
    // borrowed for the call; `avcodec_parameters_copy` replaces the first with a deep copy.
    let status = unsafe { ffi::avcodec_parameters_copy(context.par_in, parameters.as_ptr()) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    context.set_time_base_in(time_base);
    context.init().map_err(BridgeError::Ffmpeg)
}

/// Sets the codec tag (sample entry) a stream is written with; zero lets the
/// output muxer pick the first tag its container uses for the codec.
pub fn set_stream_codec_tag(stream: &mut rsmpeg::avformat::AVStreamMut<'_>, tag: u32) {
    let mut parameters = stream.codecpar_mut();
    // SAFETY: a stream's codec parameters are always allocated and owned by the stream, which is
    // mutably borrowed here, and `codec_tag` is a plain integer field.
    unsafe { (*parameters.as_mut_ptr()).codec_tag = tag };
}

/// Gives a stream the display matrix `display` describes, as coded side data
/// a muxer writes (MP4's `tkhd` matrix): the rotation, then a horizontal
/// flip when it mirrors, as [`display_rotation`] reads one back. Replaces
/// any matrix the stream had.
///
/// # Errors
/// When the side data cannot be allocated.
pub fn set_stream_display_matrix(
    stream: &mut rsmpeg::avformat::AVStreamMut<'_>,
    display: DisplayRotation,
) -> Result<(), BridgeError> {
    let mut parameters = stream.codecpar_mut();
    let parameters = parameters.as_mut_ptr();
    // SAFETY: the stream's codec parameters are allocated and owned by the stream, mutably borrowed
    // here; `av_packet_side_data_new` replaces an entry of the type or appends one, keeping the
    // array and its count consistent, and returns null only when it cannot allocate.
    let entry = unsafe {
        ffi::av_packet_side_data_new(
            &raw mut (*parameters).coded_side_data,
            &raw mut (*parameters).nb_coded_side_data,
            ffi::AV_PKT_DATA_DISPLAYMATRIX,
            size_of::<[i32; 9]>(),
            0,
        )
    };
    if entry.is_null() {
        return Err(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)));
    }
    let mut matrix = [0_i32; 9];
    // `av_display_rotation_set` turns clockwise by its angle, the opposite of what
    // `av_display_rotation_get` reads back. SAFETY: `matrix` is a complete, aligned 3x3 display
    // matrix.
    unsafe {
        ffi::av_display_rotation_set(matrix.as_mut_ptr(), -display.counter_clockwise_degrees);
        if display.flipped {
            ffi::av_display_matrix_flip(matrix.as_mut_ptr(), 1, 0);
        }
    }
    // SAFETY: the entry just made holds `size_of::<[i32; 9]>()` writable bytes, which `matrix`
    // fills; copying bytes assumes no alignment.
    unsafe {
        ptr::copy_nonoverlapping(
            matrix.as_ptr().cast::<u8>(),
            (*entry).data,
            size_of::<[i32; 9]>(),
        );
    }
    Ok(())
}

/// Returns `FFmpeg`'s stable name for a hardware device type.
#[must_use]
pub fn hardware_device_type_name(device_type: ffi::AVHWDeviceType) -> Option<String> {
    // SAFETY: `av_hwdevice_get_type_name` accepts every enum value and returns either null or a
    // process-lifetime, NUL-terminated string.
    let name = unsafe { ffi::av_hwdevice_get_type_name(device_type) };
    if name.is_null() {
        None
    } else {
        // SAFETY: non-null return values obey the FFmpeg string contract above.
        Some(
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Reads the plane layout of a YCbCr pixel format the renderer samples
/// directly, or `None` for any other format.
///
/// Accepted: three components of one depth from 8 to 16 bits, chroma
/// subsampled by at most four on each axis, in three planes or with U, V
/// interleaved in the second, each sample in the low bits of its container
/// or, above eight bits, in the high bits, in this machine's byte order. Not
/// accepted: RGB, alpha, gray, palette, Bayer, float, XYZ, bitstream, and
/// hardware formats, and V-first interleaving (NV21).
#[must_use]
pub fn yuv_layout(format: ffi::AVPixelFormat) -> Option<YuvPlaneLayout> {
    if format == ffi::AV_PIX_FMT_NONE {
        return None;
    }
    // SAFETY: `av_pix_fmt_desc_get` accepts every enum value and returns either null or a pointer
    // into FFmpeg's process-lifetime descriptor table.
    let descriptor = unsafe { ffi::av_pix_fmt_desc_get(format).as_ref() }?;
    let foreign_endian = if cfg!(target_endian = "little") {
        ffi::AV_PIX_FMT_FLAG_BE
    } else {
        0
    };
    let rejected = foreign_endian
        | ffi::AV_PIX_FMT_FLAG_PAL
        | ffi::AV_PIX_FMT_FLAG_BITSTREAM
        | ffi::AV_PIX_FMT_FLAG_HWACCEL
        | ffi::AV_PIX_FMT_FLAG_RGB
        | ffi::AV_PIX_FMT_FLAG_ALPHA
        | ffi::AV_PIX_FMT_FLAG_BAYER
        | ffi::AV_PIX_FMT_FLAG_FLOAT
        | ffi::AV_PIX_FMT_FLAG_XYZ;
    if descriptor.flags & u64::from(rejected) != 0 || descriptor.nb_components != 3 {
        return None;
    }
    let [luma, u, v, _] = descriptor.comp;
    let depth = luma.depth;
    if !(8..=16).contains(&depth) || u.depth != depth || v.depth != depth {
        return None;
    }
    if descriptor.log2_chroma_w > 2 || descriptor.log2_chroma_h > 2 {
        return None;
    }
    let bytes = if depth > 8 { 2 } else { 1 };
    let shift = luma.shift;
    let msb_aligned = match shift {
        0 => false,
        _ if depth > 8 && shift == 16 - depth => true,
        _ => return None,
    };
    let planar = u.plane == 1
        && v.plane == 2
        && u.step == bytes
        && v.step == bytes
        && u.offset == 0
        && v.offset == 0;
    let interleaved = u.plane == 1
        && v.plane == 1
        && u.step == 2 * bytes
        && v.step == 2 * bytes
        && u.offset == 0
        && v.offset == bytes;
    if luma.plane != 0
        || luma.step != bytes
        || luma.offset != 0
        || u.shift != shift
        || v.shift != shift
        || !(planar || interleaved)
    {
        return None;
    }
    Some(YuvPlaneLayout {
        chroma_shift_x: descriptor.log2_chroma_w,
        chroma_shift_y: descriptor.log2_chroma_h,
        bit_depth: u8::try_from(depth).ok()?,
        interleaved_chroma: interleaved,
        msb_aligned,
    })
}

/// Copies the planes of a frame whose format [`yuv_layout`] accepts into
/// tightly packed, forward rows of its samples, or returns `Ok(None)` for any
/// other format so callers can convert it explicitly. Odd coded dimensions
/// use ceil-divided chroma dimensions.
///
/// Only each row's samples are read: the rest of a padded row may be memory
/// nothing wrote (a frame from `av_frame_get_buffer`, or a picture from
/// libdav1d's pool), so it is never handed on. [`DecodedFrame::yuv_planes`]
/// shares a decoder's frame instead when every byte of it is written.
///
/// # Errors
/// Returns an error for invalid dimensions, null planes, or linesizes shorter
/// than their plane width.
pub fn yuv_planes(frame: &AVFrame) -> Result<Option<YuvPlanes>, BridgeError> {
    frame_yuv_planes(frame, false)
}

/// A frame a decoder returned, and whether every byte of its buffers, row
/// padding included, is written.
///
/// Only this crate can say where a frame came from, so only it makes one:
/// [`DecodedFrame::receive`] takes one from a decoder, and a hardware frame
/// lease's `download` copies one out. A frame from anywhere else goes to
/// [`yuv_planes`], which copies its samples.
pub struct DecodedFrame {
    frame: AVFrame,
    /// Every byte of each plane's rows, up to its linesize, is initialized.
    written: bool,
}

impl DecodedFrame {
    /// Takes the next frame `decoder` has ready, as
    /// `AVCodecContext::receive_frame` does.
    ///
    /// # Errors
    /// Returns what `avcodec_receive_frame` does: a drained or flushed
    /// decoder, or a decode failure.
    pub fn receive(decoder: &mut AVCodecContext) -> Result<Self, RsmpegError> {
        let written = decoder_frames_are_initialized(decoder);
        let frame = decoder.receive_frame()?;
        Ok(Self { frame, written })
    }

    /// Hands over the planes of this frame, as [`yuv_planes`] does, but
    /// shares the frame's buffer when every byte of it is written: each plane
    /// is then a view of the buffer, row padding included, and the buffer
    /// stays alive, unmodified, as long as a view does. A plane that cannot be
    /// shared, with a negative linesize or outside every buffer the frame
    /// references, is copied into tightly packed, forward rows.
    ///
    /// # Errors
    /// Returns an error for invalid dimensions, null planes, or linesizes
    /// shorter than their plane width.
    pub fn yuv_planes(&self) -> Result<Option<YuvPlanes>, BridgeError> {
        frame_yuv_planes(&self.frame, self.written)
    }

    /// Sets the frame's presentation timestamp.
    pub fn set_pts(&mut self, pts: i64) {
        self.frame.set_pts(pts);
    }

    /// The frame itself, for a caller that takes ownership of it.
    #[must_use]
    pub fn into_frame(self) -> AVFrame {
        self.frame
    }
}

impl std::ops::Deref for DecodedFrame {
    type Target = AVFrame;

    fn deref(&self) -> &AVFrame {
        &self.frame
    }
}

impl fmt::Debug for DecodedFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DecodedFrame")
            .field("format", &self.frame.format)
            .field("width", &self.frame.width)
            .field("height", &self.frame.height)
            .field("pts", &self.frame.pts)
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

/// Whether every byte of every frame buffer `decoder` returns, row padding
/// included, is initialized, so its frames' buffers may be shared.
///
/// `FFmpeg`'s own decoders allocate pictures through the context's
/// `get_buffer2`, and its default draws on pools that zero each buffer when
/// they allocate it (or fill it with a poison byte in a memory-poisoning
/// build). A decoder backed by an external library (`wrapper_name`, such as
/// libdav1d) allocates its own pictures, from memory nothing zeroes, and a
/// replaced `get_buffer2` promises nothing either: neither qualifies.
fn decoder_frames_are_initialized(decoder: &AVCodecContext) -> bool {
    static DEFAULT_GET_BUFFER2: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // The default as `FFmpeg` itself installs it in a new context: comparing against Rust's own
    // address of `avcodec_default_get_buffer2` could compare a linker stub with the function.
    let default = *DEFAULT_GET_BUFFER2.get_or_init(|| {
        // SAFETY: a context allocated without a codec is only read and then freed.
        unsafe {
            let mut context = ffi::avcodec_alloc_context3(ptr::null());
            let get_buffer2 = context
                .as_ref()
                .and_then(|context| context.get_buffer2)
                .map_or(0, |function| function as usize);
            ffi::avcodec_free_context(&raw mut context);
            get_buffer2
        }
    });
    // SAFETY: a context's `codec` is null or points at the codec's static, read-only description.
    let wrapped =
        unsafe { decoder.codec.as_ref() }.is_none_or(|codec| !codec.wrapper_name.is_null());
    let get_buffer2 = decoder.get_buffer2.map_or(0, |function| function as usize);
    !wrapped && default != 0 && get_buffer2 == default
}

/// The planes of `frame`, sharing its buffer only with `share`, which the
/// caller passes only for a frame every byte of whose rows is initialized.
fn frame_yuv_planes(frame: &AVFrame, share: bool) -> Result<Option<YuvPlanes>, BridgeError> {
    let Some(layout) = yuv_layout(frame.format) else {
        return Ok(None);
    };
    let width =
        u32::try_from(frame.width).map_err(|_| BridgeError::InvalidInput("negative YUV width"))?;
    let height = u32::try_from(frame.height)
        .map_err(|_| BridgeError::InvalidInput("negative YUV height"))?;
    if width == 0 || height == 0 {
        return Err(BridgeError::InvalidInput("empty YUV frame"));
    }
    let bytes = layout.bytes_per_sample();
    let chroma_width = width.div_ceil(1 << layout.chroma_shift_x);
    let chroma_height = height.div_ceil(1 << layout.chroma_shift_y);
    // `AVFrame::clone` takes a new reference to every buffer of a reference-counted frame; one
    // without buffers can only be copied, and so is every frame the caller did not vouch for.
    let shared =
        (share && !frame.buf[0].is_null()).then(|| std::sync::Arc::new(SharedFrame(frame.clone())));
    let copy = |index: usize,
                samples: u32,
                pairs: u32,
                rows: u32,
                context: &'static str|
     -> Result<FramePlane, BridgeError> {
        let row_bytes = samples
            .checked_mul(bytes)
            .and_then(|row| row.checked_mul(pairs))
            .ok_or(BridgeError::InvalidInput(context))?;
        if let Some(view) = shared
            .as_ref()
            .and_then(|frame| PlaneView::new(frame, index, row_bytes, rows))
        {
            let stride =
                u32::try_from(view.stride).map_err(|_| BridgeError::InvalidInput(context))?;
            return Ok(FramePlane {
                bytes: bytes::Bytes::from_owner(view),
                stride,
                width: samples,
                height: rows,
            });
        }
        Ok(FramePlane {
            bytes: copy_video_plane(
                frame.data[index],
                frame.linesize[index],
                row_bytes,
                rows,
                context,
            )?
            .into(),
            stride: row_bytes,
            width: samples,
            height: rows,
        })
    };
    let mut planes = vec![copy(0, width, 1, height, "invalid Y plane")?];
    if layout.interleaved_chroma {
        planes.push(copy(1, chroma_width, 2, chroma_height, "invalid UV plane")?);
    } else {
        planes.push(copy(1, chroma_width, 1, chroma_height, "invalid U plane")?);
        planes.push(copy(2, chroma_width, 1, chroma_height, "invalid V plane")?);
    }
    Ok(Some(YuvPlanes {
        layout,
        width,
        height,
        planes,
    }))
}

/// A decoded frame whose buffers several plane views share.
struct SharedFrame(AVFrame);

// SAFETY: the frame is only read, through the data pointers of its planes, after it is wrapped;
// nothing mutates it or its buffers while it is shared, because every holder holds a reference and
// FFmpeg does not write to a buffer with more than one.
unsafe impl Sync for SharedFrame {}

/// `stride * rows` bytes of one plane of a [`SharedFrame`], kept alive by it:
/// made only of a frame every byte of whose rows is written.
struct PlaneView {
    frame: std::sync::Arc<SharedFrame>,
    data: *const u8,
    len: usize,
    stride: usize,
}

// SAFETY: the view reads immutable memory owned by the reference-counted buffers of `frame`, which
// it keeps alive; `SharedFrame` is `Send + Sync`.
unsafe impl Send for PlaneView {}

impl PlaneView {
    /// A view of plane `index`, or `None` unless its linesize is positive and
    /// covers `row_bytes`, and every one of its `rows` rows lies inside one
    /// of the buffers the frame references.
    fn new(
        frame: &std::sync::Arc<SharedFrame>,
        index: usize,
        row_bytes: u32,
        rows: u32,
    ) -> Option<Self> {
        let inner = &frame.0;
        let data = inner.data[index].cast_const();
        let stride = usize::try_from(inner.linesize[index]).ok()?;
        let row_bytes = usize::try_from(row_bytes).ok()?;
        if data.is_null() || stride < row_bytes || rows == 0 {
            return None;
        }
        let len = stride.checked_mul(usize::try_from(rows).ok()?)?;
        let start = data as usize;
        let end = start.checked_add(len)?;
        let inside = inner.buf.iter().any(|buffer| {
            // SAFETY: a non-null entry of `AVFrame::buf` points at a live `AVBufferRef` for as long
            // as the frame holds it.
            let Some(buffer) = (unsafe { buffer.as_ref() }) else {
                return false;
            };
            let buffer_start = buffer.data as usize;
            buffer_start
                .checked_add(buffer.size)
                .is_some_and(|buffer_end| buffer_start <= start && end <= buffer_end)
        });
        inside.then(|| Self {
            frame: std::sync::Arc::clone(frame),
            data,
            len,
            stride,
        })
    }
}

impl AsRef<[u8]> for PlaneView {
    fn as_ref(&self) -> &[u8] {
        // Keeps the frame, and so the buffer, alive for the slice's lifetime.
        let _ = &self.frame;
        // SAFETY: `new` checked that `data..data + len` lies inside a buffer the frame references,
        // and the frame lives as long as `self`.
        unsafe { slice::from_raw_parts(self.data, self.len) }
    }
}

fn copy_video_plane(
    data: *const u8,
    linesize: i32,
    width: u32,
    height: u32,
    context: &'static str,
) -> Result<Vec<u8>, BridgeError> {
    let row_bytes = usize::try_from(width).map_err(|_| BridgeError::InvalidInput(context))?;
    let rows = usize::try_from(height).map_err(|_| BridgeError::InvalidInput(context))?;
    let stride =
        usize::try_from(linesize.unsigned_abs()).map_err(|_| BridgeError::InvalidInput(context))?;
    if data.is_null() || linesize == 0 || stride < row_bytes {
        return Err(BridgeError::InvalidInput(context));
    }
    let byte_len = row_bytes
        .checked_mul(rows)
        .ok_or(BridgeError::InvalidInput(context))?;
    // Appending row by row writes each byte once; `vec![0; len]` would add a pass over ~11 MB
    // per 4K frame.
    let mut output = Vec::with_capacity(byte_len);
    let signed_stride =
        isize::try_from(linesize).map_err(|_| BridgeError::InvalidInput(context))?;
    for row in 0..rows {
        let row = isize::try_from(row).map_err(|_| BridgeError::InvalidInput(context))?;
        let offset = row
            .checked_mul(signed_stride)
            .ok_or(BridgeError::InvalidInput(context))?;
        // SAFETY: `AVFrame` guarantees that each non-null video plane exposes `height` rows
        // separated by its signed linesize. The absolute stride was validated to cover `width`
        // bytes, and each borrowed slice is limited to that visible row width.
        let source = unsafe { slice::from_raw_parts(data.offset(offset), row_bytes) };
        output.extend_from_slice(source);
    }
    debug_assert_eq!(output.len(), byte_len);
    Ok(output)
}

/// Copies tightly packed RGBA bytes into an owned `FFmpeg` frame.
///
/// # Errors
/// Returns an `FFmpeg` error when its frame buffer cannot be allocated.
pub fn rgba_frame(bytes: &[u8], width: i32, height: i32) -> Result<AVFrame, BridgeError> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|value| value.checked_mul(4))
        .ok_or(BridgeError::InvalidInput("RGBA row width overflow"))?;
    let rows =
        usize::try_from(height).map_err(|_| BridgeError::InvalidInput("negative RGBA height"))?;
    let expected = row_bytes
        .checked_mul(rows)
        .ok_or(BridgeError::InvalidInput("RGBA frame size overflow"))?;
    if bytes.len() != expected {
        return Err(BridgeError::InvalidInput("RGBA byte length mismatch"));
    }

    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_PIX_FMT_RGBA);
    frame.set_width(width);
    frame.set_height(height);
    frame.get_buffer(1)?;
    frame.make_writable()?;
    let linesize = usize::try_from(frame.linesize[0])
        .map_err(|_| BridgeError::InvalidInput("negative RGBA linesize"))?;
    if frame.data[0].is_null() || linesize < row_bytes {
        return Err(BridgeError::InvalidInput(
            "RGBA destination plane is too small",
        ));
    }

    // SAFETY: av_frame_get_buffer allocated `height` writable rows of at least `linesize` bytes.
    // Both slices are bounded to one validated RGBA row.
    unsafe {
        for row in 0..rows {
            let destination_offset = row.checked_mul(linesize).ok_or(BridgeError::InvalidInput(
                "RGBA destination offset overflow",
            ))?;
            let source_offset = row
                .checked_mul(row_bytes)
                .ok_or(BridgeError::InvalidInput("RGBA source offset overflow"))?;
            let destination =
                slice::from_raw_parts_mut(frame.data[0].add(destination_offset), row_bytes);
            destination.copy_from_slice(&bytes[source_offset..source_offset + row_bytes]);
        }
    }
    Ok(frame)
}

/// Copies owned planar 8-bit 4:2:0 samples into an `FFmpeg` video frame.
///
/// # Errors
/// Returns an error for invalid dimensions, short source planes, or an
/// `FFmpeg` allocation failure.
#[allow(clippy::too_many_arguments)]
pub fn yuv420p_frame(
    y: &[u8],
    y_stride: u32,
    u: &[u8],
    u_stride: u32,
    v: &[u8],
    v_stride: u32,
    width: i32,
    height: i32,
) -> Result<AVFrame, BridgeError> {
    let width =
        u32::try_from(width).map_err(|_| BridgeError::InvalidInput("negative YUV width"))?;
    let height =
        u32::try_from(height).map_err(|_| BridgeError::InvalidInput("negative YUV height"))?;
    if width == 0 || height == 0 {
        return Err(BridgeError::InvalidInput("empty YUV frame"));
    }
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    validate_source_plane(y, y_stride, width, height, "invalid Y source plane")?;
    validate_source_plane(
        u,
        u_stride,
        chroma_width,
        chroma_height,
        "invalid U source plane",
    )?;
    validate_source_plane(
        v,
        v_stride,
        chroma_width,
        chroma_height,
        "invalid V source plane",
    )?;

    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_PIX_FMT_YUV420P);
    frame.set_width(i32::try_from(width).map_err(|_| BridgeError::InvalidInput("YUV width"))?);
    frame.set_height(i32::try_from(height).map_err(|_| BridgeError::InvalidInput("YUV height"))?);
    frame.get_buffer(1)?;
    frame.make_writable()?;
    copy_plane_to_frame(y, y_stride, width, height, frame.data[0], frame.linesize[0])?;
    copy_plane_to_frame(
        u,
        u_stride,
        chroma_width,
        chroma_height,
        frame.data[1],
        frame.linesize[1],
    )?;
    copy_plane_to_frame(
        v,
        v_stride,
        chroma_width,
        chroma_height,
        frame.data[2],
        frame.linesize[2],
    )?;
    Ok(frame)
}

/// The pixel format whose planes are laid out as `layout`: the first in
/// `FFmpeg`'s table, which for 8-bit planar YUV is the limited-range format
/// (`yuv420p`) rather than its deprecated full-range twin (`yuvj420p`).
#[must_use]
pub fn yuv_pixel_format(layout: YuvPlaneLayout) -> Option<ffi::AVPixelFormat> {
    let mut descriptor = ptr::null();
    loop {
        // SAFETY: null starts the walk; any other value is a descriptor the previous call returned,
        // from `FFmpeg`'s process-lifetime table.
        descriptor = unsafe { ffi::av_pix_fmt_desc_next(descriptor) };
        if descriptor.is_null() {
            return None;
        }
        // SAFETY: a descriptor from the table names its own format.
        let format = unsafe { ffi::av_pix_fmt_desc_get_id(descriptor) };
        if yuv_layout(format) == Some(layout) {
            return Some(format);
        }
    }
}

/// A frame of `format` holding a copy of `planes` (bytes and stride each),
/// `width` by `height` pictures in the planes `format` stores.
///
/// # Errors
/// For a format without a descriptor, a plane count other than the
/// format's, or a plane smaller than the picture.
pub fn planar_frame(
    format: ffi::AVPixelFormat,
    planes: &[(&[u8], u32)],
    width: u32,
    height: u32,
) -> Result<AVFrame, BridgeError> {
    // SAFETY: `av_pix_fmt_desc_get` accepts every enum value and returns either null or a pointer
    // into FFmpeg's process-lifetime descriptor table.
    let descriptor = unsafe { ffi::av_pix_fmt_desc_get(format).as_ref() }
        .ok_or(BridgeError::InvalidInput("pixel format has no descriptor"))?;
    // SAFETY: a pure computation over plain values.
    let count = unsafe { ffi::av_pix_fmt_count_planes(format) };
    if usize::try_from(count).ok() != Some(planes.len()) || width == 0 || height == 0 {
        return Err(BridgeError::InvalidInput(
            "planes do not match the pixel format",
        ));
    }
    let signed = |value: u32| {
        i32::try_from(value).map_err(|_| BridgeError::InvalidInput("frame size overflow"))
    };
    let mut frame = AVFrame::new();
    frame.set_format(format);
    frame.set_width(signed(width)?);
    frame.set_height(signed(height)?);
    frame.get_buffer(1)?;
    frame.make_writable()?;
    for (index, (bytes, stride)) in planes.iter().enumerate() {
        let plane = i32::try_from(index).unwrap_or(i32::MAX);
        // SAFETY: a pure computation over plain values.
        let row_bytes = unsafe { ffi::av_image_get_linesize(format, signed(width)?, plane) };
        let row_bytes = u32::try_from(row_bytes).map_err(|_| BridgeError::FfmpegCode(row_bytes))?;
        let rows = if index == 0 || index == 3 {
            height
        } else {
            height.div_ceil(1 << descriptor.log2_chroma_h)
        };
        validate_source_plane(bytes, *stride, row_bytes, rows, "invalid source plane")?;
        copy_plane_to_frame(
            bytes,
            *stride,
            row_bytes,
            rows,
            frame.data[index],
            frame.linesize[index],
        )?;
    }
    Ok(frame)
}

fn validate_source_plane(
    bytes: &[u8],
    stride: u32,
    width: u32,
    height: u32,
    context: &'static str,
) -> Result<(), BridgeError> {
    let required = usize::try_from(stride).ok().and_then(|stride| {
        usize::try_from(height)
            .ok()
            .and_then(|height| stride.checked_mul(height))
    });
    if stride < width || required != Some(bytes.len()) {
        Err(BridgeError::InvalidInput(context))
    } else {
        Ok(())
    }
}

fn copy_plane_to_frame(
    source: &[u8],
    source_stride: u32,
    width: u32,
    height: u32,
    destination: *mut u8,
    destination_stride: i32,
) -> Result<(), BridgeError> {
    let row_bytes =
        usize::try_from(width).map_err(|_| BridgeError::InvalidInput("YUV row width overflow"))?;
    let rows =
        usize::try_from(height).map_err(|_| BridgeError::InvalidInput("YUV row count overflow"))?;
    let source_stride = usize::try_from(source_stride)
        .map_err(|_| BridgeError::InvalidInput("YUV source stride overflow"))?;
    let destination_stride = usize::try_from(destination_stride)
        .map_err(|_| BridgeError::InvalidInput("negative YUV destination stride"))?;
    if destination.is_null() || destination_stride < row_bytes {
        return Err(BridgeError::InvalidInput(
            "YUV destination plane is too small",
        ));
    }
    for row in 0..rows {
        let source_offset = row
            .checked_mul(source_stride)
            .ok_or(BridgeError::InvalidInput("YUV source offset overflow"))?;
        let destination_offset = row
            .checked_mul(destination_stride)
            .ok_or(BridgeError::InvalidInput("YUV destination offset overflow"))?;
        // SAFETY: the destination frame owns `height` writable rows whose validated linesize covers
        // `width` bytes. The source plane length was validated above against its positive stride
        // and row count.
        unsafe {
            ptr::copy_nonoverlapping(
                source.as_ptr().add(source_offset),
                destination.add(destination_offset),
                row_bytes,
            );
        }
    }
    Ok(())
}

/// Copies planar `f32` samples into an owned `FFmpeg` audio frame of
/// `format`: planar float (`AV_SAMPLE_FMT_FLTP`), interleaved float
/// (`AV_SAMPLE_FMT_FLT`), or interleaved 16-bit (`AV_SAMPLE_FMT_S16`). A
/// 16-bit sample is clamped to [-1, 1] and rounded to the nearest step.
///
/// # Errors
/// Returns an error for mismatched channels, invalid dimensions, another
/// sample format, or an `FFmpeg` allocation failure.
pub fn audio_frame(
    channels: &[Vec<f32>],
    format: ffi::AVSampleFormat,
    sample_rate: i32,
    pts: i64,
) -> Result<AVFrame, BridgeError> {
    let Some(samples) = channels.first().map(Vec::len) else {
        return Err(BridgeError::InvalidInput("invalid planar audio shape"));
    };
    if sample_rate <= 0
        || samples == 0
        || channels.len() > ffi::AV_NUM_DATA_POINTERS as usize
        || channels.iter().any(|channel| channel.len() != samples)
    {
        return Err(BridgeError::InvalidInput(
            "planar audio channel lengths differ",
        ));
    }
    let sample_bytes = match format {
        ffi::AV_SAMPLE_FMT_FLTP | ffi::AV_SAMPLE_FMT_FLT => size_of::<f32>(),
        ffi::AV_SAMPLE_FMT_S16 => size_of::<i16>(),
        _ => return Err(BridgeError::InvalidInput("unsupported audio sample format")),
    };
    let planar = format == ffi::AV_SAMPLE_FMT_FLTP;
    let channel_count = i32::try_from(channels.len())
        .map_err(|_| BridgeError::InvalidInput("audio channel count overflow"))?;
    let sample_count = i32::try_from(samples)
        .map_err(|_| BridgeError::InvalidInput("audio sample count overflow"))?;
    let mut frame = AVFrame::new();
    frame.set_format(format);
    frame.set_ch_layout(
        rsmpeg::avutil::AVChannelLayout::from_nb_channels(channel_count).into_inner(),
    );
    frame.set_sample_rate(sample_rate);
    frame.set_nb_samples(sample_count);
    frame.set_pts(pts);
    frame.set_time_base(ffi::AVRational {
        num: 1,
        den: sample_rate,
    });
    frame.get_buffer(0)?;
    frame.make_writable()?;
    let plane_bytes = samples
        .checked_mul(sample_bytes)
        .and_then(|bytes| bytes.checked_mul(if planar { 1 } else { channels.len() }))
        .ok_or(BridgeError::InvalidInput("audio plane byte count overflow"))?;
    let plane_capacity = usize::try_from(frame.linesize[0]).unwrap_or(0);
    if plane_capacity < plane_bytes {
        return Err(BridgeError::InvalidInput(
            "audio plane capacity is too small",
        ));
    }
    let planes = if planar { channels.len() } else { 1 };
    if frame.data[..planes].iter().any(|plane| plane.is_null()) {
        return Err(BridgeError::InvalidInput("audio plane pointer is null"));
    }
    if planar {
        for (index, source) in channels.iter().enumerate() {
            // SAFETY: av_frame_get_buffer allocated one writable FLTP plane per channel with the
            // common `linesize[0]` capacity used by planar audio, checked above to hold the
            // channel.
            unsafe {
                ptr::copy_nonoverlapping(
                    source.as_ptr().cast::<u8>(),
                    frame.data[index],
                    plane_bytes,
                );
            }
        }
        return Ok(frame);
    }
    // SAFETY: an interleaved format has one writable plane, checked above to hold every channel's
    // samples, and nothing else refers to it while this slice lives.
    let plane = unsafe { std::slice::from_raw_parts_mut(frame.data[0], plane_bytes) };
    let interleaved = (0..samples).flat_map(|sample| channels.iter().map(move |c| c[sample]));
    if format == ffi::AV_SAMPLE_FMT_FLT {
        for (bytes, value) in plane.as_chunks_mut::<4>().0.iter_mut().zip(interleaved) {
            *bytes = value.to_ne_bytes();
        }
    } else {
        for (bytes, value) in plane.as_chunks_mut::<2>().0.iter_mut().zip(interleaved) {
            // In range after the clamp, so the cast cannot truncate.
            #[allow(clippy::cast_possible_truncation)]
            let value = (value.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
            *bytes = value.to_ne_bytes();
        }
    }
    Ok(frame)
}

/// The `FFmpeg` channel (`enum AVChannel`) of each channel of `layout`, in
/// its order; empty for a layout that gives only a count
/// (`AV_CHANNEL_ORDER_UNSPEC`).
///
/// It takes rsmpeg's layout, which only `FFmpeg` or `unsafe` code builds
/// (`AVFrame::ch_layout`, `AVCodecParameters::ch_layout`): a bare
/// `AVChannelLayout` can carry a forged channel map.
///
/// ```compile_fail,E0308
/// use rsmpeg::ffi;
/// let forged = ffi::AVChannelLayout {
///     order: ffi::AV_CHANNEL_ORDER_CUSTOM,
///     nb_channels: 4,
///     u: ffi::AVChannelLayout__bindgen_ty_1 { map: 0x10 as *mut _ },
///     opaque: std::ptr::null_mut(),
/// };
/// alpha2_ffmpeg_bridge::channel_layout_channels(&forged);
/// ```
#[must_use]
pub fn channel_layout_channels(layout: &rsmpeg::avutil::AVChannelLayout) -> Vec<ffi::AVChannel> {
    if layout.order == ffi::AV_CHANNEL_ORDER_UNSPEC {
        return Vec::new();
    }
    let channels = u32::try_from(layout.nb_channels).unwrap_or(0);
    (0..channels)
        .map(|index| {
            layout
                .channel_from_index(index)
                .unwrap_or(ffi::AV_CHAN_NONE)
        })
        .collect()
}

/// A channel layout of `channels` in this order: `FFmpeg`'s native order
/// when they are in it, a custom order otherwise.
///
/// # Errors
/// Returns an error for no channels or when `FFmpeg` cannot allocate or
/// retype the layout.
pub fn channel_layout_of(
    channels: &[ffi::AVChannel],
) -> Result<rsmpeg::avutil::AVChannelLayout, BridgeError> {
    let count = i32::try_from(channels.len())
        .ok()
        .filter(|count| *count > 0)
        .ok_or(BridgeError::InvalidInput("a channel layout needs channels"))?;
    let mut raw = std::mem::MaybeUninit::<ffi::AVChannelLayout>::zeroed();
    // SAFETY: `custom_init` initializes the zeroed layout with a map of `count` entries, each of
    // which is written exactly once below.
    let initialized = unsafe { ffi::av_channel_layout_custom_init(raw.as_mut_ptr(), count) };
    if initialized < 0 {
        return Err(BridgeError::FfmpegCode(initialized));
    }
    // SAFETY: initialized above; from here `layout` owns it and uninitializes it on drop.
    let mut layout = unsafe { rsmpeg::avutil::AVChannelLayout::new(raw.assume_init()) };
    for (index, channel) in channels.iter().enumerate() {
        // SAFETY: a custom layout's map holds `count` entries, and `index` is below it.
        unsafe { (*layout.u.map.add(index)).id = *channel };
    }
    let flags = i32::try_from(ffi::AV_CHANNEL_LAYOUT_RETYPE_FLAG_CANONICAL).unwrap_or(0);
    // SAFETY: `layout` is a valid custom layout that `retype` may rewrite in place.
    let retyped = unsafe {
        ffi::av_channel_layout_retype(layout.as_mut_ptr(), ffi::AV_CHANNEL_ORDER_UNSPEC, flags)
    };
    if retyped < 0 {
        return Err(BridgeError::FfmpegCode(retyped));
    }
    Ok(layout)
}

/// Copies a packed `AV_SAMPLE_FMT_FLT` frame into an owned interleaved vector.
///
/// # Errors
/// Returns an error if the frame format, dimensions, or buffer is invalid.
pub fn audio_f32_interleaved(frame: &AVFrame) -> Result<Vec<f32>, BridgeError> {
    if frame.format != ffi::AV_SAMPLE_FMT_FLT || frame.nb_samples < 0 {
        return Err(BridgeError::InvalidInput(
            "packed audio format or sample count is invalid",
        ));
    }
    let channels = usize::try_from(frame.ch_layout().nb_channels)
        .map_err(|_| BridgeError::InvalidInput("packed audio channel count is negative"))?;
    let frames = usize::try_from(frame.nb_samples)
        .map_err(|_| BridgeError::InvalidInput("packed audio sample count is negative"))?;
    let samples = frames
        .checked_mul(channels)
        .ok_or(BridgeError::InvalidInput(
            "packed audio sample count overflow",
        ))?;
    let bytes = samples
        .checked_mul(size_of::<f32>())
        .ok_or(BridgeError::InvalidInput(
            "packed audio byte count overflow",
        ))?;
    if channels == 0
        || frame.data[0].is_null()
        || usize::try_from(frame.linesize[0]).unwrap_or(0) < bytes
    {
        return Err(BridgeError::InvalidInput(
            "packed audio plane is missing or too small",
        ));
    }
    let mut output = vec![0.0_f32; samples];
    // SAFETY: the packed FLT plane was validated to contain every requested sample, and `output`
    // owns exactly the destination byte count.
    unsafe {
        ptr::copy_nonoverlapping(frame.data[0], output.as_mut_ptr().cast::<u8>(), bytes);
    }
    Ok(output)
}

/// The rotation a display matrix asks for, in `FFmpeg`'s convention.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DisplayRotation {
    /// Counter-clockwise angle in degrees, in `[-180, 180]`
    /// (`av_display_rotation_get`).
    pub counter_clockwise_degrees: f64,
    /// Whether the matrix also mirrors the picture. The matrix is then this
    /// rotation followed by a horizontal flip, and the angle is the rotation
    /// left once that flip is removed.
    pub flipped: bool,
}

/// Reads a stream display matrix.
#[must_use]
pub fn display_rotation(parameters: &AVCodecParameters) -> Option<DisplayRotation> {
    let count = usize::try_from(parameters.nb_coded_side_data).ok()?;
    if count == 0 || count > 64 || parameters.coded_side_data.is_null() {
        return None;
    }
    // SAFETY: AVCodecParameters owns an array containing exactly `nb_coded_side_data` entries for
    // the lifetime of this borrow.
    let entries = unsafe { slice::from_raw_parts(parameters.coded_side_data, count) };
    let matrix = entries.iter().find(|entry| {
        entry.type_ == ffi::AV_PKT_DATA_DISPLAYMATRIX
            && entry.size >= 9 * size_of::<i32>()
            && !entry.data.is_null()
    })?;
    // SAFETY: the matching side-data entry was validated to hold a complete 3x3 display matrix.
    unsafe { matrix_rotation(matrix.data) }
}

/// Reads the display matrix a decoder attached to one frame, as the JPEG
/// decoder does for an Exif `Orientation`.
#[must_use]
pub fn frame_display_rotation(frame: &AVFrame) -> Option<DisplayRotation> {
    // SAFETY: `frame` is a live frame; the returned entry, if any, is owned by it for this borrow.
    let entry =
        unsafe { ffi::av_frame_get_side_data(frame.as_ptr(), ffi::AV_FRAME_DATA_DISPLAYMATRIX) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: a non-null entry returned by `av_frame_get_side_data` is a valid side-data record.
    let entry = unsafe { &*entry };
    if entry.size < 9 * size_of::<i32>() || entry.data.is_null() {
        return None;
    }
    // SAFETY: the entry was validated to hold a complete 3x3 display matrix.
    unsafe { matrix_rotation(entry.data) }
}

/// # Safety
/// `data` must point at nine readable `i32`s, aligned or not.
unsafe fn matrix_rotation(data: *const u8) -> Option<DisplayRotation> {
    let mut aligned = [0_i32; 9];
    // SAFETY: the caller guarantees a complete 3x3 matrix. Copying as bytes avoids assuming source
    // alignment.
    unsafe {
        ptr::copy_nonoverlapping(
            data,
            aligned.as_mut_ptr().cast::<u8>(),
            size_of::<[i32; 9]>(),
        );
    }
    // A mirroring matrix has a negative determinant in its 2x2 part, and `av_display_rotation_get`
    // reads a meaningless angle from one. Removing the horizontal flip, which `FFmpeg` applies
    // after the rotation, leaves a pure rotation.
    let determinant = i64::from(aligned[0]) * i64::from(aligned[4])
        - i64::from(aligned[1]) * i64::from(aligned[3]);
    let flipped = determinant < 0;
    if flipped {
        // SAFETY: `aligned` is a complete, correctly aligned 3x3 matrix.
        unsafe { ffi::av_display_matrix_flip(aligned.as_mut_ptr(), 1, 0) };
    }
    // SAFETY: `aligned` is a complete, correctly aligned 3x3 display matrix.
    let angle = unsafe { ffi::av_display_rotation_get(aligned.as_ptr()) };
    angle.is_finite().then_some(DisplayRotation {
        counter_clockwise_degrees: angle,
        flipped,
    })
}

const fn size_of<T>() -> usize {
    std::mem::size_of::<T>()
}

/// A reusable `FFmpeg` YUV420P frame the caller writes into directly.
///
/// [`yuv420p_frame`] allocates a frame and copies three planes into it on
/// whichever thread calls it, which for an export is the encode worker. A
/// caller that can produce the pixels itself (a GPU readback packing straight
/// into encoder memory) wants the frame allocated once and rewritten, so
/// neither the allocation nor the copy happens per frame.
///
/// The buffer stays valid while `FFmpeg` holds a reference to it, which it
/// does between `send_frame` and the packet that consumes it.
/// [`Yuv420pFrameBuffer::begin_write`] is what makes reuse safe: it returns
/// the plane memory only once the buffer is no longer shared, copying it away
/// from `FFmpeg` if it still is.
pub struct Yuv420pFrameBuffer {
    frame: AVFrame,
    width: usize,
    height: usize,
    /// Each plane's visible bytes per row and rows, for a frame of another
    /// format ([`Self::with_format`]); empty for YUV420P.
    planes: Vec<(usize, usize)>,
}

/// Writable plane memory for one reusable frame.
pub struct Yuv420pFramePlanes<'a> {
    /// Luma rows, `y_stride` bytes apart.
    pub y: &'a mut [u8],
    /// Distance between luma rows.
    pub y_stride: usize,
    /// Blue-difference chroma rows, `chroma_stride` bytes apart.
    pub u: &'a mut [u8],
    /// Red-difference chroma rows, `chroma_stride` bytes apart.
    pub v: &'a mut [u8],
    /// Distance between chroma rows.
    pub chroma_stride: usize,
}

impl Yuv420pFrameBuffer {
    /// Allocates one reusable frame for this exact coded size.
    ///
    /// # Errors
    /// Returns an error for a non-positive size or a failed allocation.
    pub fn new(width: i32, height: i32) -> Result<Self, BridgeError> {
        let coded_width =
            usize::try_from(width).map_err(|_| BridgeError::InvalidInput("negative YUV width"))?;
        let coded_height = usize::try_from(height)
            .map_err(|_| BridgeError::InvalidInput("negative YUV height"))?;
        if coded_width == 0 || coded_height == 0 {
            return Err(BridgeError::InvalidInput("empty YUV frame"));
        }
        Ok(Self {
            frame: zeroed_frame(width, height, ffi::AV_PIX_FMT_YUV420P)?,
            width: coded_width,
            height: coded_height,
            planes: Vec::new(),
        })
    }

    /// Allocates one reusable frame of `format` for this exact coded size,
    /// whose planes are `planes`: each one's visible bytes per row and rows,
    /// in `FFmpeg`'s plane order. [`Self::begin_write_planes`] writes it.
    ///
    /// # Errors
    /// Returns an error for a non-positive size, a failed allocation, or
    /// planes the format does not have.
    pub fn with_format(
        width: i32,
        height: i32,
        format: ffi::AVPixelFormat,
        planes: &[(usize, usize)],
    ) -> Result<Self, BridgeError> {
        let coded_width = usize::try_from(width)
            .map_err(|_| BridgeError::InvalidInput("negative frame width"))?;
        let coded_height = usize::try_from(height)
            .map_err(|_| BridgeError::InvalidInput("negative frame height"))?;
        if coded_width == 0 || coded_height == 0 || planes.is_empty() || planes.len() > 4 {
            return Err(BridgeError::InvalidInput("empty frame"));
        }
        let frame = zeroed_frame(width, height, format)?;
        for (index, &(row_bytes, _)) in planes.iter().enumerate() {
            if frame.data[index].is_null()
                || plane_stride(frame.linesize[index], row_bytes).is_err()
            {
                return Err(BridgeError::InvalidInput(
                    "the frame's planes are not the ones described",
                ));
            }
        }
        Ok(Self {
            frame,
            width: coded_width,
            height: coded_height,
            planes: planes.to_vec(),
        })
    }

    /// Each plane's memory to overwrite and its stride, after making the
    /// frame unshared, for a frame of any format: Y, U and V for YUV420P.
    ///
    /// # Errors
    /// As [`Self::begin_write`].
    pub fn begin_write_planes(&mut self) -> Result<Vec<(&mut [u8], usize)>, BridgeError> {
        if self.planes.is_empty() {
            let planes = self.begin_write()?;
            let chroma = planes.chroma_stride;
            return Ok(vec![
                (planes.y, planes.y_stride),
                (planes.u, chroma),
                (planes.v, chroma),
            ]);
        }
        self.make_writable()?;
        let mut written = Vec::with_capacity(self.planes.len());
        for (index, &(row_bytes, rows)) in self.planes.iter().enumerate() {
            let stride = plane_stride(self.frame.linesize[index], row_bytes)?;
            // SAFETY: `zeroed_frame` allocated each plane `rows` rows of `linesize` bytes, checked
            // against the described planes when the buffer was made; `make_writable` left this
            // frame the only owner of that memory.
            let plane = unsafe { writable_plane(self.frame.data[index], stride, row_bytes, rows)? };
            written.push((plane, stride));
        }
        Ok(written)
    }

    /// Makes the frame the only owner of its memory, copying it away from
    /// `FFmpeg` if it is still shared.
    fn make_writable(&mut self) -> Result<(), BridgeError> {
        // `av_frame_make_writable` would copy into rows padded for SIMD and leave that padding
        // unwritten, under slices that span it. The copy goes to a frame allocated as this one was,
        // written through.
        if !self.frame.is_writable().map_err(BridgeError::Ffmpeg)? {
            let mut copy = zeroed_frame(self.frame.width, self.frame.height, self.frame.format)?;
            // SAFETY: both frames are live frames of one size and format; `copy` is uniquely
            // owned, and both calls only read `self.frame`.
            let status = unsafe {
                let copied = ffi::av_frame_copy(copy.as_mut_ptr(), self.frame.as_ptr());
                if copied < 0 {
                    copied
                } else {
                    ffi::av_frame_copy_props(copy.as_mut_ptr(), self.frame.as_ptr())
                }
            };
            if status < 0 {
                return Err(BridgeError::FfmpegCode(status));
            }
            self.frame = copy;
        }
        Ok(())
    }

    /// Luma width and height in pixels.
    #[must_use]
    pub const fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Returns the plane memory to overwrite, after making the frame unshared.
    ///
    /// # Errors
    /// Returns an error when `FFmpeg` cannot make the buffer writable or its
    /// planes are not laid out as this frame was allocated.
    pub fn begin_write(&mut self) -> Result<Yuv420pFramePlanes<'_>, BridgeError> {
        if !self.planes.is_empty() {
            return Err(BridgeError::InvalidInput("the frame is not YUV420P"));
        }
        self.make_writable()?;
        let chroma_width = self.width.div_ceil(2);
        let chroma_height = self.height.div_ceil(2);
        let y_stride = plane_stride(self.frame.linesize[0], self.width)?;
        let u_stride = plane_stride(self.frame.linesize[1], chroma_width)?;
        let v_stride = plane_stride(self.frame.linesize[2], chroma_width)?;
        if u_stride != v_stride {
            return Err(BridgeError::InvalidInput("mismatched chroma strides"));
        }
        // SAFETY: `zeroed_frame` allocated and wrote each plane, `height` rows of
        // `linesize` bytes, so the last row's visible width is the furthest byte any caller may
        // touch; the writability check above guarantees this frame is the only owner of that
        // memory.
        let y = unsafe { writable_plane(self.frame.data[0], y_stride, self.width, self.height)? };
        // SAFETY: as above, for the two half-resolution chroma planes.
        let u =
            unsafe { writable_plane(self.frame.data[1], u_stride, chroma_width, chroma_height)? };
        // SAFETY: as above.
        let v =
            unsafe { writable_plane(self.frame.data[2], v_stride, chroma_width, chroma_height)? };
        Ok(Yuv420pFramePlanes {
            y,
            y_stride,
            u,
            v,
            chroma_stride: u_stride,
        })
    }

    /// Stamps the presentation timestamp this frame will be encoded with.
    pub fn set_presentation(&mut self, pts: i64, time_base: ffi::AVRational) {
        self.frame.set_pts(pts);
        self.frame.set_time_base(time_base);
    }

    /// The frame to hand to an encoder.
    #[must_use]
    pub const fn frame(&self) -> &AVFrame {
        &self.frame
    }

    /// Coded luma dimensions this buffer was allocated for.
    #[must_use]
    pub const fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

/// A frame of `format` with tightly packed rows, every byte of it zero.
fn zeroed_frame(
    width: i32,
    height: i32,
    format: ffi::AVPixelFormat,
) -> Result<AVFrame, BridgeError> {
    let mut frame = AVFrame::new();
    frame.set_format(format);
    frame.set_width(width);
    frame.set_height(height);
    frame.get_buffer(1)?;
    for buffer in frame.buf {
        // SAFETY: a non-null entry of `AVFrame::buf` points at a live `AVBufferRef`; this frame was
        // just allocated, so it is the buffer's only reference, `size` bytes long.
        if let Some(buffer) = unsafe { buffer.as_ref() } {
            // SAFETY: as above: `data..data + size` is this frame's own, unshared allocation.
            unsafe { ptr::write_bytes(buffer.data, 0, buffer.size) };
        }
    }
    Ok(frame)
}

impl fmt::Debug for Yuv420pFrameBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Yuv420pFrameBuffer")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

fn plane_stride(linesize: i32, width: usize) -> Result<usize, BridgeError> {
    let stride = usize::try_from(linesize)
        .map_err(|_| BridgeError::InvalidInput("negative frame linesize"))?;
    if stride < width {
        return Err(BridgeError::InvalidInput(
            "frame linesize is below its width",
        ));
    }
    Ok(stride)
}

/// # Safety
/// `data` must point at `height` rows of `stride` bytes allocated for this
/// frame, and the caller must hold the only reference to them.
unsafe fn writable_plane<'a>(
    data: *mut u8,
    stride: usize,
    width: usize,
    height: usize,
) -> Result<&'a mut [u8], BridgeError> {
    if data.is_null() {
        return Err(BridgeError::InvalidInput("frame plane is null"));
    }
    let len = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|rows| rows.checked_add(width))
        .ok_or(BridgeError::InvalidInput("frame plane size overflow"))?;
    // SAFETY: delegated to this function's contract.
    Ok(unsafe { slice::from_raw_parts_mut(data, len) })
}

#[cfg(all(test, target_os = "linux", feature = "hwdevice"))]
mod vulkan_shim_tests;

#[cfg(all(test, target_os = "linux", feature = "hwdevice"))]
mod vulkan_access_tests;

#[cfg(test)]
mod tests;
