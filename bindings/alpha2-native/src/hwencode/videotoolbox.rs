//! The `VideoToolbox` backend: `h264_videotoolbox` fed with `IOSurface`s the
//! renderer draws into.
//!
//! The encoder-input pool is `FFmpeg`'s `VideoToolbox` frames pool, whose
//! pixel buffers are `IOSurface`-backed. Each buffer's two planes are aliased
//! as `Metal` render targets on the renderer's own device, and the NV12 pack
//! writes them directly: there is no plane ring and no copy between the
//! renderer and the encoder, and no pixel crosses the CPU.
//!
//! Nothing hands the encoder a GPU fence. The coordinator records the
//! renderer's submission that packed a surface; the encode worker waits for
//! that submission to complete before the surface reaches the encoder. The
//! coordinator never waits on the GPU.
//!
//! A surface returns to the pool when its last `AVBuffer` reference drops.
//! `h264_videotoolbox` holds one from `avcodec_send_frame` until its output
//! callback has consumed the frame (`FFmpeg` 8.0's `create_cv_pixel_buffer`
//! takes `frame->buf[0]` into the frame's `BufNode`), so a buffer the encoder
//! is still reading is never handed out again.

use std::collections::HashMap;
use std::sync::Mutex;

use alpha2_media::{ColorMetadata, EncodeError};
use alpha2_native_gpu::{MetalEncoderPlanes, SharedMetalContext};
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

const ENCODER: &str = "h264_videotoolbox";

/// Smallest output the hardware session accepts, measured on an Apple-silicon
/// GPU with `probe-gpu-encode`: every even size down to 2x2 opens one.
const MINIMUM: (u32, u32) = (2, 2);
/// Largest output the hardware session accepts, measured on an Apple-silicon
/// GPU with `probe-gpu-encode`: 4096x4096 opens, while 4112x4096, 4096x4112,
/// 5120x2880 and 7680x4320 are refused. Other Apple chips may differ, and a
/// size a device refuses still refuses at open, before any output exists.
const MAXIMUM: (u32, u32) = (4_096, 4_096);

/// What one `VideoToolbox` encode session turned out to require and produce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoToolboxEncodeProbe {
    /// `FFmpeg` encoder that opened.
    pub encoder: String,
    /// Luma and chroma plane sizes of a pool buffer, which aliased as render
    /// targets only because it is `IOSurface`-backed 8-bit video-range NV12.
    pub plane_sizes: [(u32, u32); 2],
    /// Packets drained from one submitted frame plus a flush.
    pub packets: u32,
    /// Bytes in those packets.
    pub packet_bytes: u32,
}

/// Opens `h264_videotoolbox` on a `VideoToolbox` frames pool and encodes one
/// pool frame, aliasing its planes on `metal` the way an export does.
///
/// # Errors
/// Returns an adapter failure when the encoder is absent, cannot open a
/// hardware session at this size, the pool buffer cannot be a render target,
/// or no packet is produced.
pub fn probe_videotoolbox_encode(
    metal: &SharedMetalContext,
    width: u32,
    height: u32,
) -> Result<VideoToolboxEncodeProbe, EncodeError> {
    let frame_rate = alpha2_time::FrameRate::new(30, 1)
        .map_err(|error| EncodeError::Adapter(format!("{error:?}")))?;
    let (mut frames, mut context, _) = open_encoder(
        width,
        height,
        alpha2_media::VideoRateControl {
            bitrate: 4_000_000,
            mode: alpha2_media::BitrateMode::Variable,
            quantizer: None,
        },
        frame_rate,
        ColorMetadata::EXPORT,
    )?;
    let mut frame = AVFrame::new();
    frames
        .get_buffer(&mut frame)
        .map_err(encode_adapter_error)?;
    let planes = metal
        .encoder_planes(&frame)
        .map_err(|error| EncodeError::Adapter(error.to_string()))?;
    let plane_sizes = [
        (planes.luma.width(), planes.luma.height()),
        (planes.chroma.width(), planes.chroma.height()),
    ];
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
        return Err(EncodeError::Adapter(format!(
            "{ENCODER} opened but produced no packet"
        )));
    }
    Ok(VideoToolboxEncodeProbe {
        encoder: ENCODER.to_owned(),
        plane_sizes,
        packets,
        packet_bytes,
    })
}

