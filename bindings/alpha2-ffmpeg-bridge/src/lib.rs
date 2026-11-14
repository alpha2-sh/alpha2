//! The `FFmpeg` calls rsmpeg's safe surface lacks, for the native binding: frame and packet
//! copies, codec and side-data helpers, container I/O and hardware-frame shims.

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

/// The C shims' ABI, generated from `include/` by the build script.
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

// Pins the layouts of the D3D12 and VideoToolbox interop structs so a header edit fails on every
// target, not only where those shims compile.
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

/// Copies a hardware frame into a software frame in its `sw_format`, keeping timestamps, crop and
/// color properties. Used for surfaces the renderer could not import.
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
    // The copy's row padding is not zeroed and some transfers write only the frames context's
    // width, but `DecodedFrame::yuv_planes` shares whole rows.
    zero_row_padding(&mut software, written.0, written.1)?;
    // SAFETY: both frames are live; `av_frame_copy_props` only reads `frame` and writes metadata
    // into `software`.
    let copied = unsafe { ffi::av_frame_copy_props(software.as_mut_ptr(), frame.as_ptr()) };
    if copied < 0 {
        return Err(BridgeError::FfmpegCode(copied));
    }
    Ok(software)
}

/// Zeroes each row of the first `height` rows past `width` pixels, up to the linesize.
///
/// `frame` must be the only owner of its buffers.
///
/// # Errors
/// Returns an error for a format without a descriptor, a negative size, or rows outside the
/// frame's buffers.
#[cfg_attr(not(feature = "hwdevice"), allow(dead_code))]
fn zero_row_padding(frame: &mut AVFrame, width: i32, height: i32) -> Result<(), BridgeError> {
    let format = frame.format;
    // SAFETY: av_pix_fmt_desc_get only reads the static descriptor table; null for unknown formats.
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
            // SAFETY: a non-null `AVFrame::buf` entry is a live `AVBufferRef` the frame holds.
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
            // SAFETY: the check above puts every row inside a frame buffer the caller solely owns.
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

/// `DXGI_FORMAT_NV12`, the D3D12VA encoder-input texture format.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub const DXGI_FORMAT_NV12: u32 = 103;

/// Sets a D3D12VA frames context's `dxgi_format` and `resource_flags`, with one texture per frame.
///
/// Encoder input needs one texture per frame because each frame has its own fence and a copy
/// addresses its planes as subresources 0 and 1. Call between `av_hwframe_ctx_alloc` and
/// `av_hwframe_ctx_init`.
///
/// # Errors
/// Returns an error when the context is not a D3D12VA one.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub fn d3d12_frames_configure(
    frames: &mut rsmpeg::avutil::AVHWFramesContext,
    dxgi_format: u32,
    resource_flags: u32,
) -> Result<(), BridgeError> {
    // SAFETY: `frames` is live; the shim checks the device type before writing D3D12VA fields.
    let status =
        unsafe { alpha2_d3d12_frames_configure(frames.as_mut_ptr(), dxgi_format, resource_flags) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    Ok(())
}

/// D3D12 resource and decode fence retained by one hardware frame.
///
/// Defined on every target so a change to the shim's record fails to compile everywhere.
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
    /// Width with the right crop applied but not the left.
    pub width: i32,
    /// Height with the bottom crop applied but not the top.
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

    /// Copies this frame's pixels to the CPU. Row padding is zeroed, so the planes can be shared.
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
/// `device` must be the address of a live `ID3D12Device`. The context takes its own COM reference.
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

/// The fence value `FFmpeg` waits for before next using the frame (`sync_ctx.fence_value`).
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
/// The encoder waits on the frame's fence for `sync_ctx.fence_value` before reading it, so after a
/// fenced copy is submitted, [`Self::commit`] records the value it signals. A guard dropped without
/// a commit leaves the frame unchanged.
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

    /// The frame's texture, fence and fence value as `begin` read them.
    #[must_use]
    pub const fn info(&self) -> &D3D12FrameInfo {
        &self.info
    }

    /// Retains the frame's buffers and hardware pool beyond this access guard.
    ///
    /// # Errors
    /// Returns an allocation error if `FFmpeg` cannot create the reference.
    pub fn retain_frame(&self) -> Result<AVFrame, BridgeError> {
        // SAFETY: the frame is live; av_frame_clone makes an independent AVFrame with buffer refs.
        let raw = unsafe { ffi::av_frame_clone(self.frame.as_ptr()) };
        let raw = std::ptr::NonNull::new(raw)
            .ok_or(BridgeError::InvalidInput("cannot retain encode frame"))?;
        // SAFETY: the newly allocated frame is uniquely owned by this value.
        Ok(unsafe { AVFrame::from_raw(raw) })
    }

    /// Records the fence value a submitted write into the frame signals.
    ///
    /// # Errors
    /// Returns an error if the frame is not a valid D3D12VA frame (`begin` already checked).
    pub fn commit(&mut self, fence_value: u64) -> Result<(), BridgeError> {
        // SAFETY: the guard borrows the frame exclusively; the shim checks its type before writing.
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

// The VideoToolbox shim reads only FFmpeg structures, so Linux builds and tests it too.
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
/// Defined on every target, like [`D3D12FrameInfo`].
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

/// Reads a `VideoToolbox` frame's descriptor, refusing other frames.
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn read_videotoolbox_frame_info(frame: &AVFrame) -> Result<VideoToolboxFrameInfo, BridgeError> {
    let mut descriptor = VideoToolboxFrameDescriptor::default();
    // SAFETY: `frame` is live for the copy; the shim checks each pointer before reading through it.
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

    /// Copies this frame's pixels to the CPU. Row padding is zeroed, so the planes can be shared.
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
/// Takes the same recursive lock `FFmpeg` takes around its submissions. Only contexts made by
/// [`create_vulkan_device_context`] have it.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub struct VulkanQueueLock {
    device: AVHWDeviceContext,
    queue_family: u32,
}

// SAFETY: after construction the context is only read to pass its pointer to the shim's lock and
// unlock, which synchronize on their own mutex. The single owner drops the reference once.
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
    /// Returns an input error unless `device` came from [`create_vulkan_device_context`] and has a
    /// lock for `queue_family`.
    #[allow(clippy::cast_ptr_alignment)]
    pub fn new(device: AVHWDeviceContext, queue_family: u32) -> Result<Self, BridgeError> {
        // SAFETY: `device` keeps the context live; the shim reads only its type, free callback and
        // its own state.
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

    /// Holds the queue lock until the guard drops; the guard stays on this thread.
    #[must_use]
    pub fn lock(&self) -> VulkanQueueGuard<'_> {
        // SAFETY: the guard is neither Send nor Sync and releases this exactly once.
        unsafe { self.raw_lock() };
        VulkanQueueGuard {
            lock: self,
            _owning_thread: std::marker::PhantomData,
        }
    }

    /// Acquires the queue lock without a guard, for adapters with their own lock/unlock protocol.
    ///
    /// # Safety
    /// The calling thread must release it with exactly one [`Self::raw_unlock`], in reverse order
    /// of its other holds on this lock.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn raw_lock(&self) {
        // SAFETY: `new` checked the shim set up this context with a lock for this family, and the
        // retained context keeps it alive.
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
    /// The calling thread must hold the lock from an unreleased [`Self::raw_lock`], and this must
    /// be its most recent hold. An unbalanced release lets two submissions reach the queue at once.
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

/// One hold of a [`VulkanQueueLock`], released on drop on the thread that took it.
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
/// The handles in `info` must be live, belong together, have been created with the queues,
/// features and extensions `info` names, and outlive the returned context.
///
/// # Errors
/// Returns an input error for invalid extension names or an `FFmpeg` error when the context cannot
/// be initialized.
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
    // SAFETY: the context is freshly allocated and writable, the shim copies the extension names,
    // and the caller guarantees the handles.
    let result = unsafe { alpha2_vulkan_device_init(device_context, &raw const raw) };
    if result < 0 {
        return Err(BridgeError::FfmpegCode(result));
    }
    context.init().map_err(BridgeError::Ffmpeg)?;
    Ok(context)
}

/// Opaque Vulkan handles and synchronization state for one `FFmpeg` hardware frame.
///
/// The caller must not destroy the handles and must retain the source `AVFrame` while using them.
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
    /// Allocated image width, padded to whole macroblocks.
    pub coded_width: i32,
    /// Allocated image height, padded to whole macroblocks.
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

/// Vulkan image and synchronization state of one hardware frame, read under the frame lock.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub type VulkanFrameDescriptor = interop::Alpha2VulkanFrameInfo;

/// Locked `FFmpeg` Vulkan surface state passed to the native GPU boundary.
///
/// Editing a record changes no frame by itself. The record a guard writes back is reachable only
/// through the `unsafe` `access_mut`, since its values must describe work actually submitted.
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

    /// Mutable frame state; see [`VulkanFrameAccessGuard::access_mut`] for what it must hold.
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

/// Why the Vulkan shim refused a frame (its own codes, not `FFmpeg`'s).
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

    /// Copies this frame's pixels to the CPU. Row padding is zeroed, so the planes can be shared.
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
        // SAFETY: the mutex guard borrows the frame exclusively until the end call in Drop.
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
/// Safe code cannot change what is written back: an unsignaled timeline value hangs later users and
/// a wrong layout makes the contents undefined.
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

    /// The record written back on [`Self::commit`]. Planes whose image or semaphore changed and
    /// timeline values below the frame's are ignored.
    ///
    /// # Safety
    /// On commit, each layout and access mask must be what the submitted work leaves the image in,
    /// and each timeline value one that work submitted before the guard drops signals.
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
        // SAFETY: `begun` is this frame's begin record on this thread (the guard is not Send), and
        // this is the unique end call. `access_mut`'s caller vouched for `access`.
        unsafe { alpha2_vulkan_frame_access_end(&raw const self.begun, updated) };
    }
}

