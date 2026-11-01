//! Native decode, encode, and colour-conversion tests.

use std::fs;
use std::path::{Path, PathBuf};

use alpha2_media::{
    AudioBlock, AudioBlockSink, BitrateMode, CancelToken, ChromaLocation, ColorMatrix,
    ColorMetadata, ColorRange, DecodeError, DecodeEvent, DecodeRequest, DecodedVideoFrame, Decoder,
    EncodeError, EncodeEvent, EncodeSink, Orientation, PixelFormat, Plane, Rect, Rotation,
    SampleAspectRatio, Speaker, StreamKind, VideoRateControl, YuvFormat,
};
use alpha2_time::{FrameRate, MediaTime};
use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avutil::{AVChannelLayout, AVDictionary, AVFrame, ra};
use rsmpeg::ffi;

use super::*;
use crate::capabilities::{
    NativeEncoderClass, encoder_class, encoder_is_compatible, encoder_max_b_frames,
    select_video_encoder, video_encoder_candidates,
};
use crate::convert::{
    orientation_from_display, reject_unused_decode_options, rotation_from_display, timestamp,
    unused_option_names, video_decoder_options, video_encoder_options,
};
use crate::decode::{effective_media_duration, normalize_audio_frame_layout};
use alpha2_ffmpeg_bridge::DisplayRotation;

/// Held around every child process a lib test spawns, and by tests that release a file lock and
/// expect it free again: on macOS a child between fork and exec keeps a just-released `flock`.
pub(crate) static CHILD_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes [`CHILD_PROCESS`], whatever a panicking holder left it as.
pub(crate) fn child_process_lock() -> std::sync::MutexGuard<'static, ()> {
    CHILD_PROCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The capability a hardware or platform encoder needs, or `None` for a software encoder,
/// which must encode everywhere.
pub(crate) fn encoder_capability(name: &str) -> Option<alpha2_test_support::Capability> {
    use alpha2_test_support::Capability;
    if name.ends_with("_nvenc") {
        Some(Capability::Nvenc)
    } else if name.ends_with("_vulkan") {
        Some(Capability::VulkanVideoEncode)
    } else if name.ends_with("_d3d12va") {
        Some(Capability::D3d12Encode)
    } else if name.ends_with("_videotoolbox") {
        Some(Capability::VideotoolboxEncode)
    } else if name.ends_with("_mf") {
        Some(Capability::MediaFoundation)
    } else if name.contains("openh264") {
        Some(Capability::Openh264)
    } else {
        None
    }
}

mod alpha;
mod audio_io;
mod containers;
mod copies;
mod damaged_input;
mod encoders;
mod frame_metadata;
mod hardware_decode;
mod muxing;
mod openh264;
mod outputs;
mod pixel_outputs;
mod seeking;

/// A scratch file named for `label` and the running test, with characters Windows refuses (such as
/// `:` in the thread name) replaced by `-`.
fn output_path(label: &str) -> PathBuf {
    crate::require_pinned_ffmpeg_for_tests();
    let stem = format!(
        "alpha2-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    )
    .replace(
        |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
        "-",
    );
    std::env::temp_dir().join(format!("{stem}.mp4"))
}

fn rgba_frame(pts: MediaTime, color: [u8; 4]) -> DecodedVideoFrame {
    rgba_frame_sized(pts, color, 16, 16)
}

fn rgba_frame_sized(pts: MediaTime, color: [u8; 4], width: u32, height: u32) -> DecodedVideoFrame {
    DecodedVideoFrame {
        pts,
        dts: None,
        duration: None,
        visible_rect: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        sample_aspect_ratio: SampleAspectRatio::default(),
        orientation: Orientation::UPRIGHT,
        pixel_format: PixelFormat::Rgba8,
        chroma_location: ChromaLocation::Unspecified,
        color: Some(ColorMetadata::default()),
        planes: vec![Plane {
            bytes: color.repeat((width * height) as usize).into(),
            stride: width * 4,
            width,
            height,
        }],
    }
}

/// A corpus file. Like [`output_path`], it first stops a run on anything but the pinned SDK.
fn corpus(name: &str) -> PathBuf {
    crate::require_pinned_ffmpeg_for_tests();
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
}

/// A fixture's pixel aspect ratio by `av_guess_sample_aspect_ratio`: the container's `pasp` if any,
/// else the codec's VUI. A decoded picture carries the container's ratio if declared, else its own.
fn sar(num: u32, den: u32) -> SampleAspectRatio {
    SampleAspectRatio { num, den }
}

/// `(fixture, probed ratio, ratio of the pictures before frame 30, from it)`.
fn pixel_aspect_fixtures() -> [(
    &'static str,
    SampleAspectRatio,
    SampleAspectRatio,
    SampleAspectRatio,
); 5] {
    [
        // `pasp` 1:1 and VUI 1:1.
        ("h264-bt709-copy.mp4", sar(1, 1), sar(1, 1), sar(1, 1)),
        // VUI 4:3, no `pasp`: the codec's ratio.
        ("h264-bt709-vui-sar.mp4", sar(4, 3), sar(4, 3), sar(4, 3)),
        // The same stream in MPEG-TS, which declares nothing.
        ("h264-bt709-vui-sar.ts", sar(4, 3), sar(4, 3), sar(4, 3)),
        // `pasp` 4:3 over a VUI of 1:1: the container's declaration wins.
        ("h264-bt709-pasp.mp4", sar(4, 3), sar(4, 3), sar(4, 3)),
        // VUI 4:3 for the first GOP, 1:1 from the second: each picture keeps its own.
        ("h264-bt709-sar-change.ts", sar(4, 3), sar(4, 3), sar(1, 1)),
    ]
}

fn first_frame(decoder: &mut NativeDecoder, name: &str) -> DecodedVideoFrame {
    let cancel = CancelToken::default();
    loop {
        match decoder.decode_next(&cancel).expect(name) {
            DecodeEvent::Video(frame) => return frame,
            DecodeEvent::EndOfStream => panic!("{name}: no video frame"),
            _ => {}
        }
    }
}

fn row16(plane: &Plane, row: u32) -> Vec<u16> {
    let start = usize::try_from(row * plane.stride).expect("offset");
    plane.bytes[start..start + usize::try_from(plane.width * 2).expect("row")]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|sample| u16::from_le_bytes(*sample))
        .collect()
}

