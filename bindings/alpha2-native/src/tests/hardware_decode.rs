//! Hardware decode sessions: failures, downloads, aspect, crop, and the decode report.

use super::*;

/// The device [`hardware_decode_context`] makes on this platform.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
type HardwareDecodeContext = alpha2_native_gpu::SharedVulkanContext;

#[cfg(all(target_os = "macos", feature = "hwdecode"))]
type HardwareDecodeContext = alpha2_native_gpu::SharedMetalContext;

#[cfg(all(target_os = "windows", feature = "hwdecode"))]
type HardwareDecodeContext = alpha2_native_gpu::SharedD3D12Context;

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn a_hardware_session_that_fails_before_its_first_frame_is_reported_as_unsupported() {
    // Lazy hardware setup in `get_format` can report an unsupported stream as a generic codec
    // error; callers must still be able to fall back.
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let cancel = CancelToken::default();
    // Apple silicon decodes the first two in hardware. FFmpeg offers VideoToolbox for MPEG-4 Part
    // 2, which no Mac decodes in hardware, so `get_format` fails and answers NONE.
    let mut failed_lazily = Vec::new();
    for name in ["h264-high10.mp4", "vp9.webm", "recovery-mpeg4.mp4"] {
        let mut decoder = match open_hardware_decoder(corpus(name), &context) {
            Ok(decoder) => decoder,
            Err(error) => {
                eprintln!("{name}: the hardware session refused to open ({error:?})");
                continue;
            }
        };
        match decoder.decode_video_next(&cancel) {
            Ok(Some(frame)) => {
                assert!(frame.is_hardware());
                eprintln!("skipped {name}: this device decodes it");
            }
            Err(DecodeError::Unsupported(message)) => {
                assert!(
                    message.contains("before its first frame"),
                    "{name}: {message}"
                );
                assert_eq!(
                    decoder.hardware_failure(),
                    Some(HardwareDecodeFallback::DecoderOpenFailed),
                    "{name}"
                );
                failed_lazily.push(name);
            }
            other => panic!("{name}: expected a typed hardware failure, got {other:?}"),
        }
    }
    #[cfg(target_os = "macos")]
    assert_eq!(failed_lazily, ["recovery-mpeg4.mp4"]);
    let mut decoder = open_hardware_decoder(corpus("h264-long-gop.mp4"), &context)
        .expect("8-bit H.264 opens in hardware");
    assert!(
        decoder
            .decode_video_next(&cancel)
            .expect("decodes")
            .is_some_and(|frame| frame.is_hardware())
    );
    assert_eq!(decoder.hardware_failure(), None);
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn a_hardware_frame_downloads_to_the_pixels_software_decodes() {
    // The fallback for a surface the renderer cannot import. H.264 is bit-exact, so the downloaded
    // NV12 visible picture must equal the software frame; the cropped streams sit 8 columns in and
    // 6 rows short of a 96-row surface.
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let cancel = CancelToken::default();
    for name in [
        "h264-long-gop.mp4",
        "h264-bt709-crop-left.mp4",
        "h264-bt709-crop-bottom.mp4",
    ] {
        let Some(mut hardware) = open_hardware_unless_leading_crop(name, &context) else {
            continue;
        };
        let mut software =
            NativeExportDecoder::open_with_threads(corpus(name), 1).expect("software decoder");
        for _ in 0..3 {
            let surface = hardware
                .decode_video_next(&cancel)
                .expect("hardware decodes")
                .expect("hardware frame");
            let cpu = software
                .decode_video_next(&cancel)
                .expect("software decodes")
                .expect("software frame");
            assert!(surface.is_hardware());
            let downloaded = surface.to_cpu().expect("surface downloads");
            let expected = cpu.as_cpu().expect("software frame is on the CPU");
            assert_eq!(downloaded.pts, expected.pts, "{name}");
            let (shown, wanted) = (surface.metadata().visible_rect, expected.visible_rect);
            assert_eq!(shown, downloaded.visible_rect, "{name}");
            assert_eq!(
                (shown.width, shown.height),
                (wanted.width, wanted.height),
                "{name}"
            );
            // The download keeps the surface's interleaved chroma, exactly the software U and V
            // samples.
            assert_eq!(downloaded.pixel_format, PixelFormat::Yuv(YuvFormat::NV12));
            assert_eq!(expected.pixel_format, PixelFormat::Yuv(YuvFormat::I420));
            assert_eq!(
                region_samples(&downloaded.planes[0], shown, 0, 1),
                region_samples(&expected.planes[0], wanted, 0, 1),
                "{name}: luma"
            );
            let (u, v): (Vec<u8>, Vec<u8>) = region_samples(&downloaded.planes[1], shown, 1, 2)
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .unzip();
            assert_eq!(
                u,
                region_samples(&expected.planes[1], wanted, 1, 1),
                "{name}: U"
            );
            assert_eq!(
                v,
                region_samples(&expected.planes[2], wanted, 1, 1),
                "{name}: V"
            );
        }
    }
}

/// What the bridge reads from a frame `VideoToolbox` decoded: a pixel buffer of
/// its own, the size of the picture the software decoder gives (these streams
/// have no crop), and the layout the frames context allocated at the stream's
/// depth. The shim's own test reads only a stand-in frame.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
#[test]
fn a_videotoolbox_frame_describes_the_surface_it_holds() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let cancel = CancelToken::default();
    let mut streams = vec![("h264-long-gop.mp4", ffi::AV_PIX_FMT_NV12)];
    if context.supports_ten_bit() {
        streams.push(("h264-high10.mp4", ffi::AV_PIX_FMT_P010LE));
    }
    for (name, software_format) in streams {
        let mut hardware = open_hardware_decoder(corpus(name), &context)
            .unwrap_or_else(|error| panic!("{name} opens in hardware: {error:?}"));
        let mut software =
            NativeExportDecoder::open_with_threads(corpus(name), 1).expect("software decoder");
        for _ in 0..3 {
            let frame = hardware
                .decode_video_next(&cancel)
                .expect("hardware decodes")
                .expect("hardware frame");
            let cpu = software
                .decode_video_next(&cancel)
                .expect("software decodes")
                .expect("software frame");
            let NativeVideoFrame::Hardware { metadata, surface } = &frame else {
                panic!("{name}: a software frame from the hardware session");
            };
            let info = surface
                .as_any()
                .downcast_ref::<alpha2_ffmpeg_bridge::VideoToolboxFrameLease>()
                .expect("a VideoToolbox lease")
                .info()
                .expect("a VideoToolbox frame");
            let picture = cpu.as_cpu().expect("software frame").visible_rect;
            assert_ne!(info.pixel_buffer, 0, "{name}");
            assert_eq!(
                (info.width, info.height),
                (
                    i32::try_from(picture.width).expect("width"),
                    i32::try_from(picture.height).expect("height")
                ),
                "{name}"
            );
            assert_eq!(info.software_format, software_format, "{name}");
            assert_eq!(
                Some(metadata.plane_layout),
                crate::video_frame::surface_plane_layout(software_format),
                "{name}"
            );
            assert_eq!(metadata.visible_rect, picture, "{name}");
        }
    }
}

