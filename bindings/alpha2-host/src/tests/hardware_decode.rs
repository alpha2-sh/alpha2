//! Hardware decode: backend choice and fallback, surface import, and software decode parity.

use super::*;

#[test]
fn every_hardware_fallback_reason_reports_under_its_own_key() {
    use alpha2_native::HardwareDecodeFallback;

    let reasons = [
        HardwareDecodeFallback::ContextUnavailable,
        HardwareDecodeFallback::NoHardwareConfig,
        HardwareDecodeFallback::UnsupportedColor,
        HardwareDecodeFallback::UnsupportedProfile,
        HardwareDecodeFallback::UnsupportedSurfaceFormat,
        HardwareDecodeFallback::SourceUnavailable,
        HardwareDecodeFallback::DecoderOpenFailed,
        HardwareDecodeFallback::ReverseWindow,
        HardwareDecodeFallback::UnsupportedCrop,
    ];
    let mut counts = HardwareFallbackCounts::default();
    for reason in reasons {
        counts.record(reason, reason.as_str());
    }
    let entries = counts.entries().collect::<Vec<_>>();
    assert_eq!(entries.len(), reasons.len());
    for reason in reasons {
        assert_eq!(
            entries
                .iter()
                .find(|(key, _)| *key == reason.as_str())
                .map(|(_, count)| *count),
            Some(1),
            "{} has no slot of its own",
            reason.as_str()
        );
    }
}

/// The first message per reason is what the report keeps: a later fallback of
/// the same reason, a merged shard's, or one counted before a snapshot does
/// not replace it.
#[test]
fn the_first_fallback_message_per_reason_is_kept() {
    use alpha2_native::HardwareDecodeFallback::{DecoderOpenFailed, NoHardwareConfig};

    let mut counts = HardwareFallbackCounts::default();
    counts.record(
        NoHardwareConfig,
        "VideoToolbox has no hardware decoder for av1 here",
    );
    counts.record(NoHardwareConfig, "a later refusal");
    let before = counts.clone();
    let mut shard = HardwareFallbackCounts::default();
    shard.record(NoHardwareConfig, "a shard's refusal");
    shard.record(DecoderOpenFailed, "OSStatus -12906");
    counts.merge(&shard);
    assert_eq!(
        counts.messages().collect::<Vec<_>>(),
        [
            (
                "noHardwareConfig",
                "VideoToolbox has no hardware decoder for av1 here"
            ),
            ("decoderOpenFailed", "OSStatus -12906"),
        ]
    );
    assert_eq!(
        counts.entries().collect::<Vec<_>>(),
        [("noHardwareConfig", 3), ("decoderOpenFailed", 1)]
    );
    let since = counts.since(&before);
    assert_eq!(
        since.messages().collect::<Vec<_>>(),
        [("decoderOpenFailed", "OSStatus -12906")]
    );
    assert_eq!(
        since.entries().collect::<Vec<_>>(),
        [("noHardwareConfig", 1), ("decoderOpenFailed", 1)]
    );
}

/// An explicit `WGPU_BACKEND` without Vulkan skips the Linux shared device,
/// which is the renderer's, as the documented override says: `auto` decodes in
/// software and says why, `hardware-required` refuses, and an encode-only
/// device is simply not made.
#[test]
fn an_explicit_backend_without_the_shared_one_skips_the_shared_device() {
    use wgpu::Backends;
    assert_eq!(backend_excluded(None, Backends::VULKAN, "Vulkan"), None);
    assert_eq!(
        backend_excluded(
            Some(Backends::VULKAN | Backends::GL),
            Backends::VULKAN,
            "Vulkan"
        ),
        None
    );
    let reason = backend_excluded(
        Some(Backends::from_comma_list("gl")),
        Backends::VULKAN,
        "Vulkan",
    )
    .expect("GL leaves Vulkan out");
    assert!(reason.contains("WGPU_BACKEND"), "{reason}");

    let refuse = || -> Result<u8, std::io::Error> { panic!("an excluded device is not made") };
    let (context, unavailable) =
        shared_device_context(VideoDecodePolicy::Auto, true, Some(reason.clone()), refuse)
            .expect("auto");
    assert_eq!(context, None);
    assert_eq!(
        unavailable.as_deref(),
        cfg!(feature = "hwdecode").then_some(reason.as_str())
    );
    let (context, unavailable) = shared_device_context(
        VideoDecodePolicy::Software,
        true,
        Some(reason.clone()),
        refuse,
    )
    .expect("software");
    assert_eq!((context, unavailable), (None, None));
    if cfg!(feature = "hwdecode") {
        let error = shared_device_context(
            VideoDecodePolicy::HardwareRequired,
            true,
            Some(reason.clone()),
            refuse,
        )
        .expect_err("hardware is required");
        assert_eq!(error.code, "HARDWARE_DECODE_UNAVAILABLE");
        assert_eq!(error.message, reason);
    }
    let (context, unavailable) = shared_device_context(VideoDecodePolicy::Auto, true, None, || {
        Ok::<u8, std::io::Error>(7)
    })
    .expect("made");
    assert_eq!((context, unavailable), (Some(7), None));
}

