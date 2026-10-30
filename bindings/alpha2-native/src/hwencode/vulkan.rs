//! The Vulkan backend: `h264_vulkan` on the renderer's shared device.
//!
//! Everything between the renderer and the encoder (the frames pool, its
//! image usage, the number of images per frame, the queue family that owns
//! them) is decided by `FFmpeg` and has to be observed rather than assumed.
//! The probes observe it on whichever device they are handed, so the same
//! answers can be read for `FFmpeg`'s own device and for the one wgpu created.

use std::collections::HashSet;
use std::ffi::CString;

use alpha2_media::{ColorMetadata, EncodeError};
use alpha2_time::MediaTime;
use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avutil::{AVFrame, AVHWDeviceContext, AVHWFramesContext, AVRational, av_rescale_q, ra};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use super::{
    EncoderInputHandoff, EncoderInputLayout, EncoderPoolStats, EncoderSurfaceDescriptor,
    HardwareEncodeBackend, HardwareEncodeCapability, HardwareEncodeOpenError,
    HardwareEncodeRequest, InputFrame,
};
use crate::NativeVideoCodec;
use crate::convert::{encode_adapter_error, tick_base};
use crate::encode::{EncodeStageTimings, timed};
use crate::mux::Muxer;

/// What one Vulkan encode session turned out to require and produce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanEncodeProbe {
    /// `FFmpeg` encoder that opened.
    pub encoder: String,
    /// Coded size the pool allocated.
    pub coded_size: (u32, u32),
    /// Image usage the pool was created with: the one the encoder-input pool
    /// asks for, which `FFmpeg` keeps as given. A flag the format cannot
    /// support fails the pool or the session instead of disappearing here.
    pub frames_usage: u32,
    /// `VkImage` handles backing one frame. One means a single multi-planar
    /// image and a per-plane copy; two would mean a plane per image.
    pub images_per_frame: usize,
    /// Initial layout `FFmpeg` left the frame in.
    pub initial_layout: u32,
    /// Queue family owning the frame, or `u32::MAX` for `VK_QUEUE_FAMILY_IGNORED`.
    pub queue_family: u32,
    /// Whether the frame exposes a timeline semaphore for the handoff.
    pub has_timeline_semaphore: bool,
    /// Packets drained from one submitted frame plus a flush.
    pub packets: u32,
    /// Bytes in those packets.
    pub packet_bytes: u32,
}

/// Opens `h264_vulkan` against `device` and encodes one pool frame.
///
/// The frame is left as the pool allocated it: this answers whether the
/// session can be created and driven on this device, not what the picture
/// looks like.
///
/// # Errors
/// Returns an adapter failure when the encoder is absent, the pool cannot be
/// created, the session cannot open, or no packet is produced.
pub fn probe_vulkan_encode(
    device: &AVHWDeviceContext,
    width: u32,
    height: u32,
) -> Result<VulkanEncodeProbe, EncodeError> {
    let encoder = AVCodec::find_encoder_by_name(c"h264_vulkan").ok_or_else(|| {
        EncodeError::Adapter("this FFmpeg runtime has no h264_vulkan encoder".into())
    })?;
    let coded_width =
        i32::try_from(width).map_err(|_| EncodeError::Adapter("width is out of range".into()))?;
    let coded_height =
        i32::try_from(height).map_err(|_| EncodeError::Adapter("height is out of range".into()))?;

    let mut frames = vulkan_encode_frames(device, coded_width, coded_height)?;
    let frames_usage = alpha2_ffmpeg_bridge::vulkan_frames_usage(&frames);

    let mut context = AVCodecContext::new(&encoder);
    context.set_width(coded_width);
    context.set_height(coded_height);
    context.set_pix_fmt(ffi::AV_PIX_FMT_VULKAN);
    // `ff_encode_preinit` copies the pool's `sw_format` into `sw_pix_fmt`, and rejects the open if
    // a caller set a different one, so it is left unset.
    context.set_time_base(ra(1, 30));
    context.set_framerate(ra(30, 1));
    context.set_bit_rate(12_000_000);
    context.set_gop_size(12);
    // A probe's coding settings are arbitrary; see the session for why exports have no B pictures.
    context.set_max_b_frames(0);
    context.set_hw_frames_ctx(frames.clone());
    context
        .open(None)
        .map_err(|error| encode_adapter_error_at_open(&error))?;

    let mut frame = AVFrame::new();
    frames
        .get_buffer(&mut frame)
        .map_err(encode_adapter_error)?;
    let descriptor =
        alpha2_ffmpeg_bridge::vulkan_frame_info(&frame).map_err(encode_adapter_error)?;
    frame.set_pts(0);

    context
        .send_frame(Some(&frame))
        .map_err(encode_adapter_error)?;
    context.send_frame(None).map_err(encode_adapter_error)?;
    let mut packets = 0;
    let mut packet_bytes = 0_u32;
    loop {
        match context.receive_packet() {
            Ok(packet) => {
                packets += 1;
                packet_bytes = packet_bytes.saturating_add(packet.size.unsigned_abs());
            }
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(error) => return Err(encode_adapter_error(error)),
        }
    }
    if packets == 0 {
        return Err(EncodeError::Adapter(
            "h264_vulkan opened but produced no packet".into(),
        ));
    }
    Ok(VulkanEncodeProbe {
        encoder: encoder.name().to_string_lossy().into_owned(),
        coded_size: (
            descriptor.coded_width.unsigned_abs(),
            descriptor.coded_height.unsigned_abs(),
        ),
        frames_usage,
        images_per_frame: descriptor.plane_count as usize,
        initial_layout: descriptor.layouts[0],
        queue_family: descriptor.queue_families[0],
        has_timeline_semaphore: descriptor.semaphores[0] != 0,
        packets,
        packet_bytes,
    })
}