/// Opens an 8-bit H.264 corpus stream in hardware, or `None` for the one that
/// crops its left edge on `VideoToolbox`, which must refuse it with
/// [`HardwareDecodeFallback::UnsupportedCrop`]: its surface would hold the
/// coded picture's first columns, the cropped ones included. D3D12VA, whose
/// surfaces are the coded size, shows it as software does.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn open_hardware_unless_leading_crop(
    name: &str,
    context: &HardwareDecodeContext,
) -> Option<NativeExportDecoder> {
    let leading = name == "h264-bt709-crop-left.mp4" && cfg!(target_os = "macos");
    match open_hardware_decoder(corpus(name), context) {
        Err(error) if leading => {
            assert_eq!(
                error.reason,
                HardwareDecodeFallback::UnsupportedCrop,
                "{name}: {error:?}"
            );
            None
        }
        Ok(_) if leading => {
            panic!("{name}: the hardware session opened a stream it shows cropped wrongly")
        }
        Ok(decoder) => Some(decoder),
        Err(error) => panic!("{name}: 8-bit H.264 opens in hardware: {error:?}"),
    }
}

/// The samples of `plane` under `rect`, its chroma subsampled by `shift`,
/// `per_position` bytes at each sample position.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn region_samples(
    plane: &Plane,
    rect: alpha2_media::Rect,
    shift: u32,
    per_position: usize,
) -> Vec<u8> {
    let (x, y) = ((rect.x >> shift) as usize, (rect.y >> shift) as usize);
    let width = rect.width.div_ceil(1 << shift) as usize;
    let height = rect.height.div_ceil(1 << shift) as usize;
    plane
        .bytes
        .chunks(plane.stride as usize)
        .skip(y)
        .take(height)
        .flat_map(|row| &row[x * per_position..(x + width) * per_position])
        .copied()
        .collect()
}