/// What `h264_videotoolbox` accepts here.
///
/// A registered encoder is not a usable one (a headless macOS container
/// registers it and cannot open it), so the first call opens a hardware
/// session and encodes a frame. The answer is kept for the process when it
/// cannot change: the session opened, or the Mac has no encoder for it. A
/// busy encoder (`kVTVideoEncoderNotAvailableNowErr`), a probe that did not
/// finish within the GPU wait timeout, or a failure nothing classifies is
/// asked again at the next call.
pub(super) fn capability(metal: &SharedMetalContext) -> Result<HardwareEncodeCapability, String> {
    static PROBE: Mutex<Option<Result<Vec<&'static str>, String>>> = Mutex::new(None);
    // Held across the probe, so two exports asking at once open one probe session.
    let mut kept = PROBE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(answer) = kept.as_ref() {
        return answer.clone().map(capability_with);
    }
    let (answer, kind) = bounded_probe(metal);
    if matches!(
        kind,
        None | Some(
            alpha2_media::OpenFailureKind::Absent | alpha2_media::OpenFailureKind::Unsuitable
        )
    ) {
        *kept = Some(answer.clone());
    }
    answer.map(capability_with)
}

/// [`probe_rate_control_modes`] on its own thread, waited on for at most the
/// GPU wait timeout (`ALPHA2_GPU_TIMEOUT_MS`), and what a failure is evidence
/// of; `None` with a success.
fn bounded_probe(
    metal: &SharedMetalContext,
) -> (
    Result<Vec<&'static str>, String>,
    Option<alpha2_media::OpenFailureKind>,
) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = metal.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("alpha2 {ENCODER} probe"))
        .spawn(move || {
            let mark = alpha2_ffmpeg_bridge::log_mark();
            let answer = probe_rate_control_modes(&worker);
            let lines = alpha2_ffmpeg_bridge::log_since(mark, &[ENCODER]);
            let kind = answer
                .as_ref()
                .err()
                .map(|error| crate::capabilities::classify_open_failure(ENCODER, error, &lines));
            let answer = answer
                .map_err(|error| format!("{error}{}", alpha2_ffmpeg_bridge::log_detail(&lines)));
            let _ = sender.send((answer, kind));
        });
    if let Err(error) = spawned {
        return (
            Err(format!("{ENCODER}: the probe did not start: {error}")),
            Some(alpha2_media::OpenFailureKind::Unknown),
        );
    }
    let timeout = alpha2_native_gpu::metal_wait_timeout();
    match receiver.recv_timeout(timeout) {
        Ok(answer) => answer,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (
            Err(format!(
                "{ENCODER}'s probe session did not finish within {timeout:?}"
            )),
            Some(alpha2_media::OpenFailureKind::Unknown),
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => (
            Err(format!("{ENCODER}'s probe ended without an answer")),
            Some(alpha2_media::OpenFailureKind::Unknown),
        ),
    }
}

/// The rate-control modes a hardware session opens with here, after one
/// session has encoded a frame.
///
/// Variable is the session the probe encodes with. Constant is claimed only
/// once a session has opened with it: `VideoToolbox` has
/// `kVTCompressionPropertyKey_ConstantBitRate` only from macOS 13, and the
/// runtime supports macOS 11, where `FFmpeg`'s `constant_bit_rate` fails the
/// open.
fn probe_rate_control_modes(metal: &SharedMetalContext) -> Result<Vec<&'static str>, String> {
    probe_videotoolbox_encode(metal, 256, 256)
        .map_err(|error| format!("{ENCODER} has no hardware session here: {error}"))?;
    let rate = alpha2_time::FrameRate::new(30, 1).map_err(|error| format!("{error:?}"))?;
    let constant = alpha2_media::VideoRateControl {
        bitrate: 4_000_000,
        mode: alpha2_media::BitrateMode::Constant,
        quantizer: None,
    };
    Ok(
        match open_encoder(256, 256, constant, rate, ColorMetadata::EXPORT) {
            Ok(_) => vec!["cbr", "vbr"],
            Err(error) => {
                alpha2_os::diagnostic!("alpha2 native: {ENCODER} reports no cbr: {error}");
                vec!["vbr"]
            }
        },
    )
}