/// Opens a Vulkan encoder with one rate-control request and encodes one
/// pool frame, on a device `FFmpeg` creates for the purpose.
///
/// This is `probe-encoders`' view of a hardware-frames encoder: the pass
/// condition is the same as for a software one: it opened, consumed every
/// option it was given, and produced a packet. Whether the device supports a
/// mode is `VkVideoEncodeCapabilitiesKHR::rateControlModes`, which `FFmpeg`
/// checks at open, so an unsupported one fails here rather than later.
pub(crate) fn probe_vulkan_rate_control(
    encoder: &rsmpeg::avcodec::AVCodecRef<'static>,
    width: u32,
    height: u32,
    request: &crate::rate_control::MechanismRequest,
) -> Result<(u32, String), String> {
    // The host's chosen device, passed by index: FFmpeg reads a name starting with a digit as an
    // index, so `ALPHA2_VULKAN_DEVICE` cannot pass through unchanged. Otherwise the probe walks the
    // indices until one accepts or refuses the mode, since only some devices have an encode queue.
    let named = std::env::var("ALPHA2_VULKAN_DEVICE").ok();
    let shared = alpha2_native_gpu::shared_device_index(named.as_deref());
    let candidates = match (&named, shared) {
        (Some(_), Ok(index)) => vec![index.to_string()],
        (Some(_), Err(error)) => return Err(error.to_string()),
        (None, shared) => {
            let first = shared.ok();
            first
                .into_iter()
                .chain((0..8).filter(|index| Some(*index) != first))
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
        }
    };
    let mut last = "no Vulkan device".to_owned();
    for device in candidates {
        match probe_vulkan_rate_control_on(encoder, &device, width, height, request) {
            Ok(packets) => return Ok((packets, device)),
            Err(VulkanProbeError::NoDevice(error)) => {
                last = error;
                break;
            }
            Err(VulkanProbeError::NoEncodeQueue(error)) if named.is_none() => last = error,
            Err(VulkanProbeError::NoEncodeQueue(error) | VulkanProbeError::Refused(error)) => {
                return Err(format!("on Vulkan device {device}: {error}"));
            }
        }
    }
    Err(last)
}

enum VulkanProbeError {
    /// No device at this index: the walk is over.
    NoDevice(String),
    /// The device has no video-encode queue; another one might.
    NoEncodeQueue(String),
    /// The device got as far as the encoder, which refused.
    Refused(String),
}