/// A hardware session reads the pixel aspect ratio as software does: the
/// container's declaration, otherwise each picture's own.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn a_hardware_frame_keeps_the_pixel_aspect_ratio_software_reads() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    // The stream whose SPS changes mid-stream has a test of its own, below.
    let wrong: Vec<String> = pixel_aspect_fixtures()
        .into_iter()
        .filter(|(name, ..)| *name != SAR_CHANGE)
        .flat_map(|(name, _, early, late)| hardware_aspect_mismatches(name, early, late, &context))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The fixture whose SPS changes its pixel aspect ratio after the first GOP.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
const SAR_CHANGE: &str = "h264-bt709-sar-change.ts";

/// A hardware session decodes through an SPS that changes mid-stream, which
/// reinitialises it, and each picture keeps its own ratio.
///
/// Stock `FFmpeg` 8.0.3 and 8.1.3 release an H.264 picture's frame before its
/// hardware data (`ff_h264_unref_picture`). When the SPS change has replaced
/// the frames context, that frame is the last of the old pool and is destroyed
/// with its semaphore, and Vulkan's data then waits on the destroyed semaphore:
/// the process dies with SIGSEGV inside the driver. The pinned SDK carries the
/// upstream fix (`FFmpeg` commit 0661ef6bb3e5), and this test fails on a
/// runtime without it.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn a_hardware_session_decodes_through_an_sps_change() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let (name, _, early, late) = pixel_aspect_fixtures()
        .into_iter()
        .find(|(name, ..)| *name == SAR_CHANGE)
        .expect("the SPS change fixture");
    let wrong = hardware_aspect_mismatches(name, early, late, &context);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The pictures of `name`, decoded in hardware, that read a ratio other than
/// `early` (the first 30) or `late` (the rest): the first of each such run.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
fn hardware_aspect_mismatches(
    name: &str,
    early: SampleAspectRatio,
    late: SampleAspectRatio,
    context: &HardwareDecodeContext,
) -> Vec<String> {
    let cancel = CancelToken::default();
    let mut wrong = Vec::new();
    let mut decoder = open_hardware_decoder(corpus(name), context)
        .unwrap_or_else(|error| panic!("{name}: 8-bit H.264 opens in hardware: {error:?}"));
    let mut index = 0;
    let mut wrong_run = None;
    while let Some(frame) = decoder.decode_video_next(&cancel).expect(name) {
        assert!(frame.is_hardware(), "{name}");
        let expected = if index < 30 { early } else { late };
        let found = frame.metadata().sample_aspect_ratio;
        if found != expected && (index == 0 || index == 30 || wrong_run != Some(index - 1)) {
            wrong.push(format!(
                "{name} from frame {index}: {found:?}, expected {expected:?}"
            ));
        }
        wrong_run = (found != expected).then_some(index);
        index += 1;
    }
    assert_eq!(index, 60, "{name}");
    wrong
}

/// A hardware frame shows the picture a software decode of the same stream
/// does: the columns its SPS crops away on the left, and its chroma where an
/// SPS without VUI puts it.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn a_hardware_frame_shows_the_crop_and_siting_a_software_decode_does() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let cancel = CancelToken::default();
    for (name, siting) in [
        ("h264-bt709-crop-left.mp4", ChromaLocation::Left),
        ("h264-bt709-crop-bottom.mp4", ChromaLocation::Left),
        ("h264-bt709-no-vui.mp4", ChromaLocation::Left),
    ] {
        let Some(mut hardware) = open_hardware_unless_leading_crop(name, &context) else {
            continue;
        };
        let mut software = NativeDecoder::open(corpus(name)).expect(name);
        let frame = hardware
            .decode_video_next(&cancel)
            .expect(name)
            .expect("frame");
        assert!(frame.is_hardware(), "{name}");
        let shown = first_frame(&mut software, name);
        let rect = frame.metadata().visible_rect;
        assert_eq!(
            (rect.width, rect.height),
            (shown.visible_rect.width, shown.visible_rect.height),
            "{name}: {rect:?}"
        );
        assert_eq!(frame.metadata().chroma_location, siting, "{name}");
        assert_eq!(shown.chroma_location, siting, "{name}");
    }
}