/// A validation layer older than 1.4 crashes or hangs `FFmpeg`'s decode
/// sessions as it does its encode ones, so both are kept off the device. With
/// such a layer, `auto` decodes in software and says why, `hardware-required`
/// refuses to start, and software has nothing to say.
#[test]
fn a_video_session_hazard_keeps_hardware_decode_off_the_device() {
    let hazard = "the loaded Vulkan validation layer is 1.3.204".to_owned();
    assert_eq!(
        video_session_decode(VideoDecodePolicy::Auto, None).expect("auto"),
        None
    );
    assert_eq!(
        video_session_decode(VideoDecodePolicy::Software, Some(hazard.clone())).expect("software"),
        None
    );
    let auto = video_session_decode(VideoDecodePolicy::Auto, Some(hazard.clone())).expect("auto");
    assert_eq!(
        auto.as_deref(),
        cfg!(feature = "hwdecode").then_some(hazard.as_str())
    );
    if cfg!(feature = "hwdecode") {
        let error = video_session_decode(VideoDecodePolicy::HardwareRequired, Some(hazard.clone()))
            .expect_err("hardware is required");
        assert_eq!(error.code, "HARDWARE_DECODE_UNAVAILABLE");
        assert_eq!(error.message, hazard);
    }
}

/// A host on a device that loads a validation layer older than 1.4 opens no
/// decode session there: `auto` reports software decode and why, and
/// `hardware-required` refuses to start. The layer is the system's,
/// asked for through `VK_INSTANCE_LAYERS` in a child process so that a
/// release build and a 1.4 `VK_LAYER_PATH` reach it too.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[test]
fn a_host_under_a_validation_layer_older_than_1_4_decodes_in_software() {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::hardware_decode::old_validation_layer_host_child",
            "--ignored",
            "--nocapture",
        ])
        .env("VK_INSTANCE_LAYERS", "VK_LAYER_KHRONOS_validation")
        .env_remove("VK_LAYER_PATH")
        .output()
        .expect("child");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}\n{stderr}", output.status);
    if let Some(note) = stderr.lines().find(|line| line.starts_with("skipped:")) {
        eprintln!("{note}");
    }
}

/// The child half of
/// `a_host_under_a_validation_layer_older_than_1_4_decodes_in_software`.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[test]
#[ignore = "run by a_host_under_a_validation_layer_older_than_1_4_decodes_in_software"]
fn old_validation_layer_host_child() {
    let Some(context) = alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        alpha2_native_gpu::SharedVulkanContext::create()
            .map_err(|error| format!("no Vulkan video decode device ({error:?})"))
    ) else {
        return;
    };
    let Some(hazard) = context.video_session_hazard() else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::OldVulkanValidationLayer,
            "the system validation layer is {:?}, not one older than 1.4",
            context.validation_layer()
        );
        return;
    };
    drop(context);
    let host = create_gpu_media_host(VideoDecodePolicy::Auto).expect("an auto host starts");
    assert!(!host.hardware_decode_ready(), "{hazard}");
    assert_eq!(
        host.hardware_decode_unavailable.as_deref(),
        Some(hazard.as_str())
    );
    assert_eq!(
        host.hardware_video_decode(),
        crate::assets::HardwareVideoDecode::None
    );
    drop(host);
    let error = create_gpu_media_host(VideoDecodePolicy::HardwareRequired)
        .err()
        .expect("hardware-required refuses to start");
    assert_eq!(error.code, "HARDWARE_DECODE_UNAVAILABLE");
    assert_eq!(error.message, hazard);
}

/// The streams [`an_imported_decoder_surface_renders_what_a_software_decode_does`]
/// exports: name, codec, whether it is 10-bit, and its size. The 320x180 ones
/// have a height no codec's block size divides, so their decoder surfaces are
/// taller than the picture (192 rows for H.264, 184 for HEVC and AV1); the
/// 10-bit ones import as P010.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
const IMPORT_PARITY_STREAMS: [(&str, &str, bool, u32, u32); 7] = [
    ("h264-padded.mp4", "h264", false, 320, 180),
    ("hevc-padded.mp4", "hevc", false, 320, 180),
    ("hevc-main10-padded.mp4", "hevc", true, 320, 180),
    ("av1-padded.mp4", "av1", false, 320, 180),
    ("av1-10bit-padded.mp4", "av1", true, 320, 180),
    ("vp9-128.webm", "vp9", false, 192, 128),
    ("h264-high10.mp4", "h264", true, 128, 96),
];