/// One encoder-pool frame held open for a native GPU write.
///
/// Unlike a [`VulkanFrameLease`], an encoder-input frame has one owner, so it is borrowed directly.
/// The guard stays on the thread that began it, which holds the frame and queue mutexes.
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
        // SAFETY: the frame is live and locked; av_frame_clone makes an independent AVFrame.
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
        // SAFETY: `frame` is borrowed until the guard drops, and Drop is the unique end call.
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

    /// The record written back on [`Self::commit`], checked as in
    /// [`VulkanFrameAccessGuard::access_mut`].
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
        // SAFETY: `begun` is this frame's begin record on this thread (the guard is not Send), and
        // this is the unique end call. `access_mut`'s caller vouched for `access`.
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
/// The shim takes the frame lock while copying layouts and semaphore values.
///
/// # Errors
/// Returns an error if the frame is not a valid Vulkan hardware frame.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub fn vulkan_frame_info(frame: &AVFrame) -> Result<VulkanFrameInfo, BridgeError> {
    let mut raw = VulkanFrameDescriptor::default();
    // SAFETY: `frame` is live and `raw` is a writable result; the shim checks FFmpeg's pointers
    // before dereferencing them.
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

/// rsmpeg's `alloc` unwraps the null context returned for a missing type, so check the type first.
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
    /// An `FFmpeg` operation returned a negative `AVERROR` code (platform GPU statuses are
    /// [`Self::NativeGpu`]).
    FfmpegCode(i32),
    /// A platform GPU call in a native interop helper failed.
    NativeGpu(NativeGpuFailure),
    /// The loaded `FFmpeg` runtime was built without this hardware device type.
    MissingHwDevice(&'static str),
    /// A decoded surface the GPU interop does not take, with its description and the reason.
    UnsupportedSurface(String),
}

