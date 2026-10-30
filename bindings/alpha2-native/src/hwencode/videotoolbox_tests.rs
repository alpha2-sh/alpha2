//! The `VideoToolbox` GPU-resident backend on this Mac's hardware encoder.
//!
//! Each test needs the renderer's `Metal` device and a hardware
//! `h264_videotoolbox` session; without them it skips, recorded as the
//! `videotoolbox-encode` capability.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use alpha2_media::{BitrateMode, DecodeEvent, Decoder, VideoRateControl};
use alpha2_native_gpu::SharedMetalContext;
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND};

use super::super::{HardwareEncodeDevice, HardwareEncodeOpenError, HardwareEncodeRequest};
use super::*;
use crate::OutputTarget;

thread_local! {
    static REFUSE_CONSTANT_BIT_RATE: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread's next open refuses `rate`, the way a Mac before
/// macOS 13 refuses `constant_bit_rate`.
pub(super) fn refuses_constant_bit_rate(rate: alpha2_media::VideoRateControl) -> bool {
    rate.mode == BitrateMode::Constant && REFUSE_CONSTANT_BIT_RATE.get()
}

/// The renderer's device with a working hardware encoder, or `None` (a
/// recorded skip).
fn device() -> Option<(SharedMetalContext, HardwareEncodeDevice)> {
    let made = pollster::block_on(alpha2_gpu::GpuContext::request_product())
        .map_err(|error| format!("{error:?}"))
        .and_then(|gpu| SharedMetalContext::from_gpu(gpu).map_err(|error| error.to_string()))
        .and_then(|metal| {
            let device = HardwareEncodeDevice::metal(metal.clone());
            device
                .capability(NativeVideoCodec::H264)
                .map(|_| (metal, device))
        });
    alpha2_test_support::available!(
        alpha2_test_support::Capability::VideotoolboxEncode,
        made.map_err(|error| format!("no VideoToolbox hardware encode ({error})"))
    )
}

fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("alpha2-videotoolbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory
}

fn request(output: &OutputTarget, rate_control: VideoRateControl) -> HardwareEncodeRequest<'_> {
    HardwareEncodeRequest {
        width: 128,
        height: 96,
        codec: NativeVideoCodec::H264,
        frame_rate: FrameRate::new(30, 1).expect("rate"),
        rate_control,
        color: ColorMetadata::EXPORT,
        output,
        container: alpha2_media::Container::Mp4,
        audio: None,
    }
}

fn variable() -> VideoRateControl {
    VideoRateControl::bitrate(4_000_000, BitrateMode::Variable).expect("rate")
}

fn open(
    device: &HardwareEncodeDevice,
    output: &Path,
    rate_control: VideoRateControl,
) -> (
    super::super::HardwareEncodeSession,
    super::super::HardwareEncodeInput,
) {
    device
        .open(&request(&OutputTarget::from(output), rate_control))
        .unwrap_or_else(|error| panic!("the session opens: {error}"))
}