/// Whether this machine's hardware decoder takes `codec` (at 10 bits when
/// `ten_bit`): `Some` from the device's own answer, `None` where the platform
/// has no per-codec query (Windows), so the export itself says.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[allow(clippy::unnecessary_wraps)]
fn decodes_in_hardware(
    context: &alpha2_native_gpu::SharedVulkanContext,
    codec: &str,
    ten_bit: bool,
) -> Option<bool> {
    Some(context.decode_codecs().contains(&codec) && (!ten_bit || context.supports_ten_bit()))
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
#[allow(clippy::unnecessary_wraps)]
fn decodes_in_hardware(
    context: &alpha2_native_gpu::SharedMetalContext,
    codec: &str,
    ten_bit: bool,
) -> Option<bool> {
    use alpha2_native_gpu::{VideoToolboxCodec, videotoolbox_hardware_decodes};
    let codec = match codec {
        "h264" => VideoToolboxCodec::H264,
        "hevc" => VideoToolboxCodec::Hevc,
        "vp9" => VideoToolboxCodec::Vp9,
        "av1" => VideoToolboxCodec::Av1,
        other => panic!("no VideoToolbox codec {other}"),
    };
    Some(videotoolbox_hardware_decodes(codec) && (!ten_bit || context.supports_ten_bit()))
}

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
#[allow(clippy::unnecessary_wraps, clippy::trivially_copy_pass_by_ref)]
const fn decodes_in_hardware(_context: &(), _codec: &str, _ten_bit: bool) -> Option<bool> {
    None
}

/// The device hardware decode runs on here, or `None` (a recorded skip).
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
fn parity_device() -> Option<alpha2_native_gpu::SharedVulkanContext> {
    vulkan_video_context()
}

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
fn parity_device() -> Option<alpha2_native_gpu::SharedMetalContext> {
    if !create_gpu_media_host(VideoDecodePolicy::Auto)
        .expect("host")
        .hardware_decode_ready()
    {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::Videotoolbox,
            "this host has no VideoToolbox decode"
        );
        return None;
    }
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_product()).expect("a GPU device");
    Some(alpha2_native_gpu::SharedMetalContext::from_gpu(gpu).expect("the shared Metal device"))
}

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
fn parity_device() -> Option<()> {
    if create_gpu_media_host(VideoDecodePolicy::Auto)
        .expect("host")
        .hardware_decode_ready()
    {
        Some(())
    } else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::D3d12Video,
            "this host has no D3D12VA decode"
        );
        None
    }
}

/// A decoder surface the renderer imports renders the picture a software decode
/// of the same stream does, plane for plane: each codec's session, the P010
/// plane views, and the padded coded extent the surface is sampled against.
/// Each export runs in a child, since the frame capture is read from its
/// environment, and the hardware one must import every frame with no GPU
/// diagnostic. Decoding is exact, so the two renders differ only by how each
/// path's samples reach the shader: at most one code, where the 10-bit paths
/// round to 8 bits apart. The share of off-by-one samples is printed, not
/// asserted: which 10-bit ties round up differs by GPU.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn an_imported_decoder_surface_renders_what_a_software_decode_does() {
    let _gpu = gpu_test_lock();
    let Some(device) = parity_device() else {
        return;
    };
    let directory = scratch_directory("import-parity");
    let mut checked = 0;
    for (name, codec, ten_bit, width, height) in IMPORT_PARITY_STREAMS {
        // Decoders answer per codec, not per profile: whether H.264 High 10 decodes in hardware
        // (most do not) only the export can say.
        let claimed = if codec == "h264" && ten_bit {
            None
        } else {
            decodes_in_hardware(&device, codec, ten_bit)
        };
        if claimed == Some(false) {
            eprintln!("note: this device does not decode {name} in hardware");
            continue;
        }
        let capture = |policy: &str| {
            let path = directory.join(format!("{name}.{policy}.y4m"));
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "tests::hardware_decode::import_parity_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(
                    "ALPHA2_IMPORT_PARITY",
                    format!("{name}|{policy}|{width}|{height}"),
                )
                .env("ALPHA2_EXPORT_FRAMES_Y4M", &path)
                .env(
                    "ALPHA2_IMPORT_PARITY_OUTPUT",
                    directory.join(format!("{name}.{policy}.mp4")),
                )
                .output()
                .expect("child");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{name} {policy}: {}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            stdout
                .lines()
                .find_map(|line| {
                    line.split_once("ALPHA2-PARITY-UNSUPPORTED: ")
                        .map(|(_, reason)| reason)
                })
                .map_or_else(
                    || Ok(y4m_frames(&std::fs::read(&path).expect("captured frames"))),
                    |reason| Err(reason.to_owned()),
                )
        };
        let imported = match capture("hardware") {
            Ok(frames) => frames,
            Err(reason) => {
                assert!(
                    claimed.is_none(),
                    "{name}: the device claims it and the hardware export refused it: {reason}"
                );
                eprintln!("note: this device does not decode {name} in hardware ({reason})");
                continue;
            }
        };
        let software = capture("software").expect("a software decode");
        assert_eq!(imported.len(), software.len(), "{name}");
        assert!(!imported.is_empty(), "{name}");
        let mut widest = [0_u8; 3];
        let mut off_by_one = [0_u64; 3];
        let mut samples = [0_u64; 3];
        for (index, (imported, software)) in imported.iter().zip(&software).enumerate() {
            for (plane, (imported, software)) in imported.iter().zip(software).enumerate() {
                assert_eq!(
                    imported.len(),
                    software.len(),
                    "{name} frame {index} plane {plane}"
                );
                let worst = imported
                    .iter()
                    .zip(software)
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap_or(0);
                // A surface sampled against the wrong extent is off by a hundred.
                assert!(
                    worst <= 1,
                    "{name} frame {index} plane {plane}: off by up to {worst} codes"
                );
                widest[plane] = widest[plane].max(worst);
                off_by_one[plane] += u64::try_from(
                    imported
                        .iter()
                        .zip(software)
                        .filter(|(a, b)| a != b)
                        .count(),
                )
                .expect("count");
                samples[plane] += u64::try_from(imported.len()).expect("samples");
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let shares =
            [0, 1, 2].map(|plane| 100.0 * off_by_one[plane] as f64 / samples[plane] as f64);
        eprintln!(
            "{name}: imported and software renders differ by at most {widest:?} codes (Y, U, V), \
             {shares:.3?} % of samples one code off"
        );
        checked += 1;
    }
    if checked == 0 {
        alpha2_test_support::skip!(
            if cfg!(target_os = "linux") {
                alpha2_test_support::Capability::VulkanVideo
            } else if cfg!(target_os = "macos") {
                alpha2_test_support::Capability::Videotoolbox
            } else {
                alpha2_test_support::Capability::D3d12Video
            },
            "no import parity stream decodes in hardware here"
        );
    }
}