fn probe_vulkan_rate_control_on(
    encoder: &rsmpeg::avcodec::AVCodecRef<'static>,
    device_name: &str,
    width: u32,
    height: u32,
    request: &crate::rate_control::MechanismRequest,
) -> Result<u32, VulkanProbeError> {
    let device_name = CString::new(device_name)
        .map_err(|_| VulkanProbeError::NoDevice("device name contains a NUL byte".to_owned()))?;
    // FFmpeg enables video-encode extensions on a device it creates only when asked; the export's
    // shared device asks through wgpu instead.
    let extensions = rsmpeg::avutil::AVDictionary::new(
        c"device_extensions",
        c"VK_KHR_video_queue+VK_KHR_video_encode_queue+VK_KHR_video_encode_h264+VK_KHR_video_encode_h265+VK_KHR_video_encode_av1+VK_KHR_video_maintenance1",
        0,
    );
    let device = AVHWDeviceContext::create(
        ffi::AV_HWDEVICE_TYPE_VULKAN,
        Some(&device_name),
        Some(&extensions),
        0,
    )
    .map_err(|error| VulkanProbeError::NoDevice(format!("no Vulkan device: {error}")))?;
    let refused = |error: String| VulkanProbeError::Refused(error);
    let coded_width =
        i32::try_from(width).map_err(|_| refused("width is out of range".to_owned()))?;
    let coded_height =
        i32::try_from(height).map_err(|_| refused("height is out of range".to_owned()))?;
    let mut frames = vulkan_encode_frames(&device, coded_width, coded_height)
        .map_err(|error| VulkanProbeError::NoEncodeQueue(error.to_string()))?;
    let mut context = AVCodecContext::new(encoder);
    context.set_width(coded_width);
    context.set_height(coded_height);
    context.set_pix_fmt(ffi::AV_PIX_FMT_VULKAN);
    context.set_time_base(ra(1, 30));
    context.set_framerate(ra(30, 1));
    context.set_bit_rate(request.bit_rate);
    context.set_gop_size(12);
    context.set_max_b_frames(0);
    context.set_hw_frames_ctx(frames.clone());
    let unused = alpha2_ffmpeg_bridge::open_codec_context(
        &mut context,
        request.dictionary(crate::convert::color_options(ColorMetadata::EXPORT)),
    )
    .map_err(|error| {
        // `ENOSYS` means the device has no encode queue at all, which says nothing about the mode.
        let message = error.to_string();
        if matches!(error, RsmpegError::AVError(code) if code == -38) {
            VulkanProbeError::NoEncodeQueue(message)
        } else {
            refused(message)
        }
    })?;
    crate::convert::reject_unused_encode_options(unused)
        .map_err(|error| refused(error.to_string()))?;
    let mut frame = AVFrame::new();
    frames
        .get_buffer(&mut frame)
        .map_err(|error| refused(error.to_string()))?;
    frame.set_pts(0);
    context
        .send_frame(Some(&frame))
        .map_err(|error| refused(error.to_string()))?;
    context
        .send_frame(None)
        .map_err(|error| refused(error.to_string()))?;
    let mut packets = 0;
    loop {
        match context.receive_packet() {
            Ok(_) => packets += 1,
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(error) => return Err(refused(error.to_string())),
        }
    }
    if packets == 0 {
        return Err(refused("opened but produced no packet".to_owned()));
    }
    Ok(packets)
}

/// What this device's `h264_vulkan` accepts, from the device's own answers.
pub(super) fn capability(
    shared: &alpha2_native_gpu::SharedVulkanContext,
) -> Result<HardwareEncodeCapability, String> {
    if shared.encode_codecs().is_empty() {
        return Err("this Vulkan device has no video-encode queue".to_owned());
    }
    let extent = shared
        .encode_extent()
        .ok_or_else(|| "this device did not report its H.264 encode extents".to_owned())?;
    Ok(HardwareEncodeCapability {
        backend: HardwareEncodeBackend::Vulkan,
        encoder: "h264_vulkan",
        codec: NativeVideoCodec::H264,
        gpu_input_only: true,
        input_layout: EncoderInputLayout::Nv12,
        handoff: EncoderInputHandoff::GpuCopy,
        shares_renderer_device: true,
        minimum: extent.minimum,
        maximum: extent.maximum,
        granularity: extent.granularity,
        max_p_picture_l0_references: Some(extent.max_p_picture_l0_references),
        max_b_picture_l0_references: Some(extent.max_b_picture_l0_references),
        max_l1_references: Some(extent.max_l1_references),
        rate_control_modes: extent.rate_control_modes.clone(),
        plane_ring_slots: alpha2_native_gpu::VulkanEncodeCopier::slots(),
        // The copier keeps each copy's frame in its slot until the slot is reused.
        handoff_retained_surfaces: alpha2_native_gpu::VulkanEncodeCopier::slots(),
        // Measured: the pool never exceeded the host's leases plus the copier's slots.
        codec_retained_surfaces: 0,
    })
}