fn capability_with(rate_control_modes: Vec<&'static str>) -> HardwareEncodeCapability {
    HardwareEncodeCapability {
        backend: HardwareEncodeBackend::VideoToolbox,
        encoder: ENCODER,
        codec: NativeVideoCodec::H264,
        gpu_input_only: false,
        input_layout: EncoderInputLayout::Nv12,
        handoff: EncoderInputHandoff::DirectTarget,
        // The encoder reads the IOSurface the renderer drew into, on the same SoC; nothing is
        // copied between devices.
        shares_renderer_device: true,
        minimum: MINIMUM,
        maximum: MAXIMUM,
        // Pool buffers are the output size; the encoder's macroblock padding is internal to it.
        granularity: (1, 1),
        // VideoToolbox does not publish its reference structure; alpha2 asks for no frame
        // reordering, as the readback path does.
        max_p_picture_l0_references: None,
        max_b_picture_l0_references: None,
        max_l1_references: None,
        rate_control_modes,
        plane_ring_slots: 0,
        handoff_retained_surfaces: 0,
        codec_retained_surfaces: CODEC_RETAINED_SURFACES,
        default_channel_depth: DEFAULT_CHANNEL_DEPTH,
    }
}

/// `h264_videotoolbox` encodes as fast at depth 0 as at any deeper one: on an
/// Apple-silicon GPU (three alternating rounds at 1080p and 4K) depths 0, 1,
/// 2, 4 and 8 ran within 0.1 % of each other: the encoder paces the export,
/// and each step deeper only holds another surface.
const DEFAULT_CHANNEL_DEPTH: usize = 0;

/// Surfaces `h264_videotoolbox` holds after `avcodec_send_frame` has returned,
/// until its output callback consumes them. Measured on an Apple-silicon GPU:
/// the pool allocated exactly six more surfaces than the host's lease peak at
/// channel depths 1, 2, 4 and 8, at 256x192, 1080p and 4K, and at depth 0
/// above 256x192 (three at 256x192).
const CODEC_RETAINED_SURFACES: usize = 6;

/// Opens `h264_videotoolbox`, its input half, and the output.
pub(super) fn open(
    metal: &SharedMetalContext,
    request: &HardwareEncodeRequest<'_>,
    generation: u64,
) -> Result<(VideoToolboxEncodeSession, VideoToolboxEncodeInput), HardwareEncodeOpenError> {
    let refused = |error: EncodeError| HardwareEncodeOpenError::Refused(error.to_string());
    let mark = alpha2_ffmpeg_bridge::log_mark();
    let (frames, context, selection) = open_encoder(
        request.width,
        request.height,
        request.rate_control,
        request.frame_rate,
        request.color,
    )
    .map_err(|error| super::open_refusal(ENCODER, mark, &error))?;
    let time_base = frame_time_base(request.frame_rate).map_err(refused)?;
    let input = VideoToolboxEncodeInput {
        metal: metal.clone(),
        frames,
        planes: HashMap::new(),
        size: (request.width, request.height),
        time_base,
        color: request.color,
        stats: EncoderPoolStats::default(),
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
        VideoToolboxEncodeSession {
            context,
            device: metal.gpu().device().clone(),
            time_base,
            muxer,
            stages: EncodeStageTimings::default(),
            selection,
            generation,
        },
        input,
    ))
}

fn frame_time_base(rate: alpha2_time::FrameRate) -> Result<AVRational, EncodeError> {
    let (numerator, denominator) = super::session_frame_rate(rate)?;
    Ok(ra(denominator, numerator))
}

/// Opens `h264_videotoolbox` on a new NV12 `VideoToolbox` frames pool.
fn open_encoder(
    width: u32,
    height: u32,
    rate_control: alpha2_media::VideoRateControl,
    rate: alpha2_time::FrameRate,
    color: ColorMetadata,
) -> Result<
    (
        AVHWFramesContext,
        AVCodecContext,
        crate::NativeVideoEncoderSelection,
    ),
    EncodeError,