/// The frames of a YUV4MPEG2 file of 8-bit 4:2:0 pictures, each as its Y, U
/// and V planes.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn y4m_frames(bytes: &[u8]) -> Vec<[Vec<u8>; 3]> {
    let header_end = bytes
        .iter()
        .position(|&byte| byte == b'\n')
        .expect("header");
    let header = std::str::from_utf8(&bytes[..header_end]).expect("header text");
    let dimension = |prefix: char| {
        header
            .split(' ')
            .find_map(|field| field.strip_prefix(prefix))
            .and_then(|value| value.parse::<usize>().ok())
            .expect("dimension")
    };
    let (width, height) = (dimension('W'), dimension('H'));
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    let sizes = [
        width * height,
        chroma_width * chroma_height,
        chroma_width * chroma_height,
    ];
    let mut frames = Vec::new();
    let mut rest = &bytes[header_end + 1..];
    while let Some(frame) = rest.strip_prefix(b"FRAME\n") {
        let (y, frame) = frame.split_at(sizes[0]);
        let (u, frame) = frame.split_at(sizes[1]);
        let (v, frame) = frame.split_at(sizes[2]);
        frames.push([y.to_vec(), u.to_vec(), v.to_vec()]);
        rest = frame;
    }
    assert!(rest.is_empty(), "a partial frame at the end");
    frames
}

/// The child half of
/// `an_imported_decoder_surface_renders_what_a_software_decode_does`: exports
/// the stream `ALPHA2_IMPORT_PARITY` names under its policy, at the stream's
/// own size, through the readback path whose frames the capture writes. A
/// hardware export the host refuses with a typed hardware decode failure prints
/// `ALPHA2-PARITY-UNSUPPORTED: <reason>` and passes; the parent decides whether
/// the device claimed the stream.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
#[ignore = "run by an_imported_decoder_surface_renders_what_a_software_decode_does"]
fn import_parity_child() {
    let request = std::env::var("ALPHA2_IMPORT_PARITY").expect("a stream and a policy");
    let mut fields = request.split('|');
    let (name, policy, width, height) = (
        fields.next().expect("name"),
        fields.next().expect("policy"),
        fields
            .next()
            .and_then(|w| w.parse::<u32>().ok())
            .expect("width"),
        fields
            .next()
            .and_then(|h| h.parse::<u32>().ok())
            .expect("height"),
    );
    let hardware = policy == "hardware";
    let mut host = create_gpu_media_host(if hardware {
        VideoDecodePolicy::HardwareRequired
    } else {
        VideoDecodePolicy::Software
    })
    .expect("host");
    let half = alpha2_time::TICKS_PER_SECOND / 2;
    let project = json!({
        "schema_version": 1, "id": "import-parity", "name": "Import parity",
        "settings": {"width": width, "height": height, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": half},
            "source_range": {"start": 0, "end": half}
        }]}]}
    });
    let output = PathBuf::from(std::env::var_os("ALPHA2_IMPORT_PARITY_OUTPUT").expect("output"));
    let exported = host.export(
        &project.to_string(),
        &HashMap::from([("video".to_owned(), corpus(name))]),
        &HashMap::new(),
        Vec::new(),
        0,
        half,
        RateRequest { num: 30, den: 1 },
        false,
        crate::protocol::ResolvedFormat {
            container: alpha2_media::Container::Mp4,
            video: NativeVideoCodec::Mpeg4Part2,
            audio: NativeAudioCodec::Aac,
            pixels: alpha2_media::PixelLayout::YUV420P,
        },
        None,
        VideoQualityRequest::Level(QualityLevel::High),
        AudioQualityRequest::Level(QualityLevel::High),
        None,
        &output,
        &mut |_| Ok(()),
    );
    let result = match exported.map_err(HostError::classify) {
        Ok(result) => result,
        // A hardware-required export the source cannot use refuses before decoding a frame.
        Err(error) if hardware && error.message.contains("hardware decode is required") => {
            println!("ALPHA2-PARITY-UNSUPPORTED: {}", error.message);
            return;
        }
        Err(error) => panic!("{name} {policy}: {}: {}", error.code, error.message),
    };
    let performance = &result["performance"];
    if hardware {
        assert_eq!(
            performance["hardwareImportFrames"],
            json!(15),
            "{name}: {result}"
        );
        assert_eq!(
            performance["softwareDecodedFrames"],
            json!(0),
            "{name}: {result}"
        );
    } else {
        assert_eq!(
            performance["softwareDecodedFrames"],
            json!(15),
            "{name}: {result}"
        );
    }
    let log = host.gpu_log.snapshot();
    assert!(
        log.entries.is_empty() && log.dropped == 0,
        "{name} {policy}: {log:?}"
    );
}