/// Opens `h264_vulkan` on the shared device, its input half, and the output.
pub(super) fn open(
    shared: &alpha2_native_gpu::SharedVulkanContext,
    request: &HardwareEncodeRequest<'_>,
    generation: u64,
) -> Result<(VulkanEncodeSession, VulkanEncodeInput), HardwareEncodeOpenError> {
    let refused = |error: EncodeError| HardwareEncodeOpenError::Refused(error.to_string());
    let (width, height) = (request.width, request.height);
    let frame_rate = super::session_frame_rate(request.frame_rate).map_err(refused)?;
    let (frames, context, selection) = open_encoder(
        &shared.ffmpeg_device(),
        width,
        height,
        request.rate_control,
        frame_rate,
        request.color,
    )
    .map_err(refused)?;
    let copier = alpha2_native_gpu::VulkanEncodeCopier::new(shared)
        .map_err(|error| HardwareEncodeOpenError::Refused(format!("{error:?}")))?;
    let gpu = shared.gpu();
    // One pair per copier slot: the coordinator can run ahead of the encoder, so a single pair
    // would let the next render overwrite planes a copy is still reading.
    let planes = (0..alpha2_native_gpu::VulkanEncodeCopier::slots())
        .map(|_| Nv12PlanePair {
            luma: plane_texture(
                gpu.device(),
                "alpha2 NV12 luma",
                width,
                height,
                wgpu::TextureFormat::R8Unorm,
            ),
            chroma: plane_texture(
                gpu.device(),
                "alpha2 NV12 chroma",
                width / 2,
                height / 2,
                wgpu::TextureFormat::Rg8Unorm,
            ),
        })
        .collect();
    let time_base = ra(frame_rate.1, frame_rate.0);
    let input = VulkanEncodeInput {
        copier,
        planes,
        frames,
        size: (width, height),
        time_base,
        color: request.color,
        stats: EncoderPoolStats::default(),
        images: HashSet::new(),
    };
    // From here on a failure is the export's rather than a refusal: the partial output exists.
    let muxer = Muxer::open(
        request.output,
        request.container,
        &context,
        time_base,
        request.audio,
    )
    .map_err(HardwareEncodeOpenError::Failed)?;
    Ok((
        VulkanEncodeSession {
            context,
            time_base,
            muxer,
            stages: EncodeStageTimings::default(),
            selection,
            generation,
        },
        input,
    ))
}