> {
    let encoder = AVCodec::find_encoder_by_name(c"h264_videotoolbox").ok_or_else(|| {
        EncodeError::Adapter(format!("this FFmpeg runtime has no {ENCODER} encoder"))
    })?;
    let coded_width =
        i32::try_from(width).map_err(|_| EncodeError::Adapter("width is out of range".into()))?;
    let coded_height =
        i32::try_from(height).map_err(|_| EncodeError::Adapter("height is out of range".into()))?;
    let frames = videotoolbox_encode_frames(coded_width, coded_height)?;
    let time_base = frame_time_base(rate)?;
    let open = |quantized: Option<&crate::rate_control::MechanismRequest>| {
        let mut context = AVCodecContext::new(&encoder);
        context.set_width(coded_width);
        context.set_height(coded_height);
        context.set_pix_fmt(ffi::AV_PIX_FMT_VIDEOTOOLBOX);
        // `ff_encode_preinit` copies the pool's `sw_format` into `sw_pix_fmt`.
        context.set_time_base(time_base);
        context.set_framerate(ra(time_base.den, time_base.num));
        context.set_bit_rate(quantized.map_or_else(
            || i64::try_from(rate_control.bitrate).unwrap_or(i64::MAX),
            |quantized| quantized.bit_rate,
        ));
        context.set_gop_size(crate::capabilities::keyframe_interval(rate));
        // The readback path's VideoToolbox setting, for the same reason: no frame reordering.
        context.set_max_b_frames(crate::capabilities::encoder_max_b_frames(ENCODER));
        context.set_hw_frames_ctx(frames.clone());
        // MP4 carries SPS and PPS in its `avcC`, and the flag is read at open.
        context.set_flags(
            context.flags | i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0),
        );
        // `allow_sw=0` is FFmpeg's default and sets `RequireHardwareAcceleratedVideoEncoder`, so a
        // machine without the hardware encoder fails instead of encoding in software; spelled out
        // so the guarantee does not rest on a default.
        let color = crate::convert::color_options(color).set(c"allow_sw", c"0", 0);
        let options = match quantized {
            Some(quantized) => quantized.dictionary(color),
            None => crate::rate_control::rate_control_options(ENCODER, rate_control, color),
        };
        #[cfg(test)]
        if quantized.is_none() && tests::refuses_constant_bit_rate(rate_control) {
            return Err(EncodeError::Adapter(format!(
                "{ENCODER} hardware session could not open at {width}x{height}: Generic error \
                 in an external library{}",
                crate::rate_control::constant_rate_hint(ENCODER, rate_control)
            )));
        }
        let unused =
            alpha2_ffmpeg_bridge::open_codec_context(&mut context, options).map_err(|error| {
                EncodeError::Adapter(format!(
                    "{ENCODER} hardware session could not open at {width}x{height}: {error}{}",
                    match quantized {
                        Some(_) => "",
                        None => crate::rate_control::constant_rate_hint(ENCODER, rate_control),
                    }
                ))
            })?;
        crate::convert::reject_unused_encode_options(unused)?;
        if context.extradata_size <= 0 {
            return Err(EncodeError::Adapter(format!(
                "{ENCODER} opened without SPS/PPS extradata for the MP4 header"
            )));
        }
        Ok::<_, EncodeError>(context)
    };
    let (context, applied, quantizer_refused) =
        match crate::rate_control::quantizer_request(ENCODER, rate_control) {
            Some(Ok(quantized)) => match open(Some(&quantized)) {
                Ok(context) => (context, crate::AppliedRateControl::Quality, None),
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
            "alpha2 native export: {ENCODER} runs the bitrate fallback: {reason}"
        );
    }
    let selection = crate::NativeVideoEncoderSelection {
        encoder_name: ENCODER.to_owned(),
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

/// Builds the encoder-input pool: `IOSurface`-backed NV12 pixel buffers of
/// exactly the output size, allocated on demand.
fn videotoolbox_encode_frames(width: i32, height: i32) -> Result<AVHWFramesContext, EncodeError> {
    let device = AVHWDeviceContext::create(ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, None, None, 0)
        .map_err(|error| EncodeError::Adapter(format!("no VideoToolbox device: {error}")))?;
    let mut frames = device.hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = ffi::AV_PIX_FMT_VIDEOTOOLBOX;
        data.sw_format = ffi::AV_PIX_FMT_NV12;
        data.width = width;
        data.height = height;
        data.initial_pool_size = 0;
    }
    frames.init().map_err(encode_adapter_error)?;
    Ok(frames)
}

fn pixel_buffer(frame: &AVFrame) -> Result<usize, EncodeError> {
    let info =
        alpha2_ffmpeg_bridge::videotoolbox_frame_info(frame).map_err(encode_adapter_error)?;
    usize::try_from(info.pixel_buffer)
        .ok()
        .filter(|pointer| *pointer != 0)
        .ok_or_else(|| EncodeError::Adapter("VideoToolbox frame has no pixel buffer".into()))
}

