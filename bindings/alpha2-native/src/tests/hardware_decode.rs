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
    // FFmpeg offers VideoToolbox for MPEG-4 Part 2, which no Mac decodes in hardware: it is not in
    // the machine's decode list, so the session refuses to open. Whether a Mac decodes VP9 is
    // VideoToolbox's own answer; whether it decodes H.264 High 10 it cannot say (it answers per
    // codec, not per profile), so either outcome is accepted.
    let mut failed_lazily = Vec::new();
    let mut refused_at_open = Vec::new();
    for name in ["h264-high10.mp4", "vp9.webm", "recovery-mpeg4.mp4"] {
        let mut decoder = match open_hardware_decoder(corpus(name), &context) {
            Ok(decoder) => decoder,
            Err(error) => {
                eprintln!("{name}: the hardware session refused to open ({error:?})");
                refused_at_open.push(name);
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
    {
        use alpha2_native_gpu::{VideoToolboxCodec, videotoolbox_hardware_decodes};
        assert!(
            refused_at_open.contains(&"recovery-mpeg4.mp4"),
            "MPEG-4 Part 2 has no VideoToolbox decoder: {refused_at_open:?}"
        );
        assert_eq!(
            refused_at_open.contains(&"vp9.webm"),
            !videotoolbox_hardware_decodes(VideoToolboxCodec::Vp9),
            "VP9 is refused exactly when VideoToolbox does not decode it: {refused_at_open:?}"
        );
        eprintln!(
            "h264-high10.mp4 {} in hardware on this Mac",
            if failed_lazily.contains(&"h264-high10.mp4") {
                "does not decode"
            } else {
                "decodes"
            }
        );
    }
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

/// A stream whose codec the device does not list is refused before a session
/// opens, with the typed reason a caller decodes it in software for, rather
/// than opening one that fails on its first packet.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[test]
fn a_codec_the_device_does_not_list_opens_in_software_without_a_session() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let spec = crate::video_frame::HardwareDecoderSpec {
        device_type: rsmpeg::ffi::AV_HWDEVICE_TYPE_VULKAN,
        pixel_format: rsmpeg::ffi::AV_PIX_FMT_VULKAN,
        backend: "Vulkan",
        require_device_context: true,
        ten_bit: context.supports_ten_bit(),
        keeps_leading_crop: true,
        hardware_codecs: Some(&["h264"]),
        get_format: alpha2_ffmpeg_bridge::vulkan_get_format_callback(),
    };
    let Err(refused) = crate::decode::NativeDecoder::open_hardware(
        corpus("vp9.webm"),
        1,
        context.ffmpeg_device(),
        &spec,
    ) else {
        panic!("vp9 is not listed, and opened in hardware");
    };
    assert_eq!(refused.reason, HardwareDecodeFallback::NoHardwareConfig);
    assert!(
        format!("{:?}", refused.source).contains("has no hardware decoder for vp9 here"),
        "{refused}"
    );
    // The device's own list decides an export's decoder: VP9 opens in hardware exactly where the
    // device lists it.
    let listed = context.decode_codecs().contains(&"vp9");
    match open_hardware_decoder(corpus("vp9.webm"), &context) {
        Ok(_) => assert!(listed, "vp9 opened in hardware without being listed"),
        Err(error) => {
            assert!(!listed, "vp9 is listed but refused: {error}");
            assert_eq!(error.reason, HardwareDecodeFallback::NoHardwareConfig);
        }
    }
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
    // H.264 High 10 joins only where this Mac decodes it in hardware; a refusal must be typed.
    if context.supports_ten_bit() {
        match open_hardware_decoder(corpus("h264-high10.mp4"), &context)
            .map_err(|error| format!("{error:?}"))
            .and_then(|mut decoder| {
                decoder
                    .decode_video_next(&cancel)
                    .map_err(|error| format!("{error:?}"))
            }) {
            Ok(Some(frame)) if frame.is_hardware() => {
                streams.push(("h264-high10.mp4", ffi::AV_PIX_FMT_P010LE));
            }
            Err(error) if error.contains("Unsupported") || error.contains("Refused") => {
                eprintln!("this Mac decodes H.264 High10 in software ({error})");
            }
            other => panic!(
                "h264-high10.mp4: expected a hardware frame or a typed refusal, got {other:?}"
            ),
        }
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

/// A `VideoToolbox` surface's range is what its pixel buffer's format says (`420v` video range,
/// `420f` full), which is what its planes hold whatever the stream's tag claims.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
#[test]
fn videotoolbox_import_takes_range_from_the_buffer() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let mut decoder = open_hardware_decoder(corpus("h264-long-gop.mp4"), &context)
        .expect("8-bit H.264 opens in hardware");
    let frame = decoder
        .decode_video_next(&CancelToken::default())
        .expect("decodes")
        .expect("a frame");
    let NativeVideoFrame::Hardware { metadata, surface } = frame else {
        panic!("a software frame from the hardware session");
    };
    let lease = std::sync::Arc::clone(&surface)
        .into_any()
        .downcast::<alpha2_ffmpeg_bridge::VideoToolboxFrameLease>()
        .expect("a VideoToolbox lease");
    let imported = context.import_surface(lease).expect("imports");
    assert_eq!(imported.pixel_format, "420v");
    assert!(!imported.full_range);
    // The stream declares no colour, which reads as limited range: the buffer agrees.
    assert_eq!(
        metadata
            .color
            .map_or(alpha2_media::ColorRange::Limited, |color| color.range),
        alpha2_media::ColorRange::Limited
    );
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
    } else {
        // Registration only: what a device decodes is its own list
        // (`the_videotoolbox_decode_list_comes_from_the_device` on macOS).
        assert!(
            hardware.starts_with(&[NativeVideoCodec::H264, NativeVideoCodec::Hevc]),
            "{hardware:?}"
        );
    }
}

/// The codecs a Mac's `VideoToolbox` decodes come from the machine
/// (`VTIsHardwareDecodeSupported`), not from what `FFmpeg` registers: each
/// listed codec opens a hardware session on its corpus stream and decodes a
/// hardware frame, and each unlisted one is refused before a session opens,
/// so an M1 never claims AV1 and a session never runs in `VideoToolbox`'s
/// software decoder.
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
#[test]
fn the_videotoolbox_decode_list_comes_from_the_device() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let cancel = CancelToken::default();
    let listed = context.decode_codecs();
    eprintln!("VideoToolbox decodes {listed:?} in hardware on this Mac");
    assert!(listed.contains(&"h264"), "{listed:?}");
    for (codec, name) in [
        ("h264", "h264-long-gop.mp4"),
        ("hevc", "hevc-hvc1.mp4"),
        ("vp9", "vp9.webm"),
        // 320x180: VideoToolbox's AV1 decoder (an M4's) refuses a 64x48 stream's session though it
        // decodes the codec, which is a per-stream refusal the lazy fallback handles.
        ("av1", "av1-padded.mp4"),
    ] {
        match open_hardware_decoder(corpus(name), &context) {
            Ok(mut decoder) => {
                assert!(listed.contains(&codec), "{name} opened, unlisted");
                let frame = decoder
                    .decode_video_next(&cancel)
                    .unwrap_or_else(|error| panic!("{name}: {error:?}"))
                    .unwrap_or_else(|| panic!("{name}: no frame"));
                assert!(frame.is_hardware(), "{name}");
            }
            Err(error) => {
                assert!(!listed.contains(&codec), "{name} is listed: {error:?}");
                assert_eq!(error.reason, HardwareDecodeFallback::NoHardwareConfig);
                assert!(
                    error
                        .source
                        .to_string()
                        .contains(&format!("VideoToolbox has no hardware decoder for {codec}")),
                    "{name}: {error:?}"
                );
            }
        }
    }
}

/// Importing `VideoToolbox` frames into the renderer through the never-flushed
/// `CoreVideo` texture cache does not keep the decoder's surfaces out of its
/// pool: the same session decoding the stream eight times over cycles through
/// the same few `IOSurface`s with and without imports (IDs, not pixel buffer
/// addresses, which `CoreVideo` may reuse or rewrap).
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
#[test]
fn metal_imports_do_not_grow_the_decoder_surface_set() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let cancel = CancelToken::default();
    let mut counts = Vec::new();
    for import in [false, true] {
        let mut decoder = open_hardware_decoder(corpus("h264-long-gop.mp4"), &context)
            .expect("8-bit H.264 opens in hardware");
        let mut surfaces = std::collections::BTreeSet::new();
        let mut frames = 0_u32;
        for pass in 0..8_u64 {
            let mut first = (pass > 0).then(|| {
                decoder
                    .seek_video(
                        DecodeRequest {
                            source: alpha2_media::SourceId::new("video").expect("source ID"),
                            time: MediaTime::ZERO,
                            kind: alpha2_media::DecodeRequestKind::Seek,
                            generation: pass,
                        },
                        &cancel,
                    )
                    .expect("seek")
            });
            loop {
                let frame = match first.take() {
                    Some(frame) => frame,
                    None => match decoder.decode_video_next(&cancel).expect("decodes") {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                let NativeVideoFrame::Hardware { surface, .. } = frame else {
                    panic!("a software frame from the hardware session");
                };
                let lease = std::sync::Arc::clone(&surface)
                    .into_any()
                    .downcast::<alpha2_ffmpeg_bridge::VideoToolboxFrameLease>()
                    .expect("a VideoToolbox lease");
                surfaces.insert(
                    alpha2_native_gpu::videotoolbox_surface_id(&lease).expect("an IOSurface"),
                );
                if import {
                    drop(context.import_surface(lease).expect("imports"));
                }
                frames += 1;
            }
        }
        eprintln!("imports {import}: {frames} frames, IOSurfaces {surfaces:?}");
        assert_eq!(frames, 240, "eight passes of the 30-frame stream");
        counts.push(surfaces.len());
    }
    assert_eq!(counts[0], counts[1], "{counts:?}");
    assert!(counts[1] <= 8, "{counts:?}");
}
