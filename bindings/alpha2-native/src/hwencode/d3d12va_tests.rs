//! The D3D12VA GPU-resident backend on this machine's D3D12 Video Encode.
//!
//! The capability's answers need only a D3D12 adapter; without one those
//! tests skip, unless `ALPHA2_REQUIRE_D3D12` is set. The session tests need
//! an adapter whose D3D12 Video Encode opens a session and encodes, and the
//! output tests one whose first packet a decoder can also start from; without
//! one they skip with the reason, unless `ALPHA2_REQUIRE_D3D12_ENCODE` is set.
//!
//! The session tests never decode what they write, so they run on a device
//! whose test encode opens but writes an undecodable first packet (an Intel
//! Iris Xe on driver 30.0.101.2079 does, and the capability refuses it for
//! an export): [`accepts_undecodable_packets`] lets their thread past that
//! one check. They open sessions but send no frame to one: the same driver
//! hangs or crashes inside its encode submission about once in a few
//! thousand frames, so on such a device the process encodes only the
//! capability's one test frame per codec.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use alpha2_media::{DecodeEvent, Decoder};
use alpha2_time::{FrameRate, TICKS_PER_SECOND};

use super::super::{
    EncoderSurface, HardwareEncodeDevice, HardwareEncodeInput, HardwareEncodeOpenError,
    HardwareEncodeRequest, HardwareEncodeSession, SessionBackend,
};
use super::*;

thread_local! {
    static ACCEPT_UNDECODABLE: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread's capability offers a device whose test encode
/// opened and encoded but wrote a first packet no decoder starts from.
pub(super) fn accepts_undecodable_packets() -> bool {
    ACCEPT_UNDECODABLE.get()
}

fn encode_required() -> bool {
    std::env::var_os("ALPHA2_REQUIRE_D3D12_ENCODE").is_some()
}

/// The renderer's D3D12 device as the host shares it, on the first adapter
/// `accept` takes, or `None` (and a note) when none does.
fn shared_where<T>(
    what: &str,
    required: bool,
    mut accept: impl FnMut(&SharedD3D12Context) -> Result<T, String>,
) -> Option<(SharedD3D12Context, T)> {
    let made = pollster::block_on(
        alpha2_gpu::GpuContext::request_high_performance_on_accepted(wgpu::Backends::DX12, |gpu| {
            let shared =
                SharedD3D12Context::from_gpu(gpu.clone()).map_err(|error| error.to_string())?;
            accept(&shared).map(|accepted| (shared, accepted))
        }),
    )
    .map_err(|error| format!("{error:?}"))
    .and_then(|found| found.accepted.ok_or_else(|| found.refusals.to_string()));
    match made {
        Ok(made) => Some(made),
        Err(error) => {
            assert!(!required, "{what} is required: {error}");
            eprintln!("skipped: no {what} ({error})");
            None
        }
    }
}

/// Any D3D12 device, whether it encodes or not.
fn any_device() -> Option<SharedD3D12Context> {
    shared_where(
        "D3D12 device",
        encode_required() || std::env::var_os("ALPHA2_REQUIRE_D3D12").is_some(),
        |_| Ok(()),
    )
    .map(|(shared, ())| shared)
}

/// A D3D12 device whose D3D12 Video Encode opens a session and encodes, and
/// the capability it leads with. It lets this thread's capability offer a
/// device whose first packet is undecodable, so the caller must not decode
/// what it writes.
fn session_device() -> Option<(
    SharedD3D12Context,
    HardwareEncodeDevice,
    HardwareEncodeCapability,
)> {
    ACCEPT_UNDECODABLE.set(true);
    device_where("D3D12 Video Encode session")
}

/// A D3D12 device whose D3D12 Video Encode writes a stream a decoder takes,
/// and the capability it leads with.
fn encoding_device() -> Option<(
    SharedD3D12Context,
    HardwareEncodeDevice,
    HardwareEncodeCapability,
)> {
    ACCEPT_UNDECODABLE.set(false);
    device_where("decodable D3D12 Video Encode")
}

fn device_where(
    what: &str,
) -> Option<(
    SharedD3D12Context,
    HardwareEncodeDevice,
    HardwareEncodeCapability,
)> {
    shared_where(what, encode_required(), |shared| {
        HardwareEncodeDevice::d3d12(shared.clone()).preferred_capability()
    })
    .map(|(shared, capability)| {
        let device = HardwareEncodeDevice::d3d12(shared.clone());
        (shared, device, capability)
    })
}

fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("alpha2-d3d12va-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory
}

/// The rate control an export on this capability encodes with: a constant
/// quantizer where the device has one, otherwise a variable rate or a
/// constant one, whichever it lists.
fn encodable(capability: &HardwareEncodeCapability) -> VideoRateControl {
    let modes = &capability.rate_control_modes;
    let rate = |mode| VideoRateControl::bitrate(4_000_000, mode).expect("rate");
    if modes.contains(&"disabled") {
        VideoRateControl {
            quantizer: Some(26),
            ..rate(BitrateMode::Variable)
        }
    } else if modes.contains(&"vbr") {
        rate(BitrateMode::Variable)
    } else {
        rate(BitrateMode::Constant)
    }
}

fn request<'a>(
    capability: &HardwareEncodeCapability,
    output: &'a Path,
    size: (u32, u32),
    rate_control: VideoRateControl,
) -> HardwareEncodeRequest<'a> {
    HardwareEncodeRequest {
        width: size.0,
        height: size.1,
        codec: capability.codec,
        frame_rate: FrameRate::new(30, 1).expect("rate"),
        rate_control,
        color: ColorMetadata::EXPORT,
        output,
        audio: None,
    }
}