const PQ_BT2020: ColorMetadata = ColorMetadata {
    primaries: alpha2_media::ColorPrimaries::Bt2020,
    transfer: alpha2_media::TransferFunction::Pq,
    matrix: ColorMatrix::Bt2020Ncl,
    range: ColorRange::Limited,
    dynamic_range: alpha2_media::DynamicRange::Hdr,
};

/// A hardware session on this machine's Vulkan decode device, or `None` (a
/// recorded `vulkan-video` skip).
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
fn hardware_decode_context() -> Option<HeldDevice<alpha2_native_gpu::SharedVulkanContext>> {
    let lock = vulkan_device_lock();
    let context = alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        alpha2_native_gpu::SharedVulkanContext::create()
            .map_err(|error| format!("no Vulkan video decode device ({error:?})"))
    )?;
    // Debug builds load the system validation layer, and versions older than 1.4 crash inside
    // `FFmpeg`'s video session setup.
    if let Some(hazard) = context.video_session_hazard() {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VulkanVideo,
            "{hazard}; run a release build, or point VK_LAYER_PATH at the 1.4 layer \
             scripts/fetch-vulkan-validation-layer.sh fetches"
        );
        return None;
    }
    Some(HeldDevice {
        context,
        _lock: lock,
    })
}

/// Held by every test that creates a Vulkan device (its own or `FFmpeg`'s) for the device's
/// life: NVIDIA's driver can deadlock or crash when two devices are created and destroyed
/// concurrently in one process.
fn vulkan_device_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
struct HeldDevice<T> {
    context: T,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
impl<T> std::ops::Deref for HeldDevice<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.context
    }
}

/// The renderer's `Metal` device, or `None`: a recorded `metal` skip without the device, a
/// `videotoolbox` one where H.264 has no hardware decode.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
fn hardware_decode_context() -> Option<alpha2_native_gpu::SharedMetalContext> {
    // Install before the device so the validation collector watches it.
    alpha2_gpu::validation_log::install_unless_off();
    let made = pollster::block_on(alpha2_gpu::GpuContext::request_product())
        .map_err(|error| format!("{error:?}"))
        .and_then(|gpu| {
            alpha2_native_gpu::SharedMetalContext::from_gpu(gpu).map_err(|error| error.to_string())
        });
    let context = alpha2_test_support::available!(
        alpha2_test_support::Capability::Metal,
        made.map_err(|error| format!("no Metal device ({error})"))
    )?;
    if !alpha2_native_gpu::videotoolbox_hardware_decodes(alpha2_native_gpu::VideoToolboxCodec::H264)
    {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::Videotoolbox,
            "VideoToolbox decodes no H.264 in hardware on this Mac"
        );
        return None;
    }
    if let Some(reason) = alpha2_test_support::virtual_machine() {
        alpha2_test_support::skip!(alpha2_test_support::Capability::Videotoolbox, "{reason}");
        return None;
    }
    Some(context)
}