/// The encode worker's half: the codec, the muxer, and the renderer's device,
/// whose queue it waits on before a surface reaches the encoder.
pub(super) struct VideoToolboxEncodeSession {
    context: AVCodecContext,
    device: wgpu::Device,
    time_base: AVRational,
    muxer: Muxer,
    stages: EncodeStageTimings,
    selection: crate::NativeVideoEncoderSelection,
    generation: u64,
}

/// The coordinator's half: the pool, and each pool buffer's planes as the
/// renderer's render targets.
///
/// The pool never frees a buffer while the session runs (`FFmpeg`'s buffer
/// pool keeps every buffer it made until the pool itself goes), so a buffer's
/// address names it for the session, and its planes are wrapped once.
pub(super) struct VideoToolboxEncodeInput {
    metal: SharedMetalContext,
    frames: AVHWFramesContext,
    planes: HashMap<usize, MetalEncoderPlanes>,
    size: (u32, u32),
    time_base: AVRational,
    color: ColorMetadata,
    stats: EncoderPoolStats,
}

/// One pixel buffer taken from the encoder-input pool, and the renderer
/// submission that packed it.
pub(super) struct VideoToolboxEncoderFrame {
    frame: AVFrame,
    pixel_buffer: usize,
    packed: Option<wgpu::SubmissionIndex>,
}

impl VideoToolboxEncoderFrame {
    pub(super) fn set_pts(&mut self, pts: i64) {
        self.frame.set_pts(pts);
    }
}

impl VideoToolboxEncodeInput {
    pub(super) fn descriptor(&self, generation: u64) -> EncoderSurfaceDescriptor {
        EncoderSurfaceDescriptor {
            backend: HardwareEncodeBackend::VideoToolbox,
            pool_generation: generation,
            layout: EncoderInputLayout::Nv12,
            visible: self.size,
            color: self.color,
        }
    }

    pub(super) fn acquire_frame(&mut self) -> Result<VideoToolboxEncoderFrame, EncodeError> {
        let mut frame = AVFrame::new();
        self.frames
            .get_buffer(&mut frame)
            .map_err(encode_adapter_error)?;
        let pixel_buffer = pixel_buffer(&frame)?;
        if !self.planes.contains_key(&pixel_buffer) {
            let planes = self
                .metal
                .encoder_planes(&frame)
                .map_err(|error| EncodeError::Adapter(error.to_string()))?;
            if (planes.luma.width(), planes.luma.height()) != self.size {
                return Err(EncodeError::Adapter(format!(
                    "the encoder pool made a {}x{} luma plane for a {}x{} output",
                    planes.luma.width(),
                    planes.luma.height(),
                    self.size.0,
                    self.size.1
                )));
            }
            self.planes.insert(pixel_buffer, planes);
        }
        self.stats.acquired = self.stats.acquired.saturating_add(1);
        self.stats.allocated = u64::try_from(self.planes.len()).unwrap_or(u64::MAX);
        Ok(VideoToolboxEncoderFrame {
            frame,
            pixel_buffer,
            packed: None,
        })
    }

    /// The planes are the surface's own, so there is nothing to wait for:
    /// the pool hands out only buffers nothing else still reads.
    pub(super) fn begin_frame(
        &self,
        surface: &VideoToolboxEncoderFrame,
    ) -> Result<InputFrame, EncodeError> {
        if self.planes.contains_key(&surface.pixel_buffer) {
            Ok(InputFrame {
                pixel_buffer: surface.pixel_buffer,
            })
        } else {
            Err(EncodeError::Adapter(
                "encoder surface has no render targets in this pool".into(),
            ))
        }
    }

    pub(super) fn planes(&self, frame: &InputFrame) -> (&wgpu::Texture, &wgpu::Texture) {
        let planes = &self.planes[&frame.pixel_buffer];
        (&planes.luma, &planes.chroma)
    }

    /// Records the submission the encoder must wait for. The render that
    /// packed `frame` has been submitted by now, and an empty submission after
    /// it completes only once it has.
    // Taking the token is the contract: a begun frame is submitted once.
    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn submit(
        &mut self,
        frame: InputFrame,
        backing: &mut VideoToolboxEncoderFrame,
    ) -> Result<(), EncodeError> {
        if frame.pixel_buffer != backing.pixel_buffer {
            return Err(EncodeError::Adapter(
                "a frame was packed into one encoder surface and submitted with another".into(),
            ));
        }
        backing.packed = Some(self.metal.gpu().submit(std::iter::empty()));
        Ok(())
    }