#[test]
fn a_native_gpu_fault_stays_typed_through_the_adapter_errors() {
    // A helper's VK_ERROR_DEVICE_LOST stays typed, not "encoder adapter failed: FFmpeg error code
    // -1404", a string nothing can classify.
    use alpha2_ffmpeg_bridge::{BridgeError, GpuFault, NativeGpuBackend, NativeGpuFailure};
    let lost = NativeGpuFailure {
        backend: NativeGpuBackend::Vulkan,
        operation: "encode copy",
        step: "queue submit",
        status: -4,
        fault: Some(GpuFault::DeviceLost),
    };
    let encode = crate::convert::encode_adapter_error(BridgeError::NativeGpu(lost));
    assert_eq!(
        encode,
        alpha2_media::EncodeError::Gpu {
            fault: GpuFault::DeviceLost,
            message: "Vulkan encode copy failed at queue submit (VkResult -4)".into(),
        }
    );
    assert_eq!(
        encode.to_string(),
        "encoder adapter failed: Vulkan encode copy failed at queue submit (VkResult -4)"
    );
    let at = crate::convert::encode_adapter_error_at("closing", BridgeError::NativeGpu(lost));
    assert!(matches!(
        at,
        alpha2_media::EncodeError::Gpu {
            fault: GpuFault::DeviceLost,
            ..
        }
    ));
    let probe = crate::convert::adapter_error(BridgeError::NativeGpu(NativeGpuFailure {
        backend: NativeGpuBackend::Vulkan,
        operation: "decode probe",
        step: "fence wait",
        status: 2,
        fault: Some(GpuFault::Timeout),
    }));
    assert!(matches!(
        probe,
        alpha2_media::DecodeError::Gpu {
            fault: GpuFault::Timeout,
            ..
        }
    ));
    assert!(probe.is_fatal());
    let unknown = crate::convert::encode_adapter_error(BridgeError::NativeGpu(NativeGpuFailure {
        backend: NativeGpuBackend::Vulkan,
        operation: "encode copy",
        step: "queue submit",
        status: -3,
        fault: None,
    }));
    assert!(matches!(unknown, alpha2_media::EncodeError::Adapter(_)));
    assert!(matches!(
        crate::convert::encode_adapter_error(BridgeError::FfmpegCode(-28)),
        alpha2_media::EncodeError::Adapter(_)
    ));
}

/// What `capabilities.decode` tells a client: this build's backend, and per
/// codec the decoder `FFmpeg` picks, its software decoder, and whether a
/// registered decoder has this backend's hardware configuration.
/// The pinned runtime enables every platform's backend, so H.264 and HEVC
/// have it wherever the build decodes in hardware.
#[test]
fn the_decode_report_names_this_builds_backend_and_the_runtimes_hardware_decoders() {
    let report = hardware_decode_capabilities();
    let backend = if !cfg!(feature = "hwdecode") {
        "none"
    } else if cfg!(target_os = "linux") {
        "vulkan"
    } else if cfg!(target_os = "macos") {
        "videotoolbox"
    } else if cfg!(target_os = "windows") {
        "d3d12va"
    } else {
        "none"
    };
    assert_eq!(report.backend, backend);
    assert_eq!(report.backend, hardware_decode_backend());
    let rows = report
        .codecs
        .iter()
        .map(|row| {
            (
                row.codec,
                row.decoder_name.as_deref(),
                row.software_decoder.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            (NativeVideoCodec::H264, Some("h264"), Some("h264")),
            (NativeVideoCodec::Hevc, Some("hevc"), Some("hevc")),
            (NativeVideoCodec::Av1, Some("libdav1d"), Some("libdav1d")),
            (NativeVideoCodec::Vp9, Some("vp9"), Some("vp9")),
        ]
    );
    let hardware = report
        .codecs
        .iter()
        .filter(|row| row.hardware_config)
        .map(|row| row.codec)
        .collect::<Vec<_>>();
    if backend == "none" {
        assert_eq!(hardware, []);
    } else if cfg!(target_os = "macos") {
        // The macOS runtime's VideoToolbox has a hardware path for all four.
        assert_eq!(hardware.len(), 4, "{hardware:?}");
    } else {
        assert!(
            hardware.starts_with(&[NativeVideoCodec::H264, NativeVideoCodec::Hevc]),
            "{hardware:?}"
        );
    }
}