#[test]
#[allow(clippy::used_underscore_binding)]
fn every_yuv_layout_uploads_as_planes_and_keeps_its_aspect_ratio() {
    let _gpu = gpu_test_lock();
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_product()).expect("a GPU device");
    // Uploads run with every feature the host's device has; any validation error fails the test.
    let device = gpu.device().clone();
    let renderer = alpha2_render::Renderer::new(gpu);
    let (width, height) = (4_u32, 2_u32);
    for format in [
        alpha2_media::YuvFormat::planar(0, 0, 8),
        alpha2_media::YuvFormat::planar(1, 0, 10),
        alpha2_media::YuvFormat::P010,
    ] {
        let (chroma_width, chroma_height) = format.chroma_size(width, height);
        // Mid-grey codes: every byte 0x80 is valid at any depth or alignment.
        let plane = |positions: u32, samples: u32, rows: u32| {
            let stride = positions * samples * format.bytes_per_sample();
            alpha2_media::Plane {
                bytes: vec![0x80; (stride * rows) as usize].into(),
                stride,
                width: positions,
                height: rows,
            }
        };
        let planes = if format.interleaved_chroma {
            vec![
                plane(width, 1, height),
                plane(chroma_width, 2, chroma_height),
            ]
        } else {
            vec![
                plane(width, 1, height),
                plane(chroma_width, 1, chroma_height),
                plane(chroma_width, 1, chroma_height),
            ]
        };
        let frame = DecodedVideoFrame {
            pts: MediaTime::ZERO,
            dts: None,
            duration: None,
            visible_rect: alpha2_media::Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            sample_aspect_ratio: alpha2_media::SampleAspectRatio { num: 2, den: 1 },
            orientation: alpha2_media::Orientation::UPRIGHT,
            pixel_format: alpha2_media::PixelFormat::Yuv(format),
            chroma_location: alpha2_media::ChromaLocation::Left,
            color: None,
            planes,
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let upload = crate::frames::upload_frame(
            &renderer,
            RenderSize::new(8, 8).expect("size"),
            alpha2_render::ColorPolicy::RejectHdr,
            &frame,
        )
        .expect("uploads");
        assert!(upload.direct_yuv, "{format:?} uploads as planes");
        let pending = upload
            .texture
            .pending
            .as_ref()
            .expect("presentation pending");
        assert!(renderer.submit_pending_source(pending));
        let pixels = renderer
            .read_rgba8(&upload.texture._texture)
            .expect("reads the presented texture");
        if let Some(error) = pollster::block_on(scope.pop()) {
            panic!("{format:?}: {error}");
        }
        // Shown 8 by 2, centred in the 8x8 output: rows 3 and 4 are covered edge to edge, and the
        // rest is empty. Square pixels would have covered rows 2 to 5.
        let alpha = |x: usize, y: usize| pixels[(y * 8 + x) * 4 + 3];
        for y in 0..8 {
            let covered = (3..5).contains(&y);
            for x in [0, 4, 7] {
                assert_eq!(
                    alpha(x, y),
                    if covered { 255 } else { 0 },
                    "{format:?} ({x}, {y})"
                );
            }
        }
    }
}

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdecode"))]
#[test]
#[allow(clippy::used_underscore_binding, clippy::too_many_lines)]
fn a_surface_that_cannot_be_imported_reaches_the_frame_through_a_download() {
    use crate::frames::{FrameInstallMetrics, NativeGpuFrames};

    let _gpu = gpu_test_lock();
    #[cfg(target_os = "linux")]
    let Some(context) = vulkan_video_context() else {
        return;
    };
    #[cfg(target_os = "windows")]
    let Some(context) = create_gpu_media_host(VideoDecodePolicy::Auto)
        .expect("host")
        .decode_d3d12()
    else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::D3d12Video,
            "this host has no D3D12VA decode"
        );
        return;
    };
    #[cfg(target_os = "linux")]
    let opened = alpha2_native::NativeExportDecoder::open_vulkan(
        corpus("h264-long-gop.mp4"),
        1,
        &context,
        1,
    );
    #[cfg(target_os = "windows")]
    let opened =
        alpha2_native::NativeExportDecoder::open_d3d12(corpus("h264-long-gop.mp4"), 1, &context, 1);
    let mut decoder = opened.expect("8-bit H.264 opens in hardware");
    let frame = decoder
        .decode_video_next(&CancelToken::default())
        .expect("decodes")
        .expect("a frame");
    assert!(frame.is_hardware());
    let device = context.gpu().device().clone();
    let renderer = alpha2_render::Renderer::new(context.gpu());
    let size = RenderSize::new(128, 96).expect("size");
    let presented = |upload: &crate::frames::NativeUpload| {
        assert!(
            renderer.submit_pending_source(
                upload
                    .texture
                    .pending
                    .as_ref()
                    .expect("presentation pending")
            )
        );
        renderer
            .read_rgba8(&upload.texture._texture)
            .expect("reads the presented texture")
    };

    // Without the shared context the import (on D3D12, the copy) fails, the way a failed per-frame
    // allocation would; the frame must still arrive, from the CPU.
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let frames = NativeGpuFrames::new(size);
    let mut metrics = FrameInstallMetrics::default();
    let (upload, imported) = frames
        .upload_or_download(&renderer, &frame, &mut metrics)
        .expect("the download fallback uploads the frame");
    assert!(!imported);
    assert!(
        upload.direct_yuv,
        "the downloaded surface uploads as planes"
    );
    assert_eq!(metrics.import_fallbacks, 1);
    // The download is counted: one 128x96 NV12 picture.
    assert_eq!(metrics.source_download_bytes, 128 * 96 * 3 / 2);
    let downloaded = presented(&upload);
    // What arrives is what the software decoder's frame presents as: H.264 decodes bit-exactly, and
    // both reach the GPU as planes.
    let mut software =
        alpha2_native::NativeExportDecoder::open_with_threads(corpus("h264-long-gop.mp4"), 1)
            .expect("software decoder");
    let cpu = software
        .decode_video_next(&CancelToken::default())
        .expect("decodes")
        .expect("a frame");
    assert!(!cpu.is_hardware());
    let (upload, _) = frames
        .upload_or_download(&renderer, &cpu, &mut metrics)
        .expect("the software frame uploads");
    let expected = presented(&upload);
    if let Some(error) = pollster::block_on(scope.pop()) {
        panic!("{error}");
    }
    assert_eq!(downloaded.len(), expected.len());
    let worst = downloaded
        .iter()
        .zip(&expected)
        .map(|(a, b)| a.abs_diff(*b))
        .max();
    assert!(
        worst <= Some(1),
        "the downloaded frame differs by up to {worst:?}"
    );
    assert!(
        expected.chunks(4).any(|pixel| pixel[..3] != expected[..3]),
        "the frame has content to compare"
    );

    let mut strict = NativeGpuFrames::new(size);
    strict.import_fallback = false;
    let Err(error) =
        strict.upload_or_download(&renderer, &frame, &mut FrameInstallMetrics::default())
    else {
        panic!("no fallback when hardware is required");
    };
    #[cfg(target_os = "linux")]
    let unshared = "no shared Vulkan context";
    #[cfg(target_os = "windows")]
    let unshared = "no shared D3D12 context";
    assert!(error.contains(unshared), "{error}");
}