/// The renderer's DX12 device shared with D3D12VA, or `None` (a recorded `d3d12-video` skip).
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
fn hardware_decode_context() -> Option<alpha2_native_gpu::SharedD3D12Context> {
    // Install before the device so the validation collector watches it.
    alpha2_gpu::validation_log::install_unless_off();
    let made = pollster::block_on(alpha2_gpu::GpuContext::request_product_accepted(|gpu| {
        alpha2_native_gpu::SharedD3D12Context::from_gpu(gpu.clone())
    }))
    .map_err(|error| format!("{error:?}"))
    .and_then(|found| found.accepted.ok_or_else(|| found.refusals.to_string()));
    alpha2_test_support::available!(
        alpha2_test_support::Capability::D3d12Video,
        made.map_err(|error| format!("no D3D12VA device ({error})"))
    )
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
fn open_hardware_decoder(
    path: PathBuf,
    context: &alpha2_native_gpu::SharedVulkanContext,
) -> Result<NativeExportDecoder, HardwareOpenError> {
    NativeExportDecoder::open_vulkan(path, 1, context, 1)
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
fn open_hardware_decoder(
    path: PathBuf,
    context: &alpha2_native_gpu::SharedMetalContext,
) -> Result<NativeExportDecoder, HardwareOpenError> {
    NativeExportDecoder::open_videotoolbox(path, 1, context, 1)
}

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
fn open_hardware_decoder(
    path: PathBuf,
    context: &alpha2_native_gpu::SharedD3D12Context,
) -> Result<NativeExportDecoder, HardwareOpenError> {
    NativeExportDecoder::open_d3d12(path, 1, context, 1)
}

fn partial_beside(path: &Path) -> PathBuf {
    crate::partial::partial_output_path(path).expect("partial path")
}

fn av_config(audio_sample_rate: u32) -> AvEncodeConfig {
    AvEncodeConfig {
        video: VideoEncodeConfig {
            width: 16,
            height: 16,
            frame_rate: FrameRate::new(30, 1).expect("rate"),
            color: ColorMetadata::default(),
            rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable).unwrap(),
            codec: NativeVideoCodec::Mpeg4Part2,
            display: crate::VideoDisplay::default(),
            container: alpha2_media::Container::Mp4,
            pixels: alpha2_media::PixelLayout::YUV420P,
        },
        audio_sample_rate,
        audio_channels: 2,
        audio_codec: NativeAudioCodec::Aac,
        audio_bit_rate: 128_000,
    }
}

/// An opened MPEG-4 Part 2 encoder with global headers, as an MP4 video stream needs.
fn mp4_video_encoder() -> AVCodecContext {
    let codec = AVCodec::find_encoder(ffi::AV_CODEC_ID_MPEG4).expect("mpeg4 encoder");
    let mut encoder = AVCodecContext::new(&codec);
    encoder.set_width(16);
    encoder.set_height(16);
    encoder.set_pix_fmt(ffi::AV_PIX_FMT_YUV420P);
    encoder.set_time_base(ra(1, 30));
    encoder.set_flags(encoder.flags | i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0));
    encoder.open(None).expect("open mpeg4");
    encoder
}

fn seek_request(seconds_ticks: i64) -> DecodeRequest {
    DecodeRequest {
        source: alpha2_media::SourceId::new("audit").expect("id"),
        time: MediaTime::from_ticks(seconds_ticks),
        kind: alpha2_media::DecodeRequestKind::Seek,
        generation: 1,
    }
}

/// A 440 Hz tone at `rate`, `samples` long, from `first`.
#[allow(clippy::cast_precision_loss)]
fn tone_block(first: i64, samples: usize, rate: u32) -> AudioBlock {
    let channel = (0..samples)
        .map(|n| {
            let t = (first + i64::try_from(n).expect("small")) as f32 / rate as f32;
            0.25 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
        })
        .collect::<Vec<_>>();
    AudioBlock {
        start_sample: first,
        sample_rate: rate,
        speakers: alpha2_media::SpeakerLayout::STEREO,
        channels: vec![channel.clone(), channel],
    }
}

/// The same through the muxer the GPU-resident and packet-copy exports use.
fn export_muxed_audio(label: &str, rate: u32, samples: usize) -> PathBuf {
    let path = output_path(label);
    let _ = fs::remove_file(&path);
    let mut muxer = crate::mux::Muxer::open(
        &path,
        alpha2_media::Container::Mp4,
        &mp4_video_encoder(),
        ra(1, 30),
        Some(crate::AudioStreamConfig {
            codec: crate::NativeAudioCodec::Aac,
            sample_rate: rate,
            channels: 2,
            bit_rate: 128_000,
        }),
    )
    .expect("muxer");
    let mut stages = crate::EncodeStageTimings::default();
    let mut first = 0;
    while first < samples {
        let count = (samples - first).min(4_096);
        muxer
            .write_audio(
                &tone_block(i64::try_from(first).expect("small"), count, rate),
                &mut stages,
            )
            .expect("audio");
        first += count;
    }
    muxer.finish_audio().expect("finish audio");
    muxer
        .write_ready_audio(true, &mut stages)
        .expect("write audio");
    muxer
        .close(&alpha2_media::CancelToken::default())
        .expect("close");
    path
}

/// The exported audio stream's rate and where its last packet ends, in samples. (The edit list and
/// the decoder are both coarser.)
fn exported_audio(path: &Path) -> (u32, i64) {
    let probed = probe(path).expect("probe");
    let index = probed.selected_audio.expect("audio stream");
    let rate = probed
        .selected_audio_stream()
        .and_then(|stream| stream.sample_rate)
        .expect("rate");
    let mut input = crate::decode::open_input(path).expect("input");
    let time_base = input.streams()[index].time_base;
    assert_eq!(
        (time_base.num, time_base.den),
        (1, i32::try_from(rate).expect("rate"))
    );
    let mut end = 0;
    while let Some(packet) = input.read_packet().expect("packet") {
        if usize::try_from(packet.stream_index).ok() == Some(index) {
            end = packet.pts + packet.duration;
        }
    }
    (rate, end)
}