/// One renderer-owned NV12 plane target for the encode handoff.
///
/// `COPY_SRC` is what gives the image `VK_IMAGE_USAGE_TRANSFER_SRC`, which the
/// raw copy into the encoder frame reads from.
fn plane_texture(
    device: &wgpu::Device,
    label: &'static str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// One slot of the NV12 plane ring the renderer packs into.
struct Nv12PlanePair {
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
}

/// Opens `h264_vulkan` on `device` with an NV12 encoder-input pool.
fn open_encoder(
    device: &AVHWDeviceContext,
    width: u32,
    height: u32,
    rate_control: alpha2_media::VideoRateControl,
    frame_rate: (i32, i32),
    color: ColorMetadata,
) -> Result<
    (
        AVHWFramesContext,
        AVCodecContext,
        crate::NativeVideoEncoderSelection,
    ),
    EncodeError,
> {
    let encoder = AVCodec::find_encoder_by_name(c"h264_vulkan").ok_or_else(|| {
        EncodeError::Adapter("this FFmpeg runtime has no h264_vulkan encoder".into())
    })?;
    let coded_width =
        i32::try_from(width).map_err(|_| EncodeError::Adapter("width is out of range".into()))?;
    let coded_height =
        i32::try_from(height).map_err(|_| EncodeError::Adapter("height is out of range".into()))?;
    let frames = vulkan_encode_frames(device, coded_width, coded_height)?;
    let rate = u32::try_from(frame_rate.0)
        .ok()
        .zip(u32::try_from(frame_rate.1).ok())
        .and_then(|(num, den)| alpha2_time::FrameRate::new(num, den).ok())
        .ok_or_else(|| EncodeError::Adapter("frame rate is out of range".into()))?;
    let open = |quantized: Option<&crate::rate_control::MechanismRequest>| {
        let mut context = AVCodecContext::new(&encoder);
        context.set_width(coded_width);
        context.set_height(coded_height);
        context.set_pix_fmt(ffi::AV_PIX_FMT_VULKAN);
        // `ff_encode_preinit` copies the pool's `sw_format` into `sw_pix_fmt`, and rejects the open
        // if a caller set a different one.
        context.set_time_base(ra(frame_rate.1, frame_rate.0));
        context.set_framerate(ra(frame_rate.0, frame_rate.1));
        context.set_bit_rate(quantized.map_or_else(
            || i64::try_from(rate_control.bitrate).unwrap_or(i64::MAX),
            |quantized| quantized.bit_rate,
        ));
        context.set_gop_size(crate::capabilities::keyframe_interval(rate));
        // No B pictures, by measurement: devices allow them (an RTX 4090 reports 7 L0 / 1 L1
        // references) and FFmpeg emits them, but on the 4K showcase at a fixed rate two B frames
        // cost 0.34-0.63 dB PSNR-Y against none across clips and GOP lengths.
        context.set_max_b_frames(0);
        context.set_hw_frames_ctx(frames.clone());
        // MP4 carries SPS and PPS in `avcC`. The flag is read at open, so it must be set before it,
        // or the parameter sets travel in-band only.
        context.set_flags(
            context.flags | i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0),
        );
        // Without these the stream has no VUI and decoders guess the colour; the software sinks tag
        // their streams through the same options.
        let color = crate::convert::color_options(color);
        let options = match quantized {
            Some(quantized) => quantized.dictionary(color),
            None => crate::rate_control::rate_control_options("h264_vulkan", rate_control, color),
        };
        let unused = alpha2_ffmpeg_bridge::open_codec_context(&mut context, options)
            .map_err(|error| encode_adapter_error_at_open(&error))?;
        crate::convert::reject_unused_encode_options(unused)?;
        if context.extradata_size <= 0 {
            return Err(EncodeError::Adapter(
                "h264_vulkan opened without SPS/PPS extradata for the MP4 header".into(),
            ));
        }
        Ok::<_, EncodeError>(context)
    };
    // A refused quantizer falls back to the rate control's own bitrate on the same encoder, and the
    // selection says which one runs.
    let (context, applied, quantizer_refused) =
        match crate::rate_control::quantizer_request("h264_vulkan", rate_control) {
            Some(Ok(quantized)) => match open(Some(&quantized)) {
                Ok(context) => (context, crate::AppliedRateControl::Quantizer, None),
                Err(error) => (
                    open(None)?,
                    crate::AppliedRateControl::Bitrate,
                    Some(format!("{} refused: {error}", quantized.describe())),
                ),
            },
            Some(Err(reason)) => (
                open(None)?,
                crate::AppliedRateControl::Bitrate,
                Some(reason),
            ),
            None => (open(None)?, crate::AppliedRateControl::Bitrate, None),
        };
    if let Some(reason) = &quantizer_refused {
        alpha2_os::diagnostic!(
            "alpha2 native export: h264_vulkan runs the bitrate fallback: {reason}"
        );
    }
    // No candidate list: this encoder is chosen from the device, and anything that could refuse it
    // already has.
    let selection = crate::NativeVideoEncoderSelection {
        encoder_name: "h264_vulkan".to_owned(),
        class: crate::NativeEncoderClass::Hardware,
        hardware_accelerated: true,
        rejected: Vec::new(),
        rate_control: applied,
        quantizer_refused,
        software_threads: None,
        pixels: alpha2_media::PixelLayout::YUV420P,
    };
    Ok((frames, context, selection))
}

/// The encode worker's half of a Vulkan session: the codec and the muxer.
pub(super) struct VulkanEncodeSession {
    context: AVCodecContext,
    time_base: AVRational,
    muxer: Muxer,
    stages: EncodeStageTimings,
    selection: crate::NativeVideoEncoderSelection,
    generation: u64,
}