/// A device fault is not an import failure: a download would wait on the
/// same device, so only the frame's own failures fall back.
#[test]
fn only_a_frame_s_own_failure_falls_back_to_a_download() {
    use crate::frames::SurfaceError;
    let timeout = format!(
        "{}the GPU did not finish in time",
        protocol::gpu_timeout_tag()
    );
    let lost = protocol::code_tag("GPU_DEVICE_LOST", "the device was lost");
    for message in [timeout, lost] {
        assert!(
            matches!(SurfaceError::from(message.clone()), SurfaceError::Device(_)),
            "{message}"
        );
    }
    let import = protocol::code_tag("ALPHA2_ERROR", "vkAllocateMemory failed");
    assert!(matches!(
        SurfaceError::from(import),
        SurfaceError::Import(_)
    ));
}

/// A host that decodes H.264 into surfaces its renderer reads in place and
/// encodes on the same device, or `None` (a recorded skip) where there is none.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode",
    feature = "hwdecode"
))]
fn surface_importing_gpu_output_host() -> Option<Host> {
    let (host, _) = gpu_output_host(VideoDecodePolicy::Auto)?;
    if imports_decoded(&host, "h264") == Some(true) {
        Some(host)
    } else {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::HardwareDecodeImport,
            "this host does not import decoded H.264 surfaces"
        );
        None
    }
}