/// Clears the frame's planes to one luma code and mid chroma, as the NV12
/// pack would, and submits the render.
fn pack(
    metal: &SharedMetalContext,
    input: &super::super::HardwareEncodeInput,
    frame: &super::super::InputFrame,
    luma: u8,
) {
    let gpu = metal.gpu();
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let (y, uv) = input.planes(frame);
    for (texture, color) in [
        (
            y,
            wgpu::Color {
                r: f64::from(luma) / 255.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        ),
        (
            uv,
            wgpu::Color {
                r: 128.0 / 255.0,
                g: 128.0 / 255.0,
                b: 0.0,
                a: 1.0,
            },
        ),
    ] {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pack"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    gpu.submit([encoder.finish()]);
}

/// Each decoded frame's mean visible luma.
fn mean_luma(path: &Path) -> Vec<f64> {
    let mut decoder = crate::NativeDecoder::open(path).expect("open the output");
    let cancel = alpha2_media::CancelToken::default();
    let mut means = Vec::new();
    loop {
        match decoder.decode_next(&cancel).expect("decode") {
            DecodeEvent::Video(frame) => {
                let plane = &frame.planes[0];
                let samples = plane
                    .bytes
                    .chunks(plane.stride as usize)
                    .take(plane.height as usize)
                    .flat_map(|row| &row[..plane.width as usize])
                    .map(|&value| f64::from(value))
                    .collect::<Vec<_>>();
                #[allow(clippy::cast_precision_loss)]
                means.push(samples.iter().sum::<f64>() / samples.len() as f64);
            }
            DecodeEvent::EndOfStream => return means,
            _ => {}
        }
    }
}

#[test]
fn the_capability_is_the_direct_target_shape_and_claims_only_modes_that_open() {
    let Some((_, device)) = device() else {
        return;
    };
    let capability = device
        .capability(NativeVideoCodec::H264)
        .expect("capability");
    assert_eq!(capability.backend, HardwareEncodeBackend::VideoToolbox);
    assert_eq!(capability.encoder, ENCODER);
    assert_eq!(capability.handoff, EncoderInputHandoff::DirectTarget);
    // The readback path can feed h264_videotoolbox too, so pinning it does not demand this path.
    assert!(!capability.gpu_input_only);
    assert_eq!(capability.plane_ring_slots, 0);
    assert_eq!(capability.handoff_retained_surfaces, 0);
    assert_eq!(capability.codec_retained_surfaces, CODEC_RETAINED_SURFACES);
    assert!(capability.rate_control_modes.contains(&"vbr"));
    let rate = FrameRate::new(30, 1).expect("rate");
    for (mode, claimed) in [
        (BitrateMode::Variable, "vbr"),
        (BitrateMode::Constant, "cbr"),
    ] {
        let rate_control = VideoRateControl::bitrate(4_000_000, mode).expect("rate");
        let opened = open_encoder(256, 256, rate_control, rate, ColorMetadata::EXPORT);
        assert_eq!(
            opened.is_ok(),
            capability.rate_control_modes.contains(&claimed),
            "{claimed} claimed {:?}, open {:?}",
            capability.rate_control_modes,
            opened.err()
        );
    }
}

#[test]
fn without_a_constant_bit_rate_cbr_is_not_claimed_and_its_refusal_says_why() {
    let Some((metal, device)) = device() else {
        return;
    };
    REFUSE_CONSTANT_BIT_RATE.set(true);
    let modes = probe_rate_control_modes(&metal);
    let directory = scratch("no-cbr");
    let output = directory.join("out.mp4");
    let constant = VideoRateControl::bitrate(4_000_000, BitrateMode::Constant).expect("rate");
    let refused = device.open(&request(&OutputTarget::from(&output), constant));
    REFUSE_CONSTANT_BIT_RATE.set(false);
    assert_eq!(modes, Ok(vec!["vbr"]));
    let Err(HardwareEncodeOpenError::Refused(reason)) = refused else {
        panic!("a constant rate is refused before the output exists");
    };
    assert!(reason.contains("macOS 13"), "{reason}");
    assert!(!output.exists());
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn a_session_encodes_what_the_renderer_drew_into_its_surfaces() {
    let Some((metal, device)) = device() else {
        return;
    };
    let directory = scratch("encode");
    let output = directory.join("out.mp4");
    let (mut session, mut input) = open(&device, &output, variable());
    assert_eq!(session.selection().encoder_name, ENCODER);
    let lumas = (0..10_u8).map(|index| 32 + index * 20).collect::<Vec<_>>();
    for (index, &luma) in lumas.iter().enumerate() {
        let mut surface = input.acquire_surface().expect("a surface");
        assert_eq!(surface.descriptor().visible, (128, 96));
        let frame = input.begin_frame(&surface).expect("begin");
        pack(&metal, &input, &frame, luma);
        input.submit(frame, &mut surface).expect("submit");
        let ticks = i64::try_from(index).expect("index") * TICKS_PER_SECOND / 30;
        input.set_time(&mut surface, MediaTime::from_ticks(ticks));
        session.encode(&surface).expect("encode");
    }
    session
        .finish(&alpha2_media::CancelToken::default())
        .expect("finish");
    let stats = input.stats();
    input.finish().expect("the input finishes");
    // The renderer drew into the surfaces themselves: no handoff copied anything.
    assert_eq!(stats.acquired, 10);
    assert_eq!((stats.copy_operations, stats.copy_bytes), (0, 0));
    assert!(stats.allocated >= 1 && stats.allocated <= 10, "{stats:?}");
    let means = mean_luma(&output);
    assert_eq!(means.len(), lumas.len());
    for (index, (mean, luma)) in means.iter().zip(&lumas).enumerate() {
        assert!(
            (mean - f64::from(*luma)).abs() < 3.0,
            "frame {index}: drew {luma}, decoded a mean of {mean}"
        );
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn a_surface_is_encoded_only_by_its_own_session_and_only_once_packed() {
    let Some((metal, device)) = device() else {
        return;
    };
    let directory = scratch("generation");
    let (first_output, second_output) = (directory.join("a.mp4"), directory.join("b.mp4"));
    let (mut first, mut first_input) = open(&device, &first_output, variable());
    let (mut second, mut second_input) = open(&device, &second_output, variable());

    let mut surface = first_input.acquire_surface().expect("a surface");
    let error = second_input
        .begin_frame(&surface)
        .err()
        .expect("another pool's surface");
    assert!(error.to_string().contains("reached pool"), "{error}");
    let error = second
        .encode(&surface)
        .expect_err("another session's surface");
    assert!(error.to_string().contains("reached session"), "{error}");

    // Begun but never submitted: nothing says the render that packs it finished.
    let error = first.encode(&surface).expect_err("an unpacked surface");
    assert!(error.to_string().contains("unpacked"), "{error}");

    let frame = first_input.begin_frame(&surface).expect("begin");
    pack(&metal, &first_input, &frame, 128);
    first_input.submit(frame, &mut surface).expect("submit");
    first_input.set_time(&mut surface, MediaTime::ZERO);
    first.encode(&surface).expect("its own session encodes it");

    first.abort();
    second.abort();
    drop((surface, first_input, second_input));
    let left = std::fs::read_dir(&directory)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    assert!(left.is_empty(), "left behind: {left:?}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn a_quality_level_runs_the_bitrate_fallback_and_says_so() {
    let Some((_, device)) = device() else {
        return;
    };
    let directory = scratch("quantizer");
    let output = directory.join("out.mp4");
    let quantized = VideoRateControl {
        quantizer: Some(23),
        ..variable()
    };
    let (mut session, _input) = open(&device, &output, quantized);
    let selection = session.selection().clone();
    session.abort();
    assert_eq!(selection.rate_control, crate::AppliedRateControl::Bitrate);
    assert!(
        selection
            .quantizer_refused
            .as_deref()
            .is_some_and(|reason| reason.contains(ENCODER)),
        "{selection:?}"
    );
    let _ = std::fs::remove_dir_all(directory);
}