/// The coordinator's half of a Vulkan session.
///
/// The codec moves to an encode worker; the copy cannot follow it, because
/// the wgpu plane textures and the render signal both belong to the thread
/// that rendered them. This is what stays behind: the copier and its plane
/// ring, and a reference to the encoder-input pool, which is reference
/// counted, so a frame taken here stays valid for as long as the encoder
/// needs it even though the two halves are on different threads.
///
/// Fields drop in declaration order, and the copier comes first: dropping it
/// waits on every in-flight copy, retaining the images, encoder frames, and
/// device together if that wait times out, since wgpu cannot track these raw
/// copies. Dropping the planes first would free them under a running copy.
pub(super) struct VulkanEncodeInput {
    copier: alpha2_native_gpu::VulkanEncodeCopier,
    planes: Vec<Nv12PlanePair>,
    frames: AVHWFramesContext,
    size: (u32, u32),
    time_base: AVRational,
    color: ColorMetadata,
    stats: EncoderPoolStats,
    /// The distinct images the pool has handed out, which is how many it
    /// allocated: it recycles an image only once its last reference drops.
    images: HashSet<u64>,
}

/// One frame taken from a Vulkan encoder-input pool.
pub(super) struct VulkanEncoderFrame(AVFrame);

impl VulkanEncoderFrame {
    pub(super) fn set_pts(&mut self, pts: i64) {
        self.0.set_pts(pts);
    }
}

impl VulkanEncodeInput {
    pub(super) fn descriptor(&self, generation: u64) -> EncoderSurfaceDescriptor {
        EncoderSurfaceDescriptor {
            backend: HardwareEncodeBackend::Vulkan,
            pool_generation: generation,
            layout: EncoderInputLayout::Nv12,
            visible: self.size,
            color: self.color,
        }
    }

    /// Takes one frame from the encoder-input pool.
    ///
    /// `FFmpeg`'s pool recycles the image behind a frame once the last
    /// reference to it is dropped: the host's, the encoder's, and the
    /// copier's, which keeps each frame it copied into until its slot comes
    /// round again.
    pub(super) fn acquire_frame(&mut self) -> Result<VulkanEncoderFrame, EncodeError> {
        let mut frame = AVFrame::new();
        self.frames
            .get_buffer(&mut frame)
            .map_err(encode_adapter_error)?;
        let image = alpha2_ffmpeg_bridge::vulkan_frame_info(&frame)
            .map_err(encode_adapter_error)?
            .images[0];
        self.images.insert(image);
        self.stats.acquired = self.stats.acquired.saturating_add(1);
        self.stats.allocated = u64::try_from(self.images.len()).unwrap_or(u64::MAX);
        Ok(VulkanEncoderFrame(frame))
    }

    /// Waits until the plane pair the next copy reads is free. The render that
    /// packs the planes is submitted before [`Self::submit`]'s copy, on the
    /// same queue, which is all that orders the two.
    pub(super) fn begin_frame(&mut self) -> Result<InputFrame, EncodeError> {
        let slot = self.copier.await_slot().map_err(encode_adapter_error)?;
        if slot >= self.planes.len() {
            return Err(EncodeError::Adapter(format!(
                "encode copier reported slot {slot}"
            )));
        }
        Ok(InputFrame { slot })
    }

    pub(super) fn planes(&self, frame: &InputFrame) -> (&wgpu::Texture, &wgpu::Texture) {
        let pair = &self.planes[frame.slot];
        (&pair.luma, &pair.chroma)
    }

    /// Copies `frame`'s planes into `backing`. The copy signals the frame's
    /// own timeline semaphore and records the value on the frame, so the
    /// encoder waits for it on the GPU and this call does not have to.
    // Taking the token is the contract: a begun frame is submitted once.
    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn submit(
        &mut self,
        frame: InputFrame,
        backing: &mut VulkanEncoderFrame,
    ) -> Result<(), EncodeError> {
        let InputFrame { slot } = frame;
        let pair = &self.planes[slot];
        let mut access = alpha2_ffmpeg_bridge::VulkanEncodeFrameAccess::begin(&mut backing.0)
            .map_err(encode_adapter_error)?;
        self.copier
            .submit(&mut access, &pair.luma, &pair.chroma, self.size)
            .map_err(encode_adapter_error)?;
        self.stats.copy_operations = self.stats.copy_operations.saturating_add(2);
        self.stats.copy_bytes = self
            .stats
            .copy_bytes
            .saturating_add(EncoderInputLayout::Nv12.frame_bytes(self.size.0, self.size.1));
        Ok(())
    }

