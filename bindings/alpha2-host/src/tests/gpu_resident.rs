//! GPU-resident exports: hardware sessions, import tickets, transcodes, and D3D12VA decode.

use super::*;

#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdecode"))]
#[test]
fn an_export_survives_a_hardware_session_that_fails_on_its_first_packet() {
    let _gpu = gpu_test_lock();
    #[cfg(target_os = "linux")]
    if vulkan_video_context().is_none() {
        return;
    }
    // No Mac has a VideoToolbox decoder for MPEG-4 Part 2, which FFmpeg still offers it, so the
    // session fails on the first packet.
    #[cfg(target_os = "linux")]
    let streams = [("vp9.webm", 128, 96), ("h264-high10.mp4", 128, 96)];
    #[cfg(target_os = "macos")]
    let streams = {
        if !create_gpu_test_host(VideoDecodePolicy::Auto)
            .expect("host")
            .hardware_decode_ready()
        {
            alpha2_test_support::skip!(
                alpha2_test_support::Capability::Videotoolbox,
                "this host has no VideoToolbox decode"
            );
            return;
        }
        [("recovery-mpeg4.mp4", 64, 48)]
    };
    let directory = scratch_directory("hardware-fallback");
    for (name, width, height) in streams {
        let project = json!({
            "schema_version": 1,
            "id": "hardware-fallback",
            "name": "Hardware fallback",
            "settings": {
                "width": width,
                "height": height,
                "frame_rate": {"num": 30, "den": 1},
                "sample_rate": 48000
            },
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip",
                "source_id": "video",
                "timeline_range": {"start": 0, "end": 12_000},
                "source_range": {"start": 0, "end": 12_000}
            }]}]}
        });
        let assets = HashMap::from([("video".to_owned(), corpus(name))]);
        for policy in [VideoDecodePolicy::Auto, VideoDecodePolicy::HardwareRequired] {
            let mut host = create_gpu_test_host(policy).expect("host");
            let output = directory.join(format!("{name}-{}.mp4", policy.as_str()));
            let exported = host.export(
                &project.to_string(),
                &assets,
                &HashMap::new(),
                Vec::new(),
                0,
                12_000,
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
            if policy == VideoDecodePolicy::Auto {
                let result = exported.unwrap_or_else(|error| panic!("{name}: {error}"));
                let performance = &result["performance"];
                let reasons = &performance["hardwareFallbackReasons"];
                if cfg!(target_os = "macos") {
                    assert_eq!(
                        performance["hardwareDecodedFrames"],
                        json!(0),
                        "{name}: {result}"
                    );
                }
                if performance["hardwareDecodedFrames"].as_u64() == Some(0) {
                    assert_eq!(reasons["decoderOpenFailed"], json!(1), "{name}: {result}");
                }
                assert_eq!(result["frames"], json!(3), "{name}");
            } else {
                if cfg!(target_os = "macos") {
                    assert!(exported.is_err(), "{name}: decoded in hardware");
                }
                if let Err(error) = exported {
                    assert!(error.contains("decoderOpenFailed"), "{name}: {error}");
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[test]
fn imported_frames_reuse_their_vulkan_import_tickets() {
    let _gpu = gpu_test_lock();
    if vulkan_video_context().is_none() {
        return;
    }
    let mut host = create_gpu_test_host(VideoDecodePolicy::Auto).expect("host");
    let directory = scratch_directory("import-tickets");
    let second = alpha2_time::TICKS_PER_SECOND;
    let project = json!({
        "schema_version": 1,
        "id": "import-tickets",
        "name": "Import tickets",
        "settings": {
            "width": 640,
            "height": 360,
            "frame_rate": {"num": 30, "den": 1},
            "sample_rate": 48000
        },
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip",
            "source_id": "video",
            "timeline_range": {"start": 0, "end": second},
            "source_range": {"start": 0, "end": second}
        }]}]}
    });
    let assets = HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]);
    for (codec, name) in [
        (NativeVideoCodec::Mpeg4Part2, "readback.mp4"),
        (NativeVideoCodec::H264, "resident.mp4"),
    ] {
        let result = host
            .export(
                &project.to_string(),
                &assets,
                &HashMap::new(),
                Vec::new(),
                0,
                second,
                RateRequest { num: 30, den: 1 },
                false,
                crate::protocol::ResolvedFormat {
                    container: alpha2_media::Container::Mp4,
                    video: codec,
                    audio: NativeAudioCodec::Aac,
                    pixels: alpha2_media::PixelLayout::YUV420P,
                },
                None,
                VideoQualityRequest::Level(QualityLevel::High),
                AudioQualityRequest::Level(QualityLevel::High),
                None,
                directory.join(name),
                &mut |_| Ok(()),
            )
            .expect("export");
        let tickets = &result["vulkanImportTickets"];
        let (created, reused) = (tickets["created"].as_u64(), tickets["reused"].as_u64());
        let imports = created
            .zip(reused)
            .map(|(created, reused)| created + reused);
        if imports == Some(0) {
            eprintln!("note: {name} imported no frames on this device: {result}");
            continue;
        }
        // Frames in flight bound the tickets alive at once, not frame count.
        assert_eq!(imports, Some(30), "{name}: {tickets}");
        assert!(
            created.is_some_and(|created| created <= 4),
            "{name}: {tickets}"
        );
        assert_eq!(tickets["abandoned"].as_u64(), Some(0), "{name}: {tickets}");
    }
    // Tickets released by the renderer's own submission never submitted a fence, so teardown must
    // not wait on them (30 s timeout each).
    let dropped = Instant::now();
    drop(host);
    assert!(
        dropped.elapsed() < Duration::from_secs(10),
        "tearing the host down took {:?}",
        dropped.elapsed()
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn a_resident_host_exports_again_after_a_gpu_resident_export_fails_midway() {
    let _gpu = gpu_test_lock();
    // `serve` keeps one host, so whatever a failed export leaves on the shared device is what the
    // next export runs into.
    let Some((mut host, capability, _)) = gpu_output_host(VideoDecodePolicy::Auto) else {
        return;
    };
    let directory = scratch_directory("resident-retry");
    let project = |end: i64| {
        json!({
            "schema_version": 1,
            "id": "resident-retry",
            "name": "Resident retry",
            "settings": {
                "width": 640,
                "height": 360,
                "frame_rate": {"num": 30, "den": 1},
                "sample_rate": 48000
            },
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip",
                "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        })
    };
    let assets = HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]);
    let mut export = |end: i64, name: &str, fail_at: Option<u64>| {
        host.export(
            &project(end).to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join(name),
            &mut |progress| match fail_at {
                Some(frame) if progress.completed >= frame => {
                    Err(HostError::new("TEST_FAILURE", "the export stops midway"))
                }
                _ => Ok(()),
            },
        )
    };
    // Progress is reported every 30 frames, so this fails with renders and copies in flight.
    // Whether the encode worker records "cancelled" first depends on timing, so the export runs a
    // few times.
    for attempt in 0..3 {
        let failed = export(
            2 * alpha2_time::TICKS_PER_SECOND,
            &format!("fails-{attempt}.mp4"),
            Some(30),
        );
        let message = failed.expect_err("an export whose progress callback fails");
        assert_eq!(
            crate::failure::strip_phase_tags(message.clone()),
            "the export stops midway",
            "attempt {attempt}: {message}"
        );
    }
    let result =
        export(alpha2_time::TICKS_PER_SECOND / 2, "succeeds.mp4", None).expect("second export");
    assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
    let facts = host.failure.facts();
    assert_eq!(facts.get("audioEncoder"), Some(&Value::Null), "{facts:?}");
    assert_eq!(
        facts.get("hardwareDecodeReady"),
        Some(&result["hardwareDecodeReady"]),
        "{facts:?}"
    );
    assert!(
        facts
            .get("pipeline")
            .is_some_and(|pipeline| pipeline["encodeChannelDepth"].is_u64()),
        "{facts:?}"
    );
    // No raw video reaches the CPU: planes are copied into the encoder's image on the GPU or drawn
    // there directly.
    let performance = &result["performance"];
    assert_eq!(
        performance["outputReadbackBytes"],
        json!(0),
        "{performance}"
    );
    assert_eq!(performance["outputCpuPackBytes"], json!(0), "{performance}");
    let (operations, bytes) = match capability.handoff {
        alpha2_native::EncoderInputHandoff::GpuCopy => (30, 15 * 640 * 360 * 3 / 2),
        alpha2_native::EncoderInputHandoff::DirectTarget => (0, 0),
    };
    assert_eq!(
        performance["encoderInputCopyOperations"],
        json!(operations),
        "{performance}"
    );
    assert_eq!(
        performance["encoderInputCopyBytes"],
        json!(bytes),
        "{performance}"
    );
    assert_eq!(result["frames"], json!(15));
    let _ = std::fs::remove_dir_all(directory);
}

/// Decoding in software does not take the output off the GPU: the shared
/// device is kept for the encoder even when decode never touches it.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
fn a_software_decoded_export_still_encodes_on_the_gpu() {
    let _gpu = gpu_test_lock();
    let Some((mut host, _, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("software-gpu");
    let end = alpha2_time::TICKS_PER_SECOND / 2;
    let project = json!({
        "schema_version": 1, "id": "software-gpu", "name": "Software decode, GPU encode",
        "settings": {"width": 640, "height": 360, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let assets = HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]);
    let result = host
        .export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join("out.mp4"),
            &mut |_| Ok(()),
        )
        .expect("export");
    let _ = std::fs::remove_dir_all(directory);
    assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
    assert_eq!(result["hardwareDecodeReady"], json!(false), "{result}");
    assert_eq!(result.get("renderCache"), Some(&Value::Null), "{result}");
    let performance = &result["performance"];
    assert_eq!(
        performance["hardwareDecodedFrames"],
        json!(0),
        "{performance}"
    );
    assert_eq!(
        performance["softwareDecodedFrames"],
        json!(15),
        "{performance}"
    );
    assert!(
        performance["sourceUploadBytes"].as_u64() > Some(0),
        "{performance}"
    );
    for key in ["sourceUploads", "directYuvFrames"] {
        assert_eq!(performance[key], json!(15), "{key}: {performance}");
    }
    assert!(
        performance["peakDecodeQueueBytes"].as_u64() > Some(0),
        "{performance}"
    );
    assert_eq!(
        performance["outputReadbackBytes"],
        json!(0),
        "{performance}"
    );
    assert_eq!(result["frames"], json!(15));
}

/// A project of generated sources alone exports with no assets: the host
/// decodes nothing for them.
#[test]
fn a_generated_source_exports_without_an_asset() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("generated");
    let end_ticks = alpha2_time::TICKS_PER_SECOND;
    let white = json!({"red": 1, "green": 1, "blue": 1, "alpha": 1});
    let project = json!({
        "schema_version": 1, "id": "generated", "name": "Generated",
        "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "white", "kind": "generated",
             "generator": {"type": "fill", "paint": {"type": "solid", "color": white}}}
        ],
        "timeline": {"tracks": [
            {"id": "video", "name": "Video", "kind": "video", "clips": [{
                "id": "clip", "source_id": "white",
                "timeline_range": {"start": 0, "end": end_ticks},
                "source_range": {"start": 0, "end": end_ticks}
            }]}
        ]}
    });
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let output = directory.join("generated.mp4");
    let result = host
        .export(
            &project.to_string(),
            &HashMap::new(),
            &HashMap::new(),
            Vec::new(),
            0,
            end_ticks,
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
        )
        .expect("export");
    assert_eq!(result["frames"], json!(30), "{result}");
    let performance = &result["performance"];
    let packed = 30 * 64 * 48 * 3 / 2;
    assert_eq!(
        performance["outputCpuPackBytes"],
        json!(packed),
        "{performance}"
    );
    assert!(
        performance["outputReadbackBytes"].as_u64() >= Some(packed),
        "{performance}"
    );
    assert_eq!(performance["encoderInputCopyOperations"], json!(0));
    assert_eq!(performance["encoderInputCopyBytes"], json!(0));

    let mut video = alpha2_native::NativeDecoder::open(&output).expect("open video");
    let cancel = CancelToken::default();
    let luma = loop {
        match video.decode_next(&cancel).expect("decode video") {
            alpha2_media::DecodeEvent::Video(frame) => {
                let luma = &frame.planes[0];
                let visible = luma
                    .bytes
                    .chunks(luma.stride as usize)
                    .flat_map(|row| &row[..luma.width as usize]);
                #[allow(clippy::cast_precision_loss)]
                let mean = visible.clone().map(|&value| f64::from(value)).sum::<f64>()
                    / visible.count() as f64;
                break mean;
            }
            alpha2_media::DecodeEvent::EndOfStream => panic!("no frame in {output:?}"),
            _ => {}
        }
    };
    // White in limited range.
    assert!((luma - 235.0).abs() < 3.0, "mean luma {luma}");
    drop(video);
    std::fs::remove_dir_all(&directory).ok();
}