/// Exports half a second of the 256x144 H.264 transcode fixture at
/// `width`x`height`, the next GPU frame supply made to fail as `fault` says
/// if any: at the source's size the picture is transcoded, at any other it is
/// rendered.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode",
    feature = "hwdecode"
))]
fn export_with_a_surface_fault(
    host: &mut Host,
    fault: Option<crate::frames::InjectedSurfaceFault>,
    (width, height): (u32, u32),
    output: &Path,
) -> Result<Value, HostError> {
    let end = alpha2_time::TICKS_PER_SECOND / 2;
    let project = json!({
        "schema_version": 1, "id": "surface-fault", "name": "Surface fault",
        "settings": {"width": width, "height": height, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let request = json!({
        "protocolVersion": 1, "requestId": "surface-fault", "operation": "export",
        "project": project,
        "assets": {"video": corpus("h264-bt709-transcode.mp4")},
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "videoCodec": "h264",
        "inputPolicy": "gpu-required",
        "output": output
    });
    crate::frames::INJECT_SURFACE_FAULT.set(fault);
    let result = host.handle(
        parse_request(&request.to_string()).expect("export request"),
        &mut |_| Ok(()),
    );
    crate::frames::INJECT_SURFACE_FAULT.set(None);
    result
}

/// A decoder surface budget that cannot hold one frame is a configuration
/// error, and a reclaim that timed out is the device's: neither is an import
/// failure, so neither is downloaded around and counted as `importFailed`,
/// on the transcode path or the render path. Each fails in the
/// step it came from: the transcode's repack is its render, the render
/// path's import its upload.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode",
    feature = "hwdecode"
))]
#[test]
fn a_surface_budget_that_holds_no_frame_fails_the_export_instead_of_downloading() {
    let _gpu = gpu_test_lock();
    let Some(mut host) = surface_importing_gpu_output_host() else {
        return;
    };
    let directory = scratch_directory("surface-budget");
    for (path, size, phase) in [
        ("transcode", (256, 144), "render"),
        ("render", (512, 288), "upload"),
    ] {
        let error = export_with_a_surface_fault(
            &mut host,
            Some(crate::frames::InjectedSurfaceFault::EmptyBudget),
            size,
            &directory.join(format!("{path}.mp4")),
        )
        .map(|result| result["performance"]["hardwareFallbackReasons"].clone())
        .expect_err("a budget that holds no surface fails the frame");
        assert!(
            error
                .message
                .contains("the decoder surface budget cannot hold this frame"),
            "{path}: {}",
            error.message
        );
        let failure = error.failure.expect("an export failure report");
        assert_eq!(failure["phase"], phase, "{path}: {failure}");
        assert_eq!(failure["frameIndex"], 0, "{path}: {failure}");
    }
    let _ = std::fs::remove_dir_all(directory);
}

/// A decoder surface that cannot be imported reaches the encoder through a
/// download, whichever backend decoded it: installed for the graph on the
/// render path, and repacked from the CPU on the transcode path. The
/// frames are the ones an import would have given.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode",
    feature = "hwdecode"
))]
#[test]
fn a_surface_that_cannot_be_imported_is_downloaded_on_both_gpu_paths() {
    let _gpu = gpu_test_lock();
    let Some(mut host) = surface_importing_gpu_output_host() else {
        return;
    };
    let directory = scratch_directory("surface-import-fallback");
    for (path, size, transcode) in [
        ("transcode", (256, 144), json!("yuv-repack")),
        ("render", (512, 288), Value::Null),
    ] {
        let imported = directory.join(format!("{path}-imported.mp4"));
        let downloaded = directory.join(format!("{path}-downloaded.mp4"));
        let reference = export_with_a_surface_fault(&mut host, None, size, &imported)
            .expect("an imported export");
        assert_eq!(
            reference["performance"]["hardwareImportFrames"],
            json!(15),
            "{path}: {reference}"
        );
        let result = export_with_a_surface_fault(
            &mut host,
            Some(crate::frames::InjectedSurfaceFault::Import),
            size,
            &downloaded,
        )
        .expect("the download fallback exports every frame");
        assert_eq!(result["videoTranscode"], transcode, "{path}: {result}");
        let performance = &result["performance"];
        assert_eq!(
            performance["hardwareFallbackReasons"]["importFailed"],
            json!(15),
            "{path}: {performance}"
        );
        assert_eq!(
            performance["hardwareImportFrames"],
            json!(0),
            "{path}: {performance}"
        );
        assert!(
            performance["sourceUploadBytes"].as_u64() > Some(0),
            "{path}: {performance}"
        );
        // Each of the 15 surfaces was downloaded, a 256x144 NV12 picture each, whatever the
        // output's size.
        assert_eq!(
            performance["sourceDownloadBytes"],
            json!(15 * 256 * 144 * 3 / 2),
            "{path}: {performance}"
        );
        assert_eq!(result["frames"], json!(15), "{path}: {result}");
        for (index, (downloaded, imported)) in decoded_luma(&downloaded)
            .iter()
            .zip(&decoded_luma(&imported))
            .enumerate()
        {
            let difference = downloaded
                .iter()
                .zip(imported)
                .map(|(a, b)| u64::from(a.abs_diff(*b)))
                .sum::<u64>();
            let samples = u64::try_from(downloaded.len()).expect("sample count");
            assert!(
                difference < samples,
                "{path} frame {index} differs by {difference} over {samples} samples"
            );
        }
    }
    let _ = std::fs::remove_dir_all(directory);
}