    /// Presentation timestamp for the frame at `time`, in encoder ticks.
    pub(super) fn timestamp(&self, time: MediaTime) -> i64 {
        av_rescale_q(time.ticks(), tick_base(), self.time_base)
    }

    pub(super) const fn stats(&self) -> EncoderPoolStats {
        self.stats
    }

    /// Waits on every copy's fence before the plane textures it reads drop.
    pub(super) fn finish(self) -> Result<(), EncodeError> {
        let Self { copier, planes, .. } = self;
        copier.finish().map_err(encode_adapter_error)?;
        drop(planes);
        Ok(())
    }
}

impl VulkanEncodeSession {
    pub(super) const fn selection(&self) -> &crate::NativeVideoEncoderSelection {
        &self.selection
    }

    pub(super) fn stage_timings(&self) -> EncodeStageTimings {
        self.muxer.with_audio_timings(self.stages)
    }

    /// Submits `frame` and muxes whatever packets it produced.
    pub(super) fn encode(
        &mut self,
        generation: u64,
        frame: &VulkanEncoderFrame,
    ) -> Result<(), EncodeError> {
        if generation != self.generation {
            return Err(EncodeError::Adapter(format!(
                "encoder surface from pool {generation} reached session {}",
                self.generation
            )));
        }
        timed(&mut self.stages.video_send_frame, || {
            self.context.send_frame(Some(&frame.0))
        })
        .map_err(encode_adapter_error)?;
        self.drain()
    }

    pub(super) fn write_audio(
        &mut self,
        block: &alpha2_media::AudioBlock,
    ) -> Result<(), EncodeError> {
        self.muxer.write_audio(block, &mut self.stages)
    }

    /// Flushes the audio and the encoder, then closes the output. Flushing
    /// and closing are one step because only a closed output renames the
    /// partial file into place.
    pub(super) fn finish(&mut self, cancel: &alpha2_media::CancelToken) -> Result<(), EncodeError> {
        // Every audio packet must exist before the muxer decides the final interleave.
        self.muxer.finish_audio()?;
        crate::encode::beat_encode_call();
        self.context
            .send_frame(None)
            .map_err(encode_adapter_error)?;
        self.drain()?;
        self.muxer.write_ready_audio(true, &mut self.stages)?;
        self.muxer.close(cancel)
    }

    pub(super) fn publish_report(&self) -> Option<crate::PublishReport> {
        self.muxer.publish_report()
    }

    pub(super) fn abort(&mut self) {
        self.muxer.abort();
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        loop {
            let received = timed(&mut self.stages.video_receive_packet, || {
                self.context.receive_packet()
            });
            match received {
                Ok(mut packet) => self.muxer.write_video(&mut packet, self.time_base)?,
                Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => {
                    return self.muxer.write_ready_audio(false, &mut self.stages);
                }
                Err(error) => return Err(encode_adapter_error(error)),
            }
        }
    }
}

/// Builds the encoder-input pool.
///
/// `avcodec_get_hw_frames_parameters` is a decode-side API and answers
/// `ENOENT` for an encoder, so the pool is described here. `FFmpeg`'s Vulkan
/// encoder requests no image usage of its own, so the pool is given the one
/// an export uses (see `vulkan_frames_add_usage`) before it is initialized.
fn vulkan_encode_frames(
    device: &AVHWDeviceContext,
    width: i32,
    height: i32,
) -> Result<AVHWFramesContext, EncodeError> {
    let mut frames = device.hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = ffi::AV_PIX_FMT_VULKAN;
        data.sw_format = ffi::AV_PIX_FMT_NV12;
        data.width = width;
        data.height = height;
        data.initial_pool_size = 0;
    }
    alpha2_ffmpeg_bridge::vulkan_frames_add_usage(
        &mut frames,
        alpha2_ffmpeg_bridge::VULKAN_ENCODE_INPUT_USAGE,
    )
    .map_err(encode_adapter_error)?;
    frames.init().map_err(encode_adapter_error)?;
    Ok(frames)
}

fn encode_adapter_error_at_open(error: &RsmpegError) -> EncodeError {
    EncodeError::Adapter(format!("h264_vulkan session could not open: {error}"))
}