/// A clip shown as it is reaches the GPU encoder as its decoded planes, read
/// in place from a decoder surface or uploaded from a software decoder: no
/// graph pass and no color conversion. A clip whose chroma sits elsewhere
/// than the output's, or the same clip scaled, renders instead, and says why.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn an_untouched_clip_is_transcoded_from_its_decoded_planes() {
    let _gpu = gpu_test_lock();
    let Some((mut hardware, _, _)) = gpu_output_host(VideoDecodePolicy::Auto) else {
        return;
    };
    // Software decode keeps the adapter wgpu prefers, which on a hybrid Windows laptop can be one
    // without the encoder: the same retry finds the adapter that has it.
    let (mut software, _, _) = gpu_output_host(VideoDecodePolicy::Software)
        .expect("a software-decode host encodes on the GPU where a hardware-decode one does");
    let imported_h264 = imports_decoded(&hardware, "h264");
    let directory = scratch_directory("yuv-transcode");
    let end = alpha2_time::TICKS_PER_SECOND / 2;
    let export = |host: &mut Host, source: &Path, width: u32, height: u32, name: &str| {
        let project = json!({
            "schema_version": 1, "id": "transcode", "name": "Transcode",
            "settings": {"width": width, "height": height, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        });
        let assets = HashMap::from([("video".to_owned(), source.to_owned())]);
        let output = directory.join(name);
        let result = host
            .export(
                &project.to_string(),
                &assets,
                &HashMap::new(),
                Vec::new(),
                0,
                end,
                RateRequest { num: 30, den: 1 },
                false,
                crate::protocol::ResolvedFormat {
                    container: alpha2_media::Container::Mp4,
                    video: NativeVideoCodec::H264,
                    audio: NativeAudioCodec::Aac,
                    pixels: alpha2_media::PixelLayout::YUV420P,
                },
                None,
                VideoQualityRequest::Level(QualityLevel::VeryHigh),
                AudioQualityRequest::Level(QualityLevel::High),
                None,
                &output,
                &mut |_| Ok(()),
            )
            .expect("export");
        (result, output)
    };

    let source = corpus("h264-bt709-transcode.mp4");
    for (name, host, imported) in [
        ("hardware decode", &mut hardware, imported_h264),
        ("software decode", &mut software, Some(false)),
    ] {
        let (result, output) = export(host, &source, 256, 144, &format!("{name}.mp4"));
        assert_eq!(
            result["videoPipeline"],
            json!("gpu-resident"),
            "{name}: {result}"
        );
        assert_eq!(
            result["videoTranscode"],
            json!("yuv-repack"),
            "{name}: {result}"
        );
        assert_eq!(
            result["yuvTranscodeIneligible"],
            Value::Null,
            "{name}: {result}"
        );
        let performance = &result["performance"];
        let imported =
            imported.unwrap_or_else(|| performance["hardwareImportFrames"].as_u64() > Some(0));
        for (key, expected) in [
            ("graphNodes", 0),
            ("graphPasses", 0),
            ("sourceConversions", 0),
            ("yuvRepacks", 15),
            ("outputReadbackBytes", 0),
            ("hardwareImportFrames", if imported { 15 } else { 0 }),
        ] {
            assert_eq!(
                performance[key],
                json!(expected),
                "{name} {key}: {performance}"
            );
        }
        assert_eq!(
            performance["sourceUploadBytes"].as_u64() == Some(0),
            imported,
            "{name}: {performance}"
        );
        for (key, expected) in [
            ("directYuvFrames", 15),
            ("sourceUploads", if imported { 0 } else { 15 }),
        ] {
            assert_eq!(
                performance[key],
                json!(expected),
                "{name} {key}: {performance}"
            );
        }
        assert!(
            performance["peakDecodeQueueBytes"].as_u64() > Some(0),
            "{name}: {performance}"
        );
        assert_eq!(
            performance["peakNativeSurfaces"].as_u64() > Some(0),
            imported,
            "{name}: {performance}"
        );
        let milliseconds = |key: &str| performance[key].as_f64().expect(key);
        let parts = milliseconds("sourceCpuPlaneUploadMs")
            + milliseconds("sourceGpuImportMs")
            + milliseconds("surfaceBudgetWaitMs");
        assert!(
            (milliseconds("sourceConvertUploadMs") - parts).abs() < 1e-6,
            "{name}: {parts} ms of parts: {performance}"
        );
        // What the encoder wrote decodes back to the source's pictures plane for plane, up to
        // coding loss: the chroma arrived through the copier's second region as the luma through
        // its first.
        let exported = decoded_planes(&output);
        let original = decoded_planes(&source);
        assert_eq!(exported.len(), 15, "{name}");
        if std::env::var_os("ALPHA2_PLANE_PSNR_REPORT").is_some() {
            report_plane_psnr(name, &exported, &original, 128);
        }
        if let Err(problem) = plane_check(&exported, &original, 256, 144) {
            panic!("{name}: {problem}");
        }
    }

    // H.264 sites its chroma left and this VP9 file reads as centred, so repacking would move the
    // chroma half a sample.
    let rendered = |result: &Value, reason: &str| {
        assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
        assert_eq!(result["videoTranscode"], Value::Null, "{result}");
        assert!(
            result["yuvTranscodeIneligible"]
                .as_str()
                .is_some_and(|ineligible| ineligible.contains(reason)),
            "{result}"
        );
        let performance = &result["performance"];
        assert_eq!(performance["yuvRepacks"], json!(0), "{performance}");
        for key in ["graphPasses", "sourceConversions"] {
            assert!(performance[key].as_u64() > Some(0), "{key}: {performance}");
        }
    };
    let (result, _) = export(
        &mut hardware,
        &corpus("vp9-color.webm"),
        256,
        64,
        "unsited.mp4",
    );
    rendered(&result, "chroma sits Unspecified");
    let (result, _) = export(&mut hardware, &source, 512, 288, "rendered.mp4");
    rendered(&result, "not the 512x288 output");
    let _ = std::fs::remove_dir_all(directory);
}