    pub(super) fn timestamp(&self, time: MediaTime) -> i64 {
        av_rescale_q(time.ticks(), tick_base(), self.time_base)
    }

    pub(super) const fn stats(&self) -> EncoderPoolStats {
        self.stats
    }

    /// wgpu tracks the render targets, so dropping them under a render still
    /// in flight defers their release; the pixel buffers are the pool's.
    // The input owns nothing that needs a wait.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn finish(self) -> Result<(), EncodeError> {
        drop(self);
        Ok(())
    }
}

impl VideoToolboxEncodeSession {
    pub(super) const fn selection(&self) -> &crate::NativeVideoEncoderSelection {
        &self.selection
    }

    pub(super) fn stage_timings(&self) -> EncodeStageTimings {
        self.muxer.with_audio_timings(self.stages)
    }

    /// Waits for the render that packed `frame`, then submits it and muxes
    /// whatever packets it produced.
    pub(super) fn encode(
        &mut self,
        generation: u64,
        frame: &VideoToolboxEncoderFrame,
    ) -> Result<(), EncodeError> {
        if generation != self.generation {
            return Err(EncodeError::Adapter(format!(
                "encoder surface from pool {generation} reached session {}",
                self.generation
            )));
        }
        let packed = frame.packed.clone().ok_or_else(|| {
            EncodeError::Adapter("an encoder surface reached the encoder unpacked".into())
        })?;
        let timeout = alpha2_native_gpu::metal_wait_timeout();
        let ready = timed(&mut self.stages.video_input_ready_wait, || {
            self.device.poll(wgpu::PollType::Wait {
                submission_index: Some(packed),
                timeout: Some(timeout),
            })
        });
        match ready {
            Ok(wgpu::PollStatus::WaitSucceeded | wgpu::PollStatus::QueueEmpty) => {}
            // The renderer's wait timed out, as the Vulkan copier's would: a hung GPU, not an
            // encoder failure.
            Ok(wgpu::PollStatus::Poll) | Err(wgpu::PollError::Timeout) => {
                return Err(EncodeError::gpu_timeout(format_args!(
                    "the render packing an encoder surface, after {timeout:?}"
                )));
            }
            Err(error) => {
                return Err(EncodeError::Adapter(format!(
                    "waiting for the render packing an encoder surface failed: {error}"
                )));
            }
        }
        let mark = alpha2_ffmpeg_bridge::log_mark();
        timed(&mut self.stages.video_send_frame, || {
            self.context.send_frame(Some(&frame.frame))
        })
        .map_err(|error| with_encoder_log(encode_adapter_error(error), mark))?;
        self.drain()
    }

    pub(super) fn write_audio(
        &mut self,
        block: &alpha2_media::AudioBlock,
    ) -> Result<(), EncodeError> {
        self.muxer.write_audio(block, &mut self.stages)
    }

    /// Flushes the audio and the encoder, then closes the output.
    pub(super) fn finish(&mut self, cancel: &alpha2_media::CancelToken) -> Result<(), EncodeError> {
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
            let mark = alpha2_ffmpeg_bridge::log_mark();
            let received = timed(&mut self.stages.video_receive_packet, || {
                self.context.receive_packet()
            });
            match received {
                Ok(mut packet) => {
                    crate::vt_sei::strip_vt_user_data_sei(&mut packet)?;
                    self.muxer.write_video(&mut packet, self.time_base)?;
                }
                Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => {
                    return self.muxer.write_ready_audio(false, &mut self.stages);
                }
                Err(error) => return Err(with_encoder_log(encode_adapter_error(error), mark)),
            }
        }
    }
}

/// `error` with the lines `h264_videotoolbox` logged since `mark` appended, so
/// the `OSStatus` its session failed with reaches the typed error.
fn with_encoder_log(error: EncodeError, mark: alpha2_ffmpeg_bridge::LogMark) -> EncodeError {
    let detail =
        alpha2_ffmpeg_bridge::log_detail(&alpha2_ffmpeg_bridge::log_since(mark, &[ENCODER]));
    match error {
        EncodeError::Adapter(message) if !detail.is_empty() => {
            EncodeError::Adapter(format!("{message}{detail}"))
        }
        EncodeError::Gpu { fault, message } if !detail.is_empty() => EncodeError::Gpu {
            fault,
            message: format!("{message}{detail}"),
        },
        other => other,
    }
}

#[cfg(test)]
#[path = "videotoolbox_tests.rs"]
mod tests;