fn open(
    device: &HardwareEncodeDevice,
    capability: &HardwareEncodeCapability,
    output: &Path,
    size: (u32, u32),
) -> (HardwareEncodeSession, HardwareEncodeInput) {
    device
        .open(&request(capability, output, size, encodable(capability)))
        .unwrap_or_else(|error| panic!("the session opens: {error}"))
}

/// Clears the frame's planes to one luma code and mid chroma, as the NV12
/// pack would, and submits the render.
fn pack(shared: &SharedD3D12Context, input: &HardwareEncodeInput, frame: &InputFrame, luma: u8) {
    let gpu = shared.gpu();
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

/// Acquires, packs at `luma`, submits and stamps one surface for frame
/// `index`.
fn packed_surface(
    shared: &SharedD3D12Context,
    input: &mut HardwareEncodeInput,
    index: usize,
    luma: u8,
) -> EncoderSurface {
    let mut surface = input.acquire_surface().expect("a surface");
    let frame = input.begin_frame(&surface).expect("begin");
    pack(shared, input, &frame, luma);
    input.submit(frame, &mut surface).expect("submit");
    let ticks = i64::try_from(index).expect("index") * TICKS_PER_SECOND / 30;
    input.set_time(&mut surface, MediaTime::from_ticks(ticks));
    surface
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

/// Asserts that `directory` holds nothing: no output and no partial file.
fn assert_empty(directory: &Path) {
    let left = std::fs::read_dir(directory)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    assert!(left.is_empty(), "left behind: {left:?}");
}

fn probe(codec: NativeVideoCodec, units: &[u8]) -> D3D12EncodeProbe {
    D3D12EncodeProbe {
        encoder: Encoder::for_codec(codec).expect("encoder").name.to_owned(),
        codec,
        rate_control: "disabled",
        coded_size: (256, 256),
        opened_profile: 0,
        extradata_size: 0,
        profile: None,
        first_packet_units: units.to_vec(),
        packets: 1,
        packet_bytes: 96,
        decode_failure: None,
    }
}

#[test]
fn a_first_packet_is_usable_only_with_its_parameter_sets_and_a_picture() {
    use NativeVideoCodec::{Av1, H264, Hevc};
    for (codec, units) in [
        (H264, &[9, 7, 8, 5][..]),
        (H264, &[7, 8, 6, 5]),
        (Hevc, &[35, 32, 33, 34, 19]),
        (Hevc, &[32, 33, 34, 39, 20]),
        (Av1, &[2, 1, 6]),
        (Av1, &[2, 1, 3, 4]),
    ] {
        assert_eq!(
            probe(codec, units).first_packet_decodes(),
            Ok(()),
            "{units:?}"
        );
    }
    // What an Iris Xe on driver 30.0.101.2079 writes: the H.264 picture over its SPS and PPS, and
    // HEVC's parameter sets with no picture after them.
    for (codec, units, lacks) in [
        (H264, &[5][..], "its parameter sets (NAL unit types [5])"),
        (
            Hevc,
            &[32, 33, 34],
            "a coded picture (NAL unit types [32, 33, 34])",
        ),
        (
            Av1,
            &[2],
            "both its parameter sets and a coded picture (OBU types [2])",
        ),
    ] {
        let reason = probe(codec, units)
            .first_packet_decodes()
            .expect_err("undecodable");
        assert!(reason.contains(lacks), "{reason}");
        assert!(reason.contains(Encoder::for_codec(codec).expect("encoder").name));
    }
    // Every unit present, but slices coded against other parameter sets: what `h264_d3d12va` writes
    // at High (see `Encoder::for_codec`).
    let failure = "the software h264 decoder decoded picture 0 of 1 corrupt";
    let reason = D3D12EncodeProbe {
        decode_failure: Some(failure.to_owned()),
        ..probe(H264, &[7, 8, 5])
    }
    .first_packet_decodes()
    .expect_err("undecodable");
    assert_eq!(
        reason,
        format!("h264_d3d12va's packets do not decode: {failure}")
    );
}

#[test]
fn the_units_of_a_packet_are_read_as_its_codec_frames_them() {
    // An Iris Xe's first `h264_d3d12va` packet: an IDR slice where the SPS and PPS should be.
    let h264 = [0, 0, 1, 0x25, 0x88, 0x80, 0x8f, 0, 0, 0];
    assert_eq!(stream_units(NativeVideoCodec::H264, &h264), (vec![5], None));
    let h264 = [
        0, 0, 0, 1, 0x67, 100, 0, 0x1f, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
    ];
    assert_eq!(
        stream_units(NativeVideoCodec::H264, &h264),
        (vec![7, 8, 5], Some(100))
    );
    // Its first `hevc_d3d12va` packet: VPS, SPS (Main) and PPS, and nothing after them.
    let hevc = [
        0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 0, 1, 0x44,
        0x01, 0xc0, 0, 0,
    ];
    assert_eq!(
        stream_units(NativeVideoCodec::Hevc, &hevc),
        (vec![32, 33, 34], Some(1))
    );
    // A temporal delimiter, a sequence header of profile 0 and a frame, each with its size.
    let av1 = [0x12, 0, 0x0a, 2, 0x00, 0x00, 0x32, 1, 0xff];
    assert_eq!(
        stream_units(NativeVideoCodec::Av1, &av1),
        (vec![2, 1, 6], Some(0))
    );
}

#[test]
fn every_codec_has_a_capability_or_a_reason_naming_why_not() {
    let Some(shared) = any_device() else {
        return;
    };
    let adapter = shared.gpu().capabilities().adapter_name.clone();
    let device = HardwareEncodeDevice::d3d12(shared.clone());
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
    ] {
        let encoder = Encoder::for_codec(codec).expect("a D3D12VA codec");
        let answer = device.capability(codec);
        eprintln!("{adapter} {codec:?}: {answer:?}");
        match answer {
            Ok(capability) => {
                assert!(AVCodec::find_encoder_by_name(encoder.c_name).is_some());
                assert_eq!(capability.encoder, encoder.name);
                assert_eq!(capability.codec, codec);
                assert_eq!(capability.backend, HardwareEncodeBackend::D3D12Va);
                assert_eq!(capability.handoff, EncoderInputHandoff::GpuCopy);
                assert!(capability.gpu_input_only && capability.shares_renderer_device);
                let extent = shared.encode_support(encoder.d3d12).expect("supported");
                assert_eq!(
                    (capability.minimum, capability.maximum),
                    (extent.minimum, extent.maximum)
                );
                assert_eq!(capability.rate_control_modes, extent.rate_control_modes);
            }
            Err(reason) => {
                if AVCodec::find_encoder_by_name(encoder.c_name).is_none() {
                    assert_eq!(
                        reason,
                        format!("this FFmpeg runtime has no {} encoder", encoder.name)
                    );
                } else if let Err(refused) = shared.encode_support(encoder.d3d12) {
                    // The device's own refusal, which names the adapter, the codec and the check:
                    // an Iris Xe has no AV1 encoder, an MX450 and WARP no ID3D12VideoDevice3.
                    assert_eq!(reason, refused);
                    assert!(reason.contains(&adapter), "{reason}");
                    if !reason.contains("ID3D12VideoDevice3") {
                        assert!(reason.contains(codec.as_str()), "{reason}");
                    }
                } else {
                    assert!(reason.contains(encoder.name), "{reason}");
                    assert!(reason.contains(&adapter), "{reason}");
                }
            }
        }
    }
    let reason = device
        .capability(NativeVideoCodec::Vp9)
        .expect_err("D3D12 Video Encode has no VP9 encoder in FFmpeg");
    assert!(reason.contains("not vp9"), "{reason}");
    let preferred = device.preferred_capability();
    let first = [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
    ]
    .into_iter()
    .find_map(|codec| device.capability(codec).ok());
    assert_eq!(preferred.as_ref().ok(), first.as_ref());
}

#[test]
fn the_test_encode_opens_the_advertised_profile() {
    let Some((shared, _, capability)) = session_device() else {
        return;
    };
    for codec in [NativeVideoCodec::H264, NativeVideoCodec::Hevc] {
        if HardwareEncodeDevice::d3d12(shared.clone())
            .capability(codec)
            .is_err()
        {
            continue;
        }
        // The capability's own test encode: on a bad driver every further encode is another chance
        // to hang or crash the process (see `drive`).
        let key = (
            shared.identity().device,
            Encoder::for_codec(codec).expect("encoder").d3d12,
        );
        let probe = PROBED
            .lock()
            .expect("probes")
            .iter()
            .find(|(probed, _)| *probed == key)
            .map(|(_, answer)| answer.clone())
            .expect("the capability ran its test encode")
            .expect("the test encode encoded");
        eprintln!("{probe:?}: {:?}", probe.first_packet_decodes());
        assert!(probe.packets >= 1 && probe.packet_bytes > 0, "{probe:?}");
        assert!(
            capability.rate_control_modes.contains(&probe.rate_control),
            "{probe:?}"
        );
        assert_eq!(probe.coded_size, (256, 256), "{probe:?}");
        // FFmpeg 8.1 writes no extradata, whatever `AV_CODEC_FLAG_GLOBAL_HEADER` asks.
        assert_eq!(probe.extradata_size, 0, "{probe:?}");
        // The profile the device was asked about: Main for H.264, whose High PPS `FFmpeg` 8.1.3
        // writes wrong (see `Encoder::for_codec`).
        let profile = if codec == NativeVideoCodec::H264 {
            ffi::AV_PROFILE_H264_MAIN.cast_signed()
        } else {
            ffi::AV_PROFILE_HEVC_MAIN.cast_signed()
        };
        assert_eq!(probe.opened_profile, profile, "{probe:?}");
        if probe.profile.is_some() {
            assert_eq!(probe.profile.map(i32::from), Some(profile), "{probe:?}");
        }
    }
}

#[test]
fn an_export_size_outside_the_device_extents_is_refused_before_any_output() {
    let Some((_, device, capability)) = session_device() else {
        return;
    };
    let directory = scratch("extents");
    let output = directory.join("out.mp4");
    let too_wide = (
        capability.maximum.0 + 2 * capability.granularity.0.max(2),
        256,
    );
    for size in [too_wide, (257, 256)] {
        let refused = device.open(&request(&capability, &output, size, encodable(&capability)));
        let Err(HardwareEncodeOpenError::Refused(reason)) = refused else {
            panic!("a {size:?} export is refused before the output exists");
        };
        assert!(
            reason.contains(&format!("{}x{}", size.0, size.1)),
            "{reason}"
        );
        assert_empty(&directory);
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn a_bitrate_the_device_cannot_encode_with_is_refused_before_any_output_naming_its_mode() {
    let Some((_, device, capability)) = session_device() else {
        return;
    };
    let directory = scratch("modes");
    let output = directory.join("out.mp4");
    let mut refused = 0;
    for (mode, name) in [
        (BitrateMode::Variable, "vbr"),
        (BitrateMode::Constant, "cbr"),
    ] {
        if capability.rate_control_modes.contains(&name) {
            continue;
        }
        let rate = VideoRateControl::bitrate(4_000_000, mode).expect("rate");
        let opened = device.open(&request(&capability, &output, (256, 192), rate));
        let Err(HardwareEncodeOpenError::Refused(reason)) = opened else {
            panic!("{name} is refused before the output exists");
        };
        assert!(
            reason.contains(&format!("no {name} rate control")),
            "{reason}"
        );
        assert!(reason.contains(capability.encoder), "{reason}");
        if capability.rate_control_modes.contains(&"disabled") {
            assert!(reason.contains("ask for a quantizer"), "{reason}");
        }
        assert_empty(&directory);
        refused += 1;
    }
    if refused == 0 {
        eprintln!(
            "skipped: {} encodes with both bitrate modes here",
            capability.encoder
        );
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn a_surface_is_encoded_only_by_its_own_session_and_only_once_packed() {
    let Some((shared, device, capability)) = session_device() else {
        return;
    };
    let directory = scratch("generation");
    let (first_output, second_output) = (directory.join("a.mp4"), directory.join("b.mp4"));
    let (mut first, mut first_input) = open(&device, &capability, &first_output, (256, 192));
    let (mut second, mut second_input) = open(&device, &capability, &second_output, (256, 192));

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

    let frame = first_input.begin_frame(&surface).expect("begin");
    let error = first.encode(&surface).expect_err("an unpacked surface");
    assert!(error.to_string().contains("unpacked"), "{error}");

    pack(&shared, &first_input, &frame, 128);
    first_input.submit(frame, &mut surface).expect("submit");
    first_input.set_time(&mut surface, MediaTime::ZERO);
    let stats = first_input.stats();
    assert_eq!(
        (stats.copy_operations, stats.copy_bytes),
        (2, EncoderInputLayout::Nv12.frame_bytes(256, 192))
    );

    first.abort();
    second.abort();
    drop(surface);
    first_input.finish().expect("the input finishes");
    second_input.finish().expect("the input finishes");
    assert_empty(&directory);
    let _ = std::fs::remove_dir_all(directory);
}

/// Encodes `frames` surfaces through `input` and the session's codec the
/// way the host does with an encode channel `depth` deep, and returns the
/// most surfaces the host held at once and the pool's statistics.
///
/// The packets are drained and dropped rather than muxed: this measures what
/// the encoder and the copier hold, whatever the packets say.
fn drive(
    shared: &SharedD3D12Context,
    session: &mut HardwareEncodeSession,
    input: &mut HardwareEncodeInput,
    depth: usize,
    frames: usize,
) -> (usize, EncoderPoolStats) {
    let SessionBackend::D3D12(session) = &mut session.0;
    let mut encode = |surface: EncoderSurface| {
        let super::super::SurfaceBacking::D3D12(frame) = &surface.backing;
        session
            .context
            .send_frame(Some(&frame.frame))
            .expect("send");
        while session.context.receive_packet().is_ok() {}
    };
    let mut channel = VecDeque::new();
    let mut peak = 0;
    for index in 0..frames {
        let luma = u8::try_from(32 + index % 160).expect("luma");
        channel.push_back(packed_surface(shared, input, index, luma));
        peak = peak.max(channel.len());
        if channel.len() > depth {
            encode(channel.pop_front().expect("queued"));
        }
    }
    channel.into_iter().for_each(&mut encode);
    session.context.send_frame(None).expect("flush");
    while session.context.receive_packet().is_ok() {}
    (peak, input.stats())
}

#[test]
fn the_pool_allocates_within_the_host_leases_plus_the_retained_surfaces() {
    // About four thousand encodes. On an Iris Xe (driver 30.0.101.2079) FFmpeg 8.1.3's D3D12VA
    // encoder hangs or crashes in the driver's submission about once per few thousand frames, so
    // this runs only where an export would, unless `ALPHA2_D3D12VA_MEASURE_UNDECODABLE` is set (how
    // `CODEC_RETAINED_SURFACES` was measured there).
    let device = if std::env::var_os("ALPHA2_D3D12VA_MEASURE_UNDECODABLE").is_some() {
        session_device()
    } else {
        encoding_device()
    };
    let Some((shared, device, _)) = device else {
        return;
    };
    let directory = scratch("pool");
    // Past the keyframe interval (60 at 30 fps), so an IDR and the references after it count too.
    let frames = 72;
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
    ] {
        let Ok(capability) = device.capability(codec) else {
            continue;
        };
        let mut sizes = vec![(256, 192), (1_920, 1_080), (3_840, 2_160)];
        sizes.retain(|size| capability.accepts(size.0, size.1).is_ok());
        let mut most = 0;
        for &size in &sizes {
            for depth in [0, 1, 2, 4, 8] {
                let output = directory.join(format!("{}x{}-{depth}.mp4", size.0, size.1));
                let (mut session, mut input) = open(&device, &capability, &output, size);
                let (peak, stats) = drive(&shared, &mut session, &mut input, depth, frames);
                session.abort();
                input.finish().expect("the input finishes");
                let beyond = usize::try_from(stats.allocated).expect("count") - peak;
                eprintln!(
                    "{} {}x{} depth {depth}: host peak {peak}, allocated {}, beyond {beyond}",
                    capability.encoder, size.0, size.1, stats.allocated
                );
                assert_eq!(stats.acquired, u64::try_from(frames).expect("frames"));
                assert!(
                    beyond <= capability.retained_surfaces(),
                    "{beyond} surfaces beyond the host's {peak} at depth {depth}, {size:?}: \
                     {stats:?}"
                );
                most = most.max(beyond);
            }
        }
        // The copier's slots are `handoff_retained_surfaces`; anything past them is the codec's.
        assert!(
            most.saturating_sub(capability.handoff_retained_surfaces)
                <= capability.codec_retained_surfaces,
            "{}: {most}",
            capability.encoder
        );
        eprintln!(
            "{}: at most {most} beyond the host's leases (copier {}, codec {})",
            capability.encoder,
            capability.handoff_retained_surfaces,
            capability.codec_retained_surfaces
        );
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn ten_frames_encode_with_parameter_sets_in_the_first_packet_only() {
    let Some((shared, device, capability)) = encoding_device() else {
        return;
    };
    let directory = scratch("encode");
    let output = directory.join("out.mp4");
    let (mut session, mut input) = open(&device, &capability, &output, (256, 192));
    assert_eq!(session.selection().encoder_name, capability.encoder);
    let lumas = (0..10_u8).map(|index| 32 + index * 20).collect::<Vec<_>>();
    for (index, &luma) in lumas.iter().enumerate() {
        let surface = packed_surface(&shared, &mut input, index, luma);
        session.encode(&surface).expect("encode");
    }
    session.finish().expect("finish");
    let stats = input.stats();
    input.finish().expect("the input finishes");
    assert_eq!(stats.acquired, 10);
    assert_eq!(stats.copy_operations, 20);

    // The parameter sets are in the sample description, and in no packet.
    let mut file = crate::decode::open_input(&output).expect("output");
    let parameters = file.streams()[0].codecpar().clone();
    let record = alpha2_ffmpeg_bridge::codec_parameters_extradata(&parameters);
    assert!(!record.is_empty(), "the sample description is empty");
    if capability.codec != NativeVideoCodec::Av1 {
        let hevc = capability.codec == NativeVideoCodec::Hevc;
        let sets: &[u8] = if hevc { &[32, 33, 34] } else { &[7, 8] };
        let length_size =
            crate::passthrough::nal_length_size(capability.codec, &record).expect("length size");
        let mut packets = 0;
        while let Some(packet) = file.read_packet().expect("packet") {
            let data = alpha2_ffmpeg_bridge::packet_bytes(&packet);
            let mut at = 0;
            while at + length_size < data.len() {
                let length = data[at..at + length_size]
                    .iter()
                    .fold(0_usize, |value, byte| (value << 8) | usize::from(*byte));
                let header = data[at + length_size];
                let nal = if hevc {
                    (header >> 1) & 0x3f
                } else {
                    header & 0x1f
                };
                assert!(
                    !sets.contains(&nal),
                    "packet {packets} carries NAL type {nal}"
                );
                at += length_size + length;
            }
            packets += 1;
        }
        assert_eq!(packets, 10);
    }
    drop(file);
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

/// What D3D12 Video Encode answered for HEVC on an Iris Xe on driver
/// 30.0.101.2079: a constant QP, and CBR and VBR refused in the configuration
/// `FFmpeg` opens them with.
fn iris_xe_modes(adapter: &str) -> DeviceModes {
    DeviceModes {
        adapter: adapter.to_owned(),
        encoder: Encoder::for_codec(NativeVideoCodec::Hevc).expect("encoder"),
        extent: D3D12EncodeExtent {
            codec: D3D12EncodeCodec::Hevc,
            profile: "main",
            minimum: (64, 64),
            maximum: (4096, 2304),
            granularity: (16, 16),
            max_p_picture_l0_references: 2,
            max_b_picture_l0_references: 0,
            max_l1_references: 0,
            rate_control_modes: vec!["disabled"],
            refused_rate_control_modes: vec![("cbr", 0x887A_0020_u32.cast_signed()), ("vbr", 0)],
        },
    }
}

#[test]
fn a_rate_control_open_reads_back_from_its_arguments() {
    let open = rate_control_probe::D3D12RateControlOpen {
        adapter: 0x0000_0000_000A_B12C,
        codec: NativeVideoCodec::Hevc,
        size: (1_920, 1_080),
        bit_rate: 0,
        options: vec![
            ("rc_mode".to_owned(), "CQP".to_owned()),
            ("qp".to_owned(), "26".to_owned()),
        ],
    };
    let arguments = open.arguments();
    assert_eq!(
        arguments.join(" "),
        "--adapter 00000000000AB12C --codec hevc --size 1920x1080 --bit-rate 0 --option \
         rc_mode=CQP --option qp=26"
    );
    assert_eq!(
        rate_control_probe::D3D12RateControlOpen::parse(&arguments),
        Ok(open)
    );
    let args = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
    for bad in [
        "--adapter zz --codec hevc --size 2x2 --bit-rate 0",
        "--adapter 1 --codec vp9 --size 2x2 --bit-rate 0",
        "--adapter 1 --codec hevc --size 2 --bit-rate 0",
        "--adapter 1 --codec hevc --size 2x2 --bit-rate many",
        "--adapter 1 --codec hevc --size 2x2 --bit-rate 0 --option qp",
        "--adapter 1 --codec hevc --size 2x2 --bit-rate 0 --option",
        "--adapter 1 --codec hevc --size 2x2 --bit-rate 0 --flag qp=1",
        "--codec hevc --adapter 1 --size 2x2 --bit-rate 0",
    ] {
        assert!(
            rate_control_probe::D3D12RateControlOpen::parse(&args(bad)).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn each_mechanism_asks_for_the_mode_the_device_names() {
    use rate_control_probe::device_mode;
    assert_eq!(device_mode(RateControlMechanism::ConstantBitrate), "cbr");
    assert_eq!(device_mode(RateControlMechanism::VariableBitrate), "vbr");
    assert_eq!(
        device_mode(RateControlMechanism::ConstantQuantizer),
        "disabled"
    );
}

/// The first adapter whose device reports the encoder answers; a mode it
/// does not list is refused with its own reason, never opened on the next
/// adapter.
#[test]
fn the_first_adapter_that_encodes_answers_and_refuses_what_it_does_not_list() {
    use rate_control_probe::answering_adapter;
    let adapters = [
        (
            "DX12 adapter 1",
            Err("NVIDIA GeForce MX450's D3D12VA device did not open: AVERROR(-22)".to_owned()),
        ),
        (
            "DX12 adapter 0",
            Ok(iris_xe_modes("Intel(R) Iris(R) Xe Graphics")),
        ),
        ("DX12 adapter 3", Ok(iris_xe_modes("another adapter"))),
    ];
    assert_eq!(
        answering_adapter(&adapters, "disabled", (1_920, 1_080)),
        Ok(1)
    );
    let refused = answering_adapter(&adapters, "cbr", (1_920, 1_080)).expect_err("cbr");
    assert_eq!(
        refused,
        "on DX12 adapter 0: Intel(R) Iris(R) Xe Graphics: hevc_d3d12va has no cbr rate control \
         here (D3D12 Video Encode refuses it with HRESULT 0x887A0020; it encodes with disabled); \
         ask for a quantizer instead"
    );
    let refused = answering_adapter(&adapters, "vbr", (1_920, 1_080)).expect_err("vbr");
    assert!(
        refused.contains("(D3D12 Video Encode refuses it;"),
        "{refused}"
    );
    let refused = answering_adapter(&adapters, "qvbr", (1_920, 1_080)).expect_err("qvbr");
    assert!(refused.contains("does not list it"), "{refused}");
    let refused = answering_adapter(&adapters, "disabled", (8_192, 4_320)).expect_err("8K");
    assert_eq!(
        refused,
        "on DX12 adapter 0: hevc_d3d12va codes 8192x4320 as 8192x4320, outside the device's \
         64x64 to 4096x2304"
    );
    let none = [
        (
            "DX12 adapter 1",
            Err("NVIDIA GeForce MX450's D3D12VA device did not open: AVERROR(-22)".to_owned()),
        ),
        (
            "DX12 adapter 0",
            Err("Intel(R) Iris(R) Xe Graphics reports no D3D12 video encoder for av1".to_owned()),
        ),
    ];
    assert_eq!(
        answering_adapter(&none, "disabled", (256, 256)),
        Err(
            "DX12 adapter 1: NVIDIA GeForce MX450's D3D12VA device did not open: AVERROR(-22); \
             DX12 adapter 0: Intel(R) Iris(R) Xe Graphics reports no D3D12 video encoder for av1"
                .to_owned()
        )
    );
    assert_eq!(
        answering_adapter(&[], "disabled", (256, 256)),
        Err("no DX12 adapter".to_owned())
    );
}

/// On this machine's DX12 adapters, every mode of every D3D12VA encoder is
/// either handed to the runner on a named adapter or refused with a reason.
/// The runner here encodes nothing: this binary's frames are kept for the
/// session tests (see the top of this file), and `alpha2 probe-encoders`'
/// own test runs the opens, in children.
#[test]
fn every_d3d12va_mode_is_opened_on_an_adapter_or_refused_with_a_reason() {
    rate_control_probe::encode_no_rate_control_frames();
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
    ] {
        let Ok(name) = d3d12va_encoder(codec) else {
            continue;
        };
        for mechanism in RateControlMechanism::ALL {
            let request = crate::rate_control::mechanism_request(name, mechanism, 2_000_000, 26)
                .expect("a D3D12VA spelling");
            match probe_d3d12va_rate_control(name, 256, 256, mechanism, &request) {
                Ok((packets, adapter)) => {
                    assert!(packets > 0, "{name} {}", mechanism.as_str());
                    assert!(adapter.starts_with("DX12 adapter "), "{adapter}");
                }
                Err(reason) => assert!(
                    reason.contains("DX12 adapter"),
                    "{name} {}: {reason}",
                    mechanism.as_str()
                ),
            }
        }
    }
}