/// A rendered GPU-resident export carries each frame's own colors into its
/// chroma: the render packs them into the plane ring and the copier moves
/// that ring's chroma texture into the encoder's second plane, frame by
/// frame. Every frame here is another solid primary or secondary, so chroma
/// that was not copied, came from another slot, or had U and V swapped
/// decodes as the wrong color. Primaries keep the transfer
/// function out of it: 0 and 1 encode the same under every curve.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn a_rendered_gpu_resident_export_carries_each_frames_colors_into_its_chroma() {
    let _gpu = gpu_test_lock();
    let Some((mut host, _, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("gpu-resident-chroma");
    let colors: [[f64; 3]; 6] = [
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [1.0, 0.0, 1.0],
    ];
    let frames = 15_i64;
    let frame_ticks = alpha2_time::TICKS_PER_SECOND / 30;
    let end = frames * frame_ticks;
    let sources = colors
        .iter()
        .enumerate()
        .map(|(index, [red, green, blue])| {
            json!({"id": format!("color-{index}"), "kind": "generated",
                   "generator": {"type": "fill", "paint": {"type": "solid",
                       "color": {"red": red, "green": green, "blue": blue, "alpha": 1}}}})
        })
        .collect::<Vec<_>>();
    let clips = (0..frames)
        .map(|frame| {
            json!({"id": format!("frame-{frame}"),
                   "source_id": format!("color-{}", frame % 6),
                   "timeline_range": {"start": frame * frame_ticks, "end": (frame + 1) * frame_ticks},
                   "source_range": {"start": 0, "end": frame_ticks}})
        })
        .collect::<Vec<_>>();
    let project = json!({
        "schema_version": 1, "id": "chroma", "name": "Chroma",
        "settings": {"width": 256, "height": 144, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": sources,
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": clips}]}
    });
    let output = directory.join("colors.mp4");
    let result = host
        .export(
            &project.to_string(),
            &HashMap::new(),
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::VeryHigh),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            &output,
            &mut |_| Ok(()),
        )
        .expect("export");
    assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
    assert_eq!(result["videoTranscode"], Value::Null, "{result}");
    assert_eq!(
        result["performance"]["outputReadbackBytes"],
        json!(0),
        "{result}"
    );

    // BT.709 limited range, as the export tags it.
    let expected = |[red, green, blue]: [f64; 3]| {
        let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
        [
            16.0 + 219.0 * luma,
            128.0 + 224.0 * (blue - luma) / 1.8556,
            128.0 + 224.0 * (red - luma) / 1.5748,
        ]
    };
    let decoded = decoded_planes(&output);
    assert_eq!(decoded.len(), 15);
    for (index, planes) in decoded.iter().enumerate() {
        let want = expected(colors[index % 6]);
        assert_eq!(planes.len(), 3, "frame {index}: Y, U and V");
        for (plane, (samples, want)) in planes.iter().zip(want).enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let mean =
                samples.iter().map(|&value| f64::from(value)).sum::<f64>() / samples.len() as f64;
            let worst = samples
                .iter()
                .map(|&value| (f64::from(value) - want).abs())
                .fold(0.0, f64::max);
            assert!(
                (mean - want).abs() < 2.5 && worst < 8.0,
                "frame {index} plane {plane}: mean {mean}, worst {worst} codes from {want}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(directory);
}

/// Each decoded picture of a transcode is checked again as it arrives, since
/// a stream may change mid-way: a picture the render would have moved,
/// scaled, turned, squared, recolored or resited, or one that is not 8-bit
/// 4:2:0, is refused, and the one shown as it is passes.
#[cfg(feature = "hwencode")]
#[test]
#[allow(clippy::too_many_lines)]
fn a_decoded_picture_is_transcoded_only_when_a_render_would_not_change_it() {
    use alpha2_media::{ChromaLocation, ColorMetadata, YuvEncoding, YuvFormat};
    use alpha2_native::{NativePlaneLayout, NativeVideoFrameMetadata};
    let size = RenderSize::new(256, 144).expect("size");
    let encoding = YuvEncoding::export(alpha2_media::VideoCodec::H264);
    let shown = NativeVideoFrameMetadata {
        pts: MediaTime::ZERO,
        dts: None,
        duration: None,
        visible_rect: alpha2_media::Rect {
            x: 0,
            y: 0,
            width: 256,
            height: 144,
        },
        orientation: alpha2_media::Orientation::UPRIGHT,
        sample_aspect_ratio: alpha2_media::SampleAspectRatio::default(),
        color: Some(ColorMetadata::EXPORT),
        chroma_location: ChromaLocation::Left,
        bit_depth: 8,
        plane_layout: NativePlaneLayout::CpuYuv(YuvFormat::I420),
        device_identity: 0,
        decoder_generation: 0,
        picture_identity: 0,
    };
    for layout in [
        NativePlaneLayout::CpuYuv(YuvFormat::I420),
        NativePlaneLayout::CpuYuv(YuvFormat::NV12),
        NativePlaneLayout::GpuI420,
        NativePlaneLayout::GpuNv12,
    ] {
        let picture = NativeVideoFrameMetadata {
            plane_layout: layout,
            ..shown.clone()
        };
        assert_eq!(
            crate::frames::check_repackable(&picture, size, encoding),
            Ok(()),
            "{layout:?}"
        );
    }
    let bt601 = ColorMetadata {
        matrix: alpha2_media::ColorMatrix::Bt601,
        ..ColorMetadata::EXPORT
    };
    assert_eq!(ColorMetadata::UNDECLARED, ColorMetadata::EXPORT);
    let undeclared = NativeVideoFrameMetadata {
        color: None,
        ..shown.clone()
    };
    assert_eq!(
        crate::frames::check_repackable(&undeclared, size, encoding),
        Ok(())
    );
    let refused: [(&str, NativeVideoFrameMetadata, &str); 7] = [
        (
            "cropped",
            NativeVideoFrameMetadata {
                visible_rect: alpha2_media::Rect {
                    x: 0,
                    y: 0,
                    width: 254,
                    height: 144,
                },
                ..shown.clone()
            },
            "not the 256x144 output",
        ),
        (
            "rotated",
            NativeVideoFrameMetadata {
                orientation: alpha2_media::Orientation {
                    rotation: alpha2_media::Rotation::Deg180,
                    mirror: false,
                },
                ..shown.clone()
            },
            "rotated or mirrored",
        ),
        (
            "mirrored",
            NativeVideoFrameMetadata {
                orientation: alpha2_media::Orientation {
                    rotation: alpha2_media::Rotation::Deg0,
                    mirror: true,
                },
                ..shown.clone()
            },
            "rotated or mirrored",
        ),
        (
            "anamorphic",
            NativeVideoFrameMetadata {
                sample_aspect_ratio: alpha2_media::SampleAspectRatio { num: 4, den: 3 },
                ..shown.clone()
            },
            "shows 4:3 pixels",
        ),
        (
            "another matrix",
            NativeVideoFrameMetadata {
                color: Some(bt601),
                ..shown.clone()
            },
            "not the output's",
        ),
        (
            "centred chroma",
            NativeVideoFrameMetadata {
                chroma_location: ChromaLocation::Center,
                ..shown.clone()
            },
            "chroma sits Center",
        ),
        (
            "10-bit",
            NativeVideoFrameMetadata {
                plane_layout: NativePlaneLayout::GpuP010,
                bit_depth: 10,
                ..shown.clone()
            },
            "not 8-bit 4:2:0",
        ),
    ];
    for (label, picture, reason) in refused {
        let Err(error) = crate::frames::check_repackable(&picture, size, encoding) else {
            panic!("{label} is transcoded");
        };
        assert!(error.contains(reason), "{label}: {error}");
    }
    let Err(error) = crate::frames::check_repackable(
        &NativeVideoFrameMetadata {
            plane_layout: NativePlaneLayout::CpuYuv(YuvFormat::planar(1, 0, 8)),
            ..shown
        },
        size,
        encoding,
    ) else {
        panic!("4:2:2 is transcoded");
    };
    assert!(error.contains("not 8-bit 4:2:0"), "{error}");
}

/// A transcoded frame's picture leaves its decoded set: once the frame drops
/// it, nothing the set's decode-budget bytes paid for is still held, so the
/// release that follows is exact.
#[cfg(feature = "hwencode")]
#[test]
fn a_transcoded_picture_is_the_last_of_its_frame_set() {
    let end = alpha2_time::TICKS_PER_SECOND / 2;
    let project = Project::from_json(
        &json!({
            "schema_version": 1, "id": "transcode", "name": "Transcode",
            "settings": {"width": 256, "height": 144, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        })
        .to_string(),
    )
    .expect("project");
    let assets = HashMap::from([("video".to_owned(), corpus("h264-bt709-transcode.mp4"))]);
    let plan = crate::yuv_transcode::yuv_transcode_plan(
        &project,
        &assets,
        TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(end)).expect("range"),
        alpha2_media::YuvEncoding::export(alpha2_media::VideoCodec::H264),
    )
    .unwrap_or_else(|reason| panic!("the clip is transcoded: {reason}"));
    let frame = Arc::new(alpha2_native::NativeVideoFrame::Cpu(DecodedVideoFrame {
        pts: MediaTime::ZERO,
        dts: None,
        duration: None,
        visible_rect: Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        },
        sample_aspect_ratio: SampleAspectRatio::default(),
        orientation: Orientation::UPRIGHT,
        pixel_format: PixelFormat::Yuv(alpha2_media::YuvFormat::I420),
        chroma_location: alpha2_media::ChromaLocation::Left,
        color: Some(ColorMetadata::EXPORT),
        planes: Vec::new(),
    }));
    let held = Arc::downgrade(&frame);
    let set = crate::decode::DecodedFrameSet {
        generation: 0,
        index: 0,
        time: MediaTime::ZERO,
        frames: vec![DecodedSourceFrame {
            source: SourceId::new("video").expect("source"),
            clip: ClipId::new("clip").expect("clip"),
            requested: MediaTime::ZERO,
            kind: MediaKind::Video,
            frame,
        }],
        stills: Vec::new(),
        bytes: 6,
        decode_time: Duration::ZERO,
        stage_timings: alpha2_native::DecodeStageTimings::default(),
        reverse_cache_hits: 0,
        reverse_windows_built: 0,
        reverse_cache_bytes: 0,
        software_decoded_frames: 1,
        hardware_decoded_frames: 0,
        hardware_fallbacks: HardwareFallbackCounts::default(),
        decoder_session_reopens: 0,
    };
    let picture = plan.picture(set).expect("the frame's picture");
    drop(picture);
    assert!(
        held.upgrade().is_none(),
        "the picture is still held once the frame let it go"
    );
}

/// On Windows, a hardware-decoded clip reaches the render as D3D12 copies of
/// the decoder's surfaces, one per frame, and exports the pictures a
/// software-decoded run of the same clip does. Without a D3D12VA device it
/// skips, recorded as the `d3d12-video` capability.
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
#[test]
fn a_d3d12va_decoded_export_renders_copies_of_its_surfaces_as_software_decode_would() {
    let _gpu = gpu_test_lock();
    let mut hardware = create_gpu_test_host(VideoDecodePolicy::Auto).expect("host");
    if !hardware.hardware_decode_ready() {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::D3d12Video,
            "no D3D12VA device for the renderer's adapter"
        );
        return;
    }
    let mut software = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("d3d12va-import");
    let end = alpha2_time::TICKS_PER_SECOND;
    let project = json!({
        "schema_version": 1, "id": "d3d12va", "name": "D3D12VA import",
        "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let assets = HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]);
    let mut outputs = Vec::new();
    for (name, host) in [("hardware", &mut hardware), ("software", &mut software)] {
        let output = directory.join(format!("{name}.mp4"));
        let result = host
            .export(
                &project.to_string(),
                &assets,
                &HashMap::new(),
                Vec::new(),
                0,
                end,
                RateRequest { num: 30, den: 1 },
                false,
                crate::protocol::ResolvedFormat {
                    container: alpha2_media::Container::Mp4,
                    video: NativeVideoCodec::Mpeg4Part2,
                    audio: NativeAudioCodec::Aac,
                    pixels: alpha2_media::PixelLayout::YUV420P,
                },
                None,
                VideoQualityRequest::Level(QualityLevel::VeryHigh),
                AudioQualityRequest::Level(QualityLevel::High),
                None,
                &output,
                &mut |_| Ok(()),
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let performance = &result["performance"];
        let (decoded, imported) = if name == "hardware" { (30, 30) } else { (0, 0) };
        for (key, expected) in [
            ("hardwareDecodedFrames", decoded),
            ("hardwareImportFrames", imported),
            ("hardwareCopyFrames", imported),
            ("softwareDecodedFrames", 30 - decoded),
        ] {
            assert_eq!(
                performance[key],
                json!(expected),
                "{name} {key}: {performance}"
            );
        }
        assert_eq!(
            performance["hardwareFallbackReasons"],
            json!({}),
            "{name}: {performance}"
        );
        outputs.push(decoded_luma(&output));
    }
    // H.264 decodes bit-exactly either way, so the two renders, and what the same encoder made of
    // them, match up to its own rounding.
    let [hardware, software] = <[_; 2]>::try_from(outputs).expect("two exports");
    assert_eq!(hardware.len(), 30);
    assert_eq!(hardware.len(), software.len());
    for (index, (hardware, software)) in hardware.iter().zip(&software).enumerate() {
        let difference = hardware
            .iter()
            .zip(software)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum::<u64>();
        let samples = u64::try_from(hardware.len()).expect("sample count");
        assert!(
            difference < samples,
            "frame {index} differs by {difference} over {samples} samples"
        );
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn a_gpu_resident_export_writes_every_container_that_offers_its_codec() {
    use alpha2_media::Container;
    let _gpu = gpu_test_lock();
    let Some((mut host, capability, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("gpu-resident-containers");
    let second = alpha2_time::TICKS_PER_SECOND;
    let project = json!({
        "schema_version": 1,
        "id": "gpu-resident-containers",
        "name": "GPU-resident containers",
        "settings": {
            "width": 640,
            "height": 360,
            "frame_rate": {"num": 30, "den": 1},
            "sample_rate": 48000
        },
        "sources": [
            {"id": "video", "locator": "fixture://video", "kind": "video"},
            {"id": "audio", "locator": "fixture://audio", "kind": "audio"}
        ],
        "timeline": {"tracks": [
            {"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": second},
                "source_range": {"start": 0, "end": second}
            }]},
            {"id": "a", "name": "A", "kind": "audio", "clips": [{
                "id": "tone", "source_id": "audio",
                "timeline_range": {"start": 0, "end": second},
                "source_range": {"start": 0, "end": second}
            }]}
        ]}
    });
    let assets = HashMap::from([
        ("video".to_owned(), corpus("h264-long-gop.mp4")),
        ("audio".to_owned(), corpus("audio-only-aac-priming.m4a")),
    ]);
    let opus = alpha2_native::codec_capabilities()
        .audio
        .iter()
        .any(|entry| entry.codec == NativeAudioCodec::Opus);
    let mut cases = vec![
        (Container::Mov, capability.codec, NativeAudioCodec::Aac),
        (Container::Mkv, capability.codec, NativeAudioCodec::Aac),
    ];
    if opus {
        cases.push((Container::Mkv, capability.codec, NativeAudioCodec::Opus));
    }
    for (container, video, audio) in cases {
        if !container.offers_video(video.model()) {
            continue;
        }
        let output = directory.join(format!(
            "resident-{}.{}",
            audio.as_str(),
            container.extensions()[0]
        ));
        let result = host
            .export(
                &project.to_string(),
                &assets,
                &HashMap::new(),
                Vec::new(),
                0,
                second,
                RateRequest { num: 30, den: 1 },
                true,
                crate::protocol::ResolvedFormat {
                    container,
                    video,
                    audio,
                    pixels: alpha2_media::PixelLayout::YUV420P,
                },
                None,
                VideoQualityRequest::Level(QualityLevel::High),
                AudioQualityRequest::Level(QualityLevel::High),
                None,
                &output,
                &mut |_| Ok(()),
            )
            .unwrap_or_else(|error| panic!("{container} {}: {error}", audio.as_str()));
        assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
        assert_eq!(result["container"], json!(container.as_str()));
        assert_eq!(result["audioCodec"], json!(audio.as_str()));
        let probed = alpha2_native::probe(&output).expect("probe");
        let matroska = container == Container::Mkv;
        assert_eq!(
            probed.format.contains("matroska"),
            matroska,
            "{}",
            probed.format
        );
        assert_eq!(
            probed.selected_audio_stream().expect("audio").codec,
            audio.as_str()
        );
        let video_stream = probed.selected_video_stream().expect("video");
        assert_eq!(video_stream.codec, video.as_str());
    }
    let _ = std::fs::remove_dir_all(directory);
}

/// Prints, per plane, the lowest PSNR over the frames of an export against its source, and the
/// same for three defects of the source: chroma shifted one row, U and V swapped, chroma one
/// column on. The floor of [`plane_check`] was chosen from these figures. `chroma_width` is the
/// width of the U and V planes.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn report_plane_psnr(
    name: &str,
    exported: &[Vec<Vec<u8>>],
    original: &[Vec<Vec<u8>>],
    chroma_width: usize,
) {
    /// One plane of a frame's planes, as a defect makes it.
    type Defect<'a> = dyn Fn(&[Vec<u8>], usize) -> Vec<u8> + 'a;
    let lowest = |defect: &Defect<'_>| {
        (0..3)
            .map(|plane| {
                exported
                    .iter()
                    .zip(original)
                    .map(|(frame, source)| plane_psnr(&frame[plane], &defect(source, plane)))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect::<Vec<_>>()
    };
    // A plane read one row or one column on, its last row or column repeated.
    let shifted = |plane: &[u8], rows: usize, columns: usize| -> Vec<u8> {
        let height = plane.len() / chroma_width;
        (0..height)
            .flat_map(|y| {
                (0..chroma_width).map(move |x| {
                    let (sy, sx) = (
                        (y + rows).min(height - 1),
                        (x + columns).min(chroma_width - 1),
                    );
                    plane[sy * chroma_width + sx]
                })
            })
            .collect()
    };
    let real = lowest(&|source, plane| source[plane].clone());
    let row = lowest(&|source, plane| match plane {
        0 => source[0].clone(),
        _ => shifted(&source[plane], 1, 0),
    });
    let swapped = lowest(&|source, plane| match plane {
        1 => source[2].clone(),
        2 => source[1].clone(),
        _ => source[0].clone(),
    });
    let column = lowest(&|source, plane| match plane {
        0 => source[0].clone(),
        _ => shifted(&source[plane], 0, 1),
    });
    eprintln!(
        "plane PSNR {name} (Y, U, V, dB): real {real:.2?}, chroma one row down {row:.2?}, \
         U/V swapped {swapped:.2?}, chroma one column on {column:.2?}"
    );
}