/// A hardware decode session opens with the fixed worker count rather than a
/// share of the codec budget, and the report says which rule sized it.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode",
    feature = "hwdecode"
))]
#[test]
fn a_hardware_decode_session_opens_with_the_fixed_worker_count() {
    let _gpu = gpu_test_lock();
    let Some(mut host) = surface_importing_gpu_output_host() else {
        return;
    };
    let directory = scratch_directory("hardware-decoder-threads");
    let result =
        export_with_a_surface_fault(&mut host, None, (512, 288), &directory.join("render.mp4"))
            .expect("a hardware decoded export");
    let performance = &result["performance"];
    assert_eq!(performance["hardwareDecodedFrames"], json!(15), "{result}");
    assert_eq!(result["decoderThreadPolicy"], json!("hardware-fixed"));
    let budget = performance["codecThreadBudget"]
        .as_u64()
        .expect("a codec budget");
    let budget = usize::try_from(budget).expect("budget fits");
    assert_eq!(
        performance["hardwareDecoderThreads"],
        json!(alpha2_native::HARDWARE_DECODER_THREADS.min(budget).max(1)),
        "{performance}"
    );
    let _ = std::fs::remove_dir_all(directory);
}

/// Run by [`the_synchronous_comparison_path_reports_and_refuses_as_a_software_decode`]
/// in a child process whose `ALPHA2_EXPORT_PIPELINE` is off.
#[test]
#[ignore = "run by the_synchronous_comparison_path_reports_and_refuses_as_a_software_decode with ALPHA2_EXPORT_PIPELINE=off"]
fn a_synchronous_export_decodes_in_software() {
    assert!(!crate::config::configured_pipeline_enabled());
    let directory = scratch_directory("pipeline-off-decode");
    let request = |name: &str| {
        let mut request = json!({
            "protocolVersion": 1, "requestId": name, "operation": "export",
            "project": conform_project(&json!({})),
            "assets": {"video": corpus("h264-bt709-copy.mp4")},
            "startTicks": 0, "endTicks": 120_000,
            "frameRate": {"num": 12, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": directory.join(name)
        });
        request["project"]["settings"]["width"] = json!(128);
        request["project"]["settings"]["height"] = json!(96);
        parse_request(&request.to_string()).expect("request")
    };
    // The comparison path opens software decoders whatever the host may do, and says so.
    for policy in [VideoDecodePolicy::Software, VideoDecodePolicy::Auto] {
        let mut host = create_gpu_media_host(policy).expect("host");
        let result = host
            .handle(
                request(&format!("{}.mp4", policy.as_str())),
                &mut |_| Ok(()),
            )
            .expect("synchronous export");
        assert_eq!(result["hardwareDecodeReady"], json!(false), "{result}");
        assert_eq!(
            result["decoderThreadPolicy"],
            json!("source-area"),
            "{result}"
        );
        let performance = &result["performance"];
        assert!(
            performance["softwareDecodedFrames"]
                .as_u64()
                .is_some_and(|frames| frames >= 12),
            "{performance}"
        );
        assert_eq!(performance["hardwareDecodedFrames"], json!(0));
    }
    // `hardware-required` cannot be kept by it, so it is refused before any output opens.
    match create_gpu_media_host(VideoDecodePolicy::HardwareRequired) {
        Ok(mut host) => {
            let error = host
                .handle(request("strict.mp4"), &mut |_| Ok(()))
                .expect_err("hardware-required on the synchronous path");
            assert_eq!(
                error.code, "HARDWARE_DECODE_UNAVAILABLE",
                "{}",
                error.message
            );
            assert!(
                error.message.contains("ALPHA2_EXPORT_PIPELINE"),
                "{}",
                error.message
            );
            assert!(!directory.join("strict.mp4").exists());
            println!("checked: hardware-required refused");
        }
        Err(error) => alpha2_test_support::skip!(
            alpha2_test_support::Capability::HardwareDecodeImport,
            "the hardware-required export was refused: {}",
            error.message
        ),
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// `ALPHA2_EXPORT_PIPELINE=off` runs the synchronous comparison path, whose
/// frames come from software decoders. It counts them and reports software
/// decode, so `hardware-required` cannot pass a run that decodes every frame on
/// the CPU. The switch is read from the environment, so the exports run in a
/// child of this test binary.
#[test]
fn the_synchronous_comparison_path_reports_and_refuses_as_a_software_decode() {
    let _gpu = gpu_test_lock();
    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env("ALPHA2_EXPORT_PIPELINE", "off")
        .args([
            "--exact",
            "tests::hardware_decode::a_synchronous_export_decodes_in_software",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    let stdout = String::from_utf8_lossy(&child.stdout);
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        child.status.success() && stdout.contains("1 passed"),
        "{}\n{stdout}\n{stderr}",
        child.status
    );
}

/// A build without hardware decode cannot keep `hardware-required`, so it
/// refuses to start, before making a GPU, rather than decoding every frame in
/// software and reporting the mode it was asked for.
#[test]
fn a_build_without_hardware_decode_refuses_hardware_required_at_startup() {
    let _gpu = gpu_test_lock();
    let built = cfg!(all(
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        ),
        feature = "hwdecode"
    ));
    match create_gpu_media_host(VideoDecodePolicy::HardwareRequired) {
        Err(error) if !built => {
            assert_eq!(
                error.code, "HARDWARE_DECODE_UNAVAILABLE",
                "{}",
                error.message
            );
            assert!(
                error.message.contains("without hardware decode"),
                "{}",
                error.message
            );
        }
        Ok(host) if !built => panic!(
            "a build without hardware decode started in {} mode",
            host.decode_policy.as_str()
        ),
        _ => {}
    }
}