/// How a planar or semi-planar YCbCr pixel format stores its samples.
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
    /// `stride * height` fully written bytes, either a view of the frame's buffer or a packed copy.
    pub bytes: bytes::Bytes,
    /// Bytes between rows, at least the row's samples.
    pub stride: u32,
    /// Sample positions per row (a U, V pair each for interleaved chroma).
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
fn configure_colorspace(
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

/// Row alignment of every `sws_scale` destination: 16 RGBA pixels, the widest block a SIMD
/// converter stores per iteration.
const SCALER_DESTINATION_ALIGN: i32 = 64;

/// Converts frames of one size and pixel format into another pixel format at the same size.
///
/// The destination of `sws_scale` is always a frame this type allocates with
/// `av_frame_get_buffer(.., 64)` for its own format and size, zero filled before use. Some
/// converters write whole blocks past a row's last pixel or skip the last columns, so a caller's
/// slice is never a destination: rows padded to 64 bytes keep every block inside the frame, and
/// zero fill leaves defined bytes where a converter writes nothing.
pub struct PackedScaler {
    context: SwsContext,
    width: i32,
    height: i32,
    source: ffi::AVPixelFormat,
    target: ffi::AVPixelFormat,
    destination: AVFrame,
}

impl PackedScaler {
    /// A scaler from `source` to `target` at `width`x`height`, with bilinear filtering.
    ///
    /// # Errors
    /// Returns an error when `libswscale` has no conversion between the formats or a frame of that
    /// size cannot be allocated.
    pub fn new(
        width: i32,
        height: i32,
        source: ffi::AVPixelFormat,
        target: ffi::AVPixelFormat,
    ) -> Result<Self, BridgeError> {
        if width <= 0 || height <= 0 {
            return Err(BridgeError::InvalidInput("scaler size is not positive"));
        }
        let context = SwsContext::get_context(
            width,
            height,
            source,
            width,
            height,
            target,
            scaler_flags(width, height, source, target),
            None,
            None,
            None,
        )
        .ok_or(BridgeError::InvalidInput(
            "libswscale has no conversion between these pixel formats",
        ))?;
        let destination = padded_zeroed_frame(width, height, target)?;
        Ok(Self {
            context,
            width,
            height,
            source,
            target,
            destination,
        })
    }

    /// Applies explicit matrix and range coefficients (`SWS_CS_*` constants) to the conversion.
    /// RGB endpoints still need an explicit full-range flag.
    ///
    /// # Errors
    /// Returns an error when `libswscale` rejects the configuration.
    pub fn configure_colorspace(
        &mut self,
        source_coefficients: i32,
        source_full_range: bool,
        destination_coefficients: i32,
        destination_full_range: bool,
    ) -> Result<(), BridgeError> {
        configure_colorspace(
            &mut self.context,
            source_coefficients,
            source_full_range,
            destination_coefficients,
            destination_full_range,
        )
    }

    /// Converts `frame` and returns the target's planes packed without row padding, in plane
    /// order (the `av_image_copy_to_buffer` layout at alignment 1).
    ///
    /// # Errors
    /// Returns `InvalidInput` for a frame of another size or format than the scaler's, and an
    /// error when `libswscale` fails.
    pub fn convert(&mut self, frame: &AVFrame) -> Result<Vec<u8>, BridgeError> {
        self.check_source(frame)?;
        scale_into(&mut self.context, frame, &mut self.destination, self.height)?;
        // SAFETY: a pure size computation over plain values.
        let size =
            unsafe { ffi::av_image_get_buffer_size(self.target, self.width, self.height, 1) };
        let len = usize::try_from(size).map_err(|_| BridgeError::FfmpegCode(size))?;
        let mut packed = vec![0_u8; len];
        // SAFETY: `packed` holds exactly `size` bytes, and the destination frame's planes hold
        // `height` rows of the target format at their linesizes.
        let copied = unsafe {
            ffi::av_image_copy_to_buffer(
                packed.as_mut_ptr(),
                size,
                self.destination.data.as_ptr().cast(),
                self.destination.linesize.as_ptr(),
                self.target,
                self.width,
                self.height,
                1,
            )
        };
        if copied < 0 {
            return Err(BridgeError::FfmpegCode(copied));
        }
        Ok(packed)
    }

    /// Converts `frame` into a new frame of the target format with padded rows, for an encoder
    /// that keeps references to its input.
    ///
    /// # Errors
    /// As [`Self::convert`], and when the new frame cannot be allocated.
    pub fn convert_to_frame(&mut self, frame: &AVFrame) -> Result<AVFrame, BridgeError> {
        self.check_source(frame)?;
        let mut output = padded_zeroed_frame(self.width, self.height, self.target)?;
        scale_into(&mut self.context, frame, &mut output, self.height)?;
        Ok(output)
    }

    fn check_source(&self, frame: &AVFrame) -> Result<(), BridgeError> {
        if frame.width != self.width || frame.height != self.height || frame.format != self.source {
            return Err(BridgeError::InvalidInput(
                "frame size or format differs from the scaler's",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for PackedScaler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackedScaler")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("source", &self.source)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// Bilinear, plus accurate rounding where `libswscale` would otherwise pick its unscaled
/// 4:2:0/4:2:2-to-RGB converter for an odd width: its C version never writes the last column.
fn scaler_flags(
    width: i32,
    height: i32,
    source: ffi::AVPixelFormat,
    target: ffi::AVPixelFormat,
) -> ffi::SwsFlags {
    let unscaled_rgb_source = matches!(
        source,
        ffi::AV_PIX_FMT_YUV420P | ffi::AV_PIX_FMT_YUV422P | ffi::AV_PIX_FMT_YUVA420P
    );
    // SAFETY: `av_pix_fmt_desc_get` accepts any value and returns null or a static descriptor.
    let rgb_target = unsafe { ffi::av_pix_fmt_desc_get(target).as_ref() }
        .is_some_and(|descriptor| descriptor.flags & u64::from(ffi::AV_PIX_FMT_FLAG_RGB) != 0);
    if unscaled_rgb_source && rgb_target && height % 2 == 0 && width % 2 == 1 {
        ffi::SWS_BILINEAR | ffi::SWS_ACCURATE_RND
    } else {
        ffi::SWS_BILINEAR
    }
}

/// A frame of `format` with rows aligned to [`SCALER_DESTINATION_ALIGN`], every byte zero.
fn padded_zeroed_frame(
    width: i32,
    height: i32,
    format: ffi::AVPixelFormat,
) -> Result<AVFrame, BridgeError> {
    let mut frame = AVFrame::new();
    frame.set_format(format);
    frame.set_width(width);
    frame.set_height(height);
    frame.get_buffer(SCALER_DESTINATION_ALIGN)?;
    for buffer in frame.buf {
        // SAFETY: a non-null `AVFrame::buf` entry is live, and this new frame is its only owner.
        if let Some(buffer) = unsafe { buffer.as_ref() } {
            // SAFETY: as above.
            unsafe { ptr::write_bytes(buffer.data, 0, buffer.size) };
        }
    }
    Ok(frame)
}

/// The workspace's only `sws_scale` call. `destination` is a frame from [`padded_zeroed_frame`]
/// of the context's target format and size.
fn scale_into(
    context: &mut SwsContext,
    source: &AVFrame,
    destination: &mut AVFrame,
    height: i32,
) -> Result<(), BridgeError> {
    // SAFETY: the context is exclusively borrowed and was made for this size and both formats,
    // the source planes belong to the live `source`, and the destination planes belong to a frame
    // this crate allocated for the target format and size with padded rows.
    let rows = unsafe {
        ffi::sws_scale(
            context.as_mut_ptr(),
            source.data.as_ptr().cast(),
            source.linesize.as_ptr(),
            0,
            height,
            destination.data.as_ptr().cast(),
            destination.linesize.as_ptr(),
        )
    };
    if rows < 0 {
        return Err(BridgeError::FfmpegCode(rows));
    }
    if rows != height {
        return Err(BridgeError::InvalidInput(
            "libswscale wrote fewer rows than the frame has",
        ));
    }
    Ok(())
}

impl Error for BridgeError {}

impl BridgeError {
    /// The device fault a failed platform GPU call's status names, if any.
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
/// Classify by `fault`, which the backend reads from `status`, not by the message.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NativeGpuFailure {
    /// The API the call belongs to.
    pub backend: NativeGpuBackend,
    /// What the helper was doing, such as `encode copy`.
    pub operation: &'static str,
    /// The step of it that failed, such as `queue submit`.
    pub step: &'static str,
    /// A `VkResult`, `HRESULT` bits, or `CVReturn`; zero when the helper's own check failed.
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
        // Use the renderer's timeout sentence so a hung GPU reads the same everywhere.
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

/// Whether muxer `format` can store `codec` at normal compliance; `None` when it is not linked.
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
    /// Whether the components are RGB (palettes included) rather than luma and chroma or gray.
    pub is_rgb: bool,
}

/// Describes a pixel format, or `None` when the value has no descriptor.
///
/// A stream's format may be unset until the first frame decodes, so `None` means "not yet known".
#[must_use]
pub fn sample_profile(format: ffi::AVPixelFormat) -> Option<SampleProfile> {
    if format == ffi::AV_PIX_FMT_NONE {
        return None;
    }
    // SAFETY: `av_pix_fmt_desc_get` accepts any value and returns null or a static descriptor.
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
    // SAFETY: `av_get_pix_fmt_name` accepts any value and returns null or a static C string.
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
/// `avcodec_open2` frees the dictionary and writes the remainder back on failure as well as
/// success. rsmpeg's `AVCodecContext::open` double-frees it on failure, which would abort an
/// encoder fallback.
///
/// # Errors
/// Returns the `FFmpeg` status when the encoder or decoder cannot be opened.
pub fn open_codec_context(
    context: &mut rsmpeg::avcodec::AVCodecContext,
    options: rsmpeg::avutil::AVDictionary,
) -> Result<Option<rsmpeg::avutil::AVDictionary>, RsmpegError> {
    let _media_foundation = media_foundation_lock(context);
    let mut dictionary = options.into_raw().as_ptr();
    // SAFETY: `context` is live and FFmpeg owns `dictionary`, replacing it with a null or owned
    // remainder.
    let status = unsafe {
        ffi::avcodec_open2(
            context.as_mut_ptr(),
            ptr::null_mut(),
            std::ptr::addr_of_mut!(dictionary),
        )
    };
    // SAFETY: a non-null remainder is a dictionary FFmpeg handed back; this is its only owner.
    let unused = ptr::NonNull::new(dictionary)
        .map(|dictionary| unsafe { rsmpeg::avutil::AVDictionary::from_raw(dictionary) });
    if status < 0 {
        return Err(RsmpegError::from(status));
    }
    Ok(unused)
}

/// Serializes opening and closing `MediaFoundation` (`_mf`) encoders; `None` for other codecs.
///
/// `MFTEnumEx` (on open) and `MFShutdown` (on close) take mfplat's critical section and the
/// work-queue lock in opposite orders and deadlock across threads, whatever the `MFStartup` count.
fn media_foundation_lock(
    context: &rsmpeg::avcodec::AVCodecContext,
) -> Option<std::sync::MutexGuard<'static, ()>> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if !cfg!(target_os = "windows") || context.codec.is_null() {
        return None;
    }
    // SAFETY: a non-null `codec` is a static codec descriptor with a static C-string `name`.
    let name = unsafe { CStr::from_ptr((*context.codec).name) };
    name.to_bytes().ends_with(b"_mf").then(|| {
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    })
}

/// An [`AVCodecContext`] closed under the `MediaFoundation` lock when it is an `_mf` encoder.
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
/// `TRANSFER_DST` for the renderer's copy, `SAMPLED` for wgpu import, `TRANSFER_SRC` for readback
/// checks, `VIDEO_ENCODE_SRC` for the encoder.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub const VULKAN_ENCODE_INPUT_USAGE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_4000;

/// Adds `usage` to a Vulkan frames context before it allocates its pool.
///
/// Left at zero, `hwcontext_vulkan` picks a default that requires `STORAGE` support. A nonzero
/// usage is used exactly as given, so every flag must be supported by the format; encoder-input
/// pools ask for [`VULKAN_ENCODE_INPUT_USAGE`]. Call between `av_hwframe_ctx_alloc` and
/// `av_hwframe_ctx_init`.
///
/// # Errors
/// Returns an error when the context is not a Vulkan one.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub fn vulkan_frames_add_usage(
    frames: &mut rsmpeg::avutil::AVHWFramesContext,
    usage: u32,
) -> Result<(), BridgeError> {
    // SAFETY: `frames` is live; the shim checks the device type and only ORs into `usage`.
    let status = unsafe { alpha2_vulkan_frames_add_usage(frames.as_mut_ptr(), usage) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    Ok(())
}

/// Usage the pool was created with, including the default `FFmpeg` picks for zero.
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
#[must_use]
pub fn vulkan_frames_usage(frames: &rsmpeg::avutil::AVHWFramesContext) -> u32 {
    // SAFETY: `frames` is live; the shim checks the device type before reading `hwctx`.
    unsafe { alpha2_vulkan_frames_usage(frames.as_ptr()) }
}

/// Copies one packet's payload out of `FFmpeg`'s buffer.
#[must_use]
pub fn packet_bytes(packet: &rsmpeg::avcodec::AVPacket) -> Vec<u8> {
    let size = usize::try_from(packet.size).unwrap_or(0);
    if packet.data.is_null() || size == 0 {
        return Vec::new();
    }
    // SAFETY: the borrowed packet owns `size` readable bytes at `data`.
    unsafe { slice::from_raw_parts(packet.data.cast_const(), size) }.to_vec()
}

/// A copy of a stream's codec extradata (parameter sets for H.264 and HEVC).
///
/// Takes rsmpeg's wrapper because a bare `AVCodecParameters` can carry a forged pointer:
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
    // SAFETY: the borrowed parameters own `extradata_size` readable bytes at `extradata`.
    unsafe { slice::from_raw_parts(parameters.extradata.cast_const(), size) }.to_vec()
}

/// Whether `packet` carries side data of `kind`.
#[must_use]
pub fn packet_has_side_data(
    packet: &rsmpeg::avcodec::AVPacket,
    kind: ffi::AVPacketSideDataType,
) -> bool {
    let mut size = 0_usize;
    // SAFETY: `packet` is live; the result is only compared with null.
    !unsafe { ffi::av_packet_get_side_data(packet.as_ptr(), kind, &raw mut size) }.is_null()
}

/// Removes and returns `packet`'s side data of `kind`.
///
/// Encoder parameter sets must be taken off, not copied, or the MP4 muxer reads them as a sample
/// description change.
#[must_use]
pub fn take_packet_side_data(
    packet: &mut rsmpeg::avcodec::AVPacket,
    kind: ffi::AVPacketSideDataType,
) -> Option<Vec<u8>> {
    let mut size = 0_usize;
    // SAFETY: `packet` is live; a non-null result points at `size` bytes the packet owns.
    let data = unsafe { ffi::av_packet_get_side_data(packet.as_ptr(), kind, &raw mut size) };
    if data.is_null() {
        return None;
    }
    // SAFETY: as above; the bytes are copied before the entry is freed below.
    let bytes = unsafe { slice::from_raw_parts(data, size) }.to_vec();
    let packet = packet.as_mut_ptr();
    // SAFETY: `side_data` and `side_data_elems` are the packet's own array and count, which
    // `av_packet_side_data_remove` keeps consistent while freeing the `av_malloc`ed entry.
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
/// When the side data cannot be allocated or `data` is too large.
pub fn set_packet_side_data(
    packet: &mut rsmpeg::avcodec::AVPacket,
    kind: ffi::AVPacketSideDataType,
    data: &[u8],
) -> Result<(), BridgeError> {
    let _ = take_packet_side_data(packet, kind);
    // SAFETY: `packet` is mutably borrowed; `av_packet_new_side_data` returns null or a padded
    // entry of `data.len()` bytes.
    let entry = unsafe { ffi::av_packet_new_side_data(packet.as_mut_ptr(), kind, data.len()) };
    if entry.is_null() {
        return Err(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)));
    }
    // SAFETY: the entry just made holds `data.len()` writable bytes.
    unsafe { ptr::copy_nonoverlapping(data.as_ptr(), entry, data.len()) };
    Ok(())
}

/// Replaces the extradata of `output`'s stream `index` before the header is written.
///
/// rsmpeg exposes an output's streams read-only. `FFmpeg` needs padded `av_malloc` memory here, not
/// a Rust buffer.
///
/// # Errors
/// When there is no stream `index`, or `data` is too large or cannot be allocated.
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
    // SAFETY: the stream owns its codec parameters and the output is mutably borrowed. The old
    // extradata is null or `av_malloc`ed and is freed; the parameters now own `buffer`.
    unsafe {
        ffi::av_freep((&raw mut (*parameters).extradata).cast());
        (*parameters).extradata = buffer;
        (*parameters).extradata_size = size;
    }
    Ok(())
}

/// A packet holding a copy of `data`, every other field at its default.
///
/// # Errors
/// When `data` is too large or the packet cannot be allocated.
pub fn packet_from_bytes(data: &[u8]) -> Result<rsmpeg::avcodec::AVPacket, BridgeError> {
    let size = i32::try_from(data.len())
        .map_err(|_| BridgeError::InvalidInput("a packet is larger than FFmpeg holds"))?;
    let mut packet = rsmpeg::avcodec::AVPacket::new();
    // SAFETY: `packet` is fresh and empty; `av_new_packet` gives it a padded buffer or fails.
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

/// Replaces `packet`'s payload with `data`, no longer than it, keeping timestamps and side data.
///
/// # Errors
/// When `data` is longer than the payload or the buffer cannot be made writable.
pub fn shrink_packet_to(
    packet: &mut rsmpeg::avcodec::AVPacket,
    data: &[u8],
) -> Result<(), BridgeError> {
    let size = i32::try_from(data.len())
        .ok()
        .filter(|size| *size <= packet.size)
        .ok_or(BridgeError::InvalidInput("a packet cannot grow in place"))?;
    // SAFETY: `packet` is live; `av_packet_make_writable` gives it an unshared buffer of its size.
    let status = unsafe { ffi::av_packet_make_writable(packet.as_mut_ptr()) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    if !data.is_empty() {
        // SAFETY: the writable buffer holds at least `data.len()` bytes and cannot overlap `data`.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), packet.data, data.len()) };
    }
    // SAFETY: `size` is not above the packet's size; FFmpeg zeroes the padding after it.
    unsafe { ffi::av_shrink_packet(packet.as_mut_ptr(), size) };
    Ok(())
}

/// Codec parameters for a video stream not produced by an `FFmpeg` encoder.
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
    // SAFETY: `raw` is freshly allocated and uniquely owned; the fields are plain values.
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
        // SAFETY: `buffer` holds the extradata's length and does not overlap it. The parameters
        // had no extradata and now own `buffer`.
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

/// Opens a bitstream filter chain in `ffmpeg -bsf` list syntax for a stream described by
/// `parameters` with packets in `time_base`.
///
/// rsmpeg cannot parse a list or set a filter's private options.
///
/// # Errors
/// Returns the `FFmpeg` status when the list does not parse, names a filter or option the runtime
/// lacks, or a filter refuses the stream.
pub fn bitstream_filter_list(
    spec: &CStr,
    parameters: &AVCodecParameters,
    time_base: rsmpeg::avutil::AVRational,
) -> Result<rsmpeg::avcodec::AVBSFContext, BridgeError> {
    let mut raw = ptr::null_mut();
    // SAFETY: `spec` is NUL-terminated; on success `raw` holds a new context taken over below.
    let status = unsafe { ffi::av_bsf_list_parse_str(spec.as_ptr(), &raw mut raw) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    let raw = ptr::NonNull::new(raw).ok_or(BridgeError::FfmpegCode(ffi::AVERROR(ffi::ENOMEM)))?;
    // SAFETY: `raw` was just allocated and is unshared; the wrapper frees it with `av_bsf_free`.
    let mut context = unsafe { rsmpeg::avcodec::AVBSFContextUninit::from_raw(raw) };
    // rsmpeg's `set_par_in` panics when the copy cannot allocate.
    // SAFETY: `par_in` is the context's own parameter set and `parameters` is live;
    // `avcodec_parameters_copy` deep-copies into it.
    let status = unsafe { ffi::avcodec_parameters_copy(context.par_in, parameters.as_ptr()) };
    if status < 0 {
        return Err(BridgeError::FfmpegCode(status));
    }
    context.set_time_base_in(time_base);
    context.init().map_err(BridgeError::Ffmpeg)
}

/// Sets a stream's codec tag (sample entry); zero lets the muxer pick its default.
pub fn set_stream_codec_tag(stream: &mut rsmpeg::avformat::AVStreamMut<'_>, tag: u32) {
    let mut parameters = stream.codecpar_mut();
    // SAFETY: the mutably borrowed stream owns its codec parameters; `codec_tag` is a plain field.
    unsafe { (*parameters.as_mut_ptr()).codec_tag = tag };
}

/// Replaces a stream's display matrix (MP4's `tkhd` matrix) with `display`'s rotation and flip, as
/// [`display_rotation`] reads it back.
///
/// # Errors
/// When the side data cannot be allocated.
pub fn set_stream_display_matrix(
    stream: &mut rsmpeg::avformat::AVStreamMut<'_>,
    display: DisplayRotation,
) -> Result<(), BridgeError> {
    let mut parameters = stream.codecpar_mut();
    let parameters = parameters.as_mut_ptr();
    // SAFETY: the mutably borrowed stream owns its codec parameters; `av_packet_side_data_new`
    // keeps the array and count consistent and returns null only on allocation failure.
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
    // `av_display_rotation_set` turns clockwise, opposite to `av_display_rotation_get`.
    // SAFETY: `matrix` is a complete, aligned 3x3 display matrix.
    unsafe {
        ffi::av_display_rotation_set(matrix.as_mut_ptr(), -display.counter_clockwise_degrees);
        if display.flipped {
            ffi::av_display_matrix_flip(matrix.as_mut_ptr(), 1, 0);
        }
    }
    // SAFETY: the new entry holds `size_of::<[i32; 9]>()` writable bytes; a byte copy needs no
    // alignment.
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
    // SAFETY: `av_hwdevice_get_type_name` accepts any value and returns null or a static C string.
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

/// The plane layout of a YCbCr format the renderer samples directly, or `None`.
///
/// Accepts three components of one depth from 8 to 16 bits, chroma subsampled by at most four per
/// axis, three planes or U, V interleaved in the second, native byte order, low- or (above eight
/// bits) high-bit aligned. Rejects RGB, alpha, gray, palette, Bayer, float, XYZ, bitstream,
/// hardware formats and NV21.
#[must_use]
pub fn yuv_layout(format: ffi::AVPixelFormat) -> Option<YuvPlaneLayout> {
    if format == ffi::AV_PIX_FMT_NONE {
        return None;
    }
    // SAFETY: `av_pix_fmt_desc_get` accepts any value and returns null or a static descriptor.
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

/// Copies the samples of a frame whose format [`yuv_layout`] accepts into packed forward rows, or
/// returns `Ok(None)` for other formats. Odd dimensions round chroma up.
///
/// Row padding is never read since it may be uninitialized; [`DecodedFrame::yuv_planes`] can share
/// fully written frames instead.
///
/// # Errors
/// Returns an error for invalid dimensions, null planes, or linesizes shorter than the plane width.
pub fn yuv_planes(frame: &AVFrame) -> Result<Option<YuvPlanes>, BridgeError> {
    frame_yuv_planes(frame, false)
}

/// A decoded frame, and whether every byte of its buffers, row padding included, is written.
///
/// Only this crate constructs one; frames from elsewhere go to [`yuv_planes`].
pub struct DecodedFrame {
    frame: AVFrame,
    written: bool,
}

impl DecodedFrame {
    /// Takes the next frame `decoder` has ready.
    ///
    /// # Errors
    /// Returns what `avcodec_receive_frame` does for a drained decoder or a decode failure.
    pub fn receive(decoder: &mut AVCodecContext) -> Result<Self, RsmpegError> {
        let written = decoder_frames_are_initialized(decoder);
        let frame = decoder.receive_frame()?;
        Ok(Self { frame, written })
    }

    /// Like [`yuv_planes`], but shares the frame's buffer when it is fully written. Planes with a
    /// negative linesize or outside the frame's buffers are still copied.
    ///
    /// # Errors
    /// Returns an error for invalid dimensions, null planes, or linesizes shorter than the plane
    /// width.
    pub fn yuv_planes(&self) -> Result<Option<YuvPlanes>, BridgeError> {
        frame_yuv_planes(&self.frame, self.written)
    }

    /// Sets the frame's presentation timestamp.
    pub fn set_pts(&mut self, pts: i64) {
        self.frame.set_pts(pts);
    }

    /// Takes ownership of the frame.
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

/// Whether `decoder`'s frame buffers are fully initialized, so they may be shared.
///
/// The default `get_buffer2` uses zeroed pools. Wrapped external decoders (`wrapper_name`, such as
/// libdav1d) and replaced `get_buffer2` callbacks do not qualify.
fn decoder_frames_are_initialized(decoder: &AVCodecContext) -> bool {
    static DEFAULT_GET_BUFFER2: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // Read the default from a new context; Rust's address of the function could be a linker stub.
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
    // SAFETY: a context's `codec` is null or a static, read-only description.
    let wrapped =
        unsafe { decoder.codec.as_ref() }.is_none_or(|codec| !codec.wrapper_name.is_null());
    let get_buffer2 = decoder.get_buffer2.map_or(0, |function| function as usize);
    !wrapped && default != 0 && get_buffer2 == default
}

/// The planes of `frame`; `share` only for fully initialized frames.
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
    // Only a vouched-for, reference-counted frame is shared; `AVFrame::clone` adds buffer refs.
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

// SAFETY: the wrapped frame is only read. FFmpeg never writes to a buffer with more than one
// reference, so nothing mutates it while shared.
unsafe impl Sync for SharedFrame {}

/// `stride * rows` bytes of one plane of a fully written [`SharedFrame`], which it keeps alive.
struct PlaneView {
    frame: std::sync::Arc<SharedFrame>,
    data: *const u8,
    len: usize,
    stride: usize,
}

// SAFETY: the view reads immutable memory in `frame`'s buffers, which it keeps alive;
// `SharedFrame` is `Send + Sync`.
unsafe impl Send for PlaneView {}

impl PlaneView {
    /// `None` unless the linesize is positive, covers `row_bytes`, and all rows lie in one buffer.
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
            // SAFETY: a non-null `AVFrame::buf` entry is a live `AVBufferRef` the frame holds.
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
        let _ = &self.frame;
        // SAFETY: `new` checked `data..data + len` lies in a buffer of the frame `self` holds.
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
    // Append row by row to avoid an extra zeroing pass over the buffer.
    let mut output = Vec::with_capacity(byte_len);
    let signed_stride =
        isize::try_from(linesize).map_err(|_| BridgeError::InvalidInput(context))?;
    for row in 0..rows {
        let row = isize::try_from(row).map_err(|_| BridgeError::InvalidInput(context))?;
        let offset = row
            .checked_mul(signed_stride)
            .ok_or(BridgeError::InvalidInput(context))?;
        // SAFETY: a non-null plane has `height` rows at its signed linesize, whose absolute value
        // was checked to cover `row_bytes`.
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

    // SAFETY: av_frame_get_buffer allocated `height` writable rows of `linesize` bytes; both slices
    // span one validated RGBA row.
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
/// Returns an error for invalid dimensions, short source planes, or an allocation failure.
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

/// The first pixel format in `FFmpeg`'s table laid out as `layout` (`yuv420p`, not the deprecated
/// `yuvj420p`).
#[must_use]
pub fn yuv_pixel_format(layout: YuvPlaneLayout) -> Option<ffi::AVPixelFormat> {
    let mut descriptor = ptr::null();
    loop {
        // SAFETY: null starts the walk; other values are static descriptors from the previous call.
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

/// A `width` by `height` frame of `format` holding a copy of `planes` (bytes and stride each).
///
/// # Errors
/// For a format without a descriptor, the wrong plane count, or a plane smaller than the picture.
pub fn planar_frame(
    format: ffi::AVPixelFormat,
    planes: &[(&[u8], u32)],
    width: u32,
    height: u32,
) -> Result<AVFrame, BridgeError> {
    // SAFETY: `av_pix_fmt_desc_get` accepts any value and returns null or a static descriptor.
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
        // SAFETY: the destination has `height` writable rows covering `width` bytes, and the source
        // length was checked against its stride and row count.
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

/// Copies planar `f32` samples into an audio frame of `format` (`FLTP`, `FLT` or `S16`). 16-bit
/// samples are clamped to [-1, 1] and rounded.
///
/// # Errors
/// Returns an error for mismatched channels, invalid dimensions, another sample format, or an
/// allocation failure.
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
            // SAFETY: av_frame_get_buffer allocated one writable plane per channel of `linesize[0]`
            // bytes, checked above to hold the channel.
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
    // SAFETY: the single interleaved plane was checked to hold every sample and is unshared.
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

/// The `AVChannel` of each channel of `layout`, in order; empty for `AV_CHANNEL_ORDER_UNSPEC`.
///
/// Takes rsmpeg's wrapper because a bare `AVChannelLayout` can carry a forged channel map:
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

/// A channel layout of `channels` in order, native when possible, else custom.
///
/// # Errors
/// Returns an error for no channels or when `FFmpeg` cannot allocate or retype the layout.
pub fn channel_layout_of(
    channels: &[ffi::AVChannel],
) -> Result<rsmpeg::avutil::AVChannelLayout, BridgeError> {
    let count = i32::try_from(channels.len())
        .ok()
        .filter(|count| *count > 0)
        .ok_or(BridgeError::InvalidInput("a channel layout needs channels"))?;
    let mut raw = std::mem::MaybeUninit::<ffi::AVChannelLayout>::zeroed();
    // SAFETY: `custom_init` gives the zeroed layout a map of `count` entries, all written below.
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
    // SAFETY: the FLT plane was checked to hold every sample, and `output` is exactly that size.
    unsafe {
        ptr::copy_nonoverlapping(frame.data[0], output.as_mut_ptr().cast::<u8>(), bytes);
    }
    Ok(output)
}

/// The rotation a display matrix asks for, in `FFmpeg`'s convention.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DisplayRotation {
    /// Counter-clockwise angle in degrees, in `[-180, 180]`.
    pub counter_clockwise_degrees: f64,
    /// Whether the matrix also mirrors: this rotation followed by a horizontal flip.
    pub flipped: bool,
}

/// Reads a stream display matrix.
#[must_use]
pub fn display_rotation(parameters: &AVCodecParameters) -> Option<DisplayRotation> {
    let count = usize::try_from(parameters.nb_coded_side_data).ok()?;
    if count == 0 || count > 64 || parameters.coded_side_data.is_null() {
        return None;
    }
    // SAFETY: the borrowed parameters own exactly `nb_coded_side_data` entries.
    let entries = unsafe { slice::from_raw_parts(parameters.coded_side_data, count) };
    let matrix = entries.iter().find(|entry| {
        entry.type_ == ffi::AV_PKT_DATA_DISPLAYMATRIX
            && entry.size >= 9 * size_of::<i32>()
            && !entry.data.is_null()
    })?;
    // SAFETY: the matching side-data entry was validated to hold a complete 3x3 display matrix.
    unsafe { matrix_rotation(matrix.data) }
}

/// Reads a frame's display matrix, such as the JPEG decoder attaches for Exif `Orientation`.
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
    // SAFETY: the caller guarantees a complete 3x3 matrix; a byte copy needs no alignment.
    unsafe {
        ptr::copy_nonoverlapping(
            data,
            aligned.as_mut_ptr().cast::<u8>(),
            size_of::<[i32; 9]>(),
        );
    }
    // `av_display_rotation_get` is meaningless for a mirroring matrix (negative 2x2 determinant),
    // so remove the horizontal flip first.
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
/// Avoids [`yuv420p_frame`]'s per-frame allocation and copy. [`Yuv420pFrameBuffer::begin_write`]
/// returns plane memory only once `FFmpeg` no longer shares the buffer, copying it if needed.
pub struct Yuv420pFrameBuffer {
    frame: AVFrame,
    width: usize,
    height: usize,
    /// Each plane's visible bytes per row and rows; empty for YUV420P.
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

    /// Allocates one reusable frame of `format`; `planes` gives each plane's visible bytes per row
    /// and rows in `FFmpeg` order.
    ///
    /// # Errors
    /// Returns an error for a non-positive size, a failed allocation, or planes the format lacks.
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

    /// Each plane's memory and stride after making the frame unshared, for any format.
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
            // SAFETY: `zeroed_frame` allocated `rows` rows of `linesize` bytes per plane, and
            // `make_writable` left this frame the sole owner.
            let plane = unsafe { writable_plane(self.frame.data[index], stride, row_bytes, rows)? };
            written.push((plane, stride));
        }
        Ok(written)
    }

    fn make_writable(&mut self) -> Result<(), BridgeError> {
        // Not `av_frame_make_writable`: it leaves SIMD row padding unwritten under slices that span
        // it.
        if !self.frame.is_writable().map_err(BridgeError::Ffmpeg)? {
            let mut copy = zeroed_frame(self.frame.width, self.frame.height, self.frame.format)?;
            // SAFETY: both frames are live with one size and format; `copy` is unshared and
            // `self.frame` is only read.
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
    /// Returns an error when the buffer cannot be made writable or its planes changed layout.
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
        // SAFETY: `zeroed_frame` allocated and wrote `height` rows of `linesize` bytes per plane,
        // and the writability check makes this frame the sole owner.
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
        // SAFETY: a non-null `AVFrame::buf` entry is live, and this new frame is its only owner.
        if let Some(buffer) = unsafe { buffer.as_ref() } {
            // SAFETY: as above.
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
/// `data` must point at `height` rows of `stride` bytes the caller solely owns.
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
