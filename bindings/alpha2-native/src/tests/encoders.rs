//! Encoder capabilities, candidates, options, and rate control.

use super::*;

fn assert_final_frame_seek_boundaries(decoder: &mut NativeDecoder, cancel: &CancelToken) {
    let source = || alpha2_media::SourceId::new("round-trip").expect("source ID");
    let mut seek = |ticks: i64, generation: u64| match decoder
        .seek(
            DecodeRequest {
                source: source(),
                time: MediaTime::from_ticks(ticks),
                kind: alpha2_media::DecodeRequestKind::Seek,
                generation,
            },
            cancel,
        )
        .expect("a seek at or past the final frame answers")
        .event
    {
        DecodeEvent::Video(frame) => frame.pts,
        event => panic!("unexpected {event:?}"),
    };
    // Past the end the final frame stays presented, as the first does before the start.
    assert_eq!(seek(7_999, 2), MediaTime::from_ticks(4_000));
    assert_eq!(seek(8_000, 3), MediaTime::from_ticks(4_000));
    assert_eq!(seek(120_000, 4), MediaTime::from_ticks(4_000));
}

#[test]
fn codec_capabilities_report_stable_logical_ids_and_concrete_encoders() {
    let capabilities = codec_capabilities();
    assert!(capabilities.video.iter().any(|capability| {
        capability.codec == NativeVideoCodec::Mpeg4Part2 && !capability.encoder_name.is_empty()
    }));
    assert!(capabilities.audio.iter().any(|capability| {
        capability.codec == NativeAudioCodec::Aac && !capability.encoder_name.is_empty()
    }));
    assert_eq!(NativeVideoCodec::H264.as_str(), "h264");
    assert_eq!(NativeVideoCodec::Hevc.as_str(), "hevc");
    assert_eq!(NativeVideoCodec::Vp9.as_str(), "vp9");
    assert_eq!(NativeVideoCodec::Av1.as_str(), "av1");
    assert_eq!(NativeVideoCodec::Mpeg4Part2.as_str(), "mpeg4-part2");
    assert_eq!(NativeAudioCodec::Aac.as_str(), "aac");

    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Vp9,
        NativeVideoCodec::Av1,
        NativeVideoCodec::Mpeg4Part2,
    ] {
        let encoders = capabilities
            .video
            .iter()
            .filter(|capability| capability.codec == codec)
            .collect::<Vec<_>>();
        if encoders.is_empty() {
            continue;
        }
        assert_eq!(
            encoders
                .iter()
                .filter(|capability| capability.preferred)
                .count(),
            1
        );
        if encoders
            .iter()
            .any(|capability| capability.hardware_accelerated)
        {
            assert!(encoders[0].hardware_accelerated);
        }
    }

    assert!(matches!(
        select_video_encoder(NativeVideoCodec::Mpeg4Part2, Some("aac")),
        Err(EncodeError::Adapter(message)) if message.contains("does not implement")
    ));

    if let Some(h264) = capabilities
        .video
        .iter()
        .find(|capability| capability.codec == NativeVideoCodec::H264)
        .filter(|capability| capability.minimum_width > 64 || capability.minimum_height > 36)
    {
        let alternatives = capabilities
            .video
            .iter()
            .filter(|capability| capability.codec == NativeVideoCodec::H264)
            .filter(|capability| capability.minimum_width <= 64 && capability.minimum_height <= 36)
            .count();
        let path = output_path("undersized-h264");
        let _ = fs::remove_file(&path);
        match NativeVideoEncodeSink::create(
            &path,
            VideoEncodeConfig {
                width: 64,
                height: 36,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                color: ColorMetadata::default(),
                rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable).unwrap(),
                codec: NativeVideoCodec::H264,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
        ) {
            Ok(sink) => {
                assert!(alternatives > 0, "nothing was available to fall back to");
                assert_ne!(sink.selection().encoder_name, h264.encoder_name);
                drop(sink);
                let _ = fs::remove_file(&path);
            }
            Err(error) => {
                assert_eq!(alternatives, 0);
                let EncodeError::EncodersRefused { message, refusals } = error else {
                    panic!("{error:?}");
                };
                assert!(
                    message.contains(&h264.encoder_name) && message.contains("(dimensions)"),
                    "{message}"
                );
                assert!(
                    refusals
                        .iter()
                        .all(|refusal| refusal.kind == OpenFailureKind::Unsuitable),
                    "{refusals:?}"
                );
                assert!(!path.exists());
            }
        }
    }
}

#[test]
fn a_rejected_candidate_falls_through_to_the_next_one() {
    // Needs more than one H.264 encoder: a hardware candidate and a software floor.
    let skip = |why: &str| {
        alpha2_test_support::skip!(alpha2_test_support::Capability::HardwareH264Encode, "{why}");
    };
    let Ok(candidates) = video_encoder_candidates(NativeVideoCodec::H264, None) else {
        skip("this runtime has no H.264 encoder");
        return;
    };
    if candidates.len() < 2 {
        skip("this runtime has one H.264 encoder, so nothing to fall through to");
        return;
    }
    let first = candidates[0].name();
    let (minimum_width, minimum_height) = crate::capabilities::encoder_minimum_dimensions(&first);
    if minimum_width <= 64 && minimum_height <= 48 {
        skip(&format!(
            "the first H.264 candidate {first} accepts 64x48, so nothing rejects it"
        ));
        return;
    }
    let path = output_path("h264-fallback");
    let _ = fs::remove_file(&path);
    let sink = NativeVideoEncodeSink::create(
        &path,
        VideoEncodeConfig {
            width: 64,
            height: 48,
            frame_rate: FrameRate::new(30, 1).expect("rate"),
            color: ColorMetadata::default(),
            rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable).unwrap(),
            codec: NativeVideoCodec::H264,
            display: crate::VideoDisplay::default(),
            container: alpha2_media::Container::Mp4,
            pixels: alpha2_media::PixelLayout::YUV420P,
        },
    )
    .expect("a candidate behind the rejected one must open");
    let selection = sink.selection();
    assert_ne!(selection.encoder_name, first);
    assert_eq!(selection.rejected.len(), 1);
    assert_eq!(selection.rejected[0].encoder_name, first);
    assert_eq!(
        selection.rejected[0].reason,
        crate::capabilities::NativeEncoderRejection::Dimensions
    );
    drop(sink);
    let _ = fs::remove_file(&path);
}

#[test]
#[allow(unsafe_code)]
fn a_failed_open_with_options_does_not_free_them_twice() {
    // `avcodec_open2` updates the dictionary even on failure, which rsmpeg 0.18
    // handles only on success. An encoder that excludes YUV420P (one taking only
    // device frames) exercises that path.
    let encoder = AVCodec::iterate()
        .find(|codec| {
            // SAFETY: `codec` is a registered codec descriptor.
            let encoder = unsafe { rsmpeg::ffi::av_codec_is_encoder(codec.as_ptr()) } != 0;
            encoder
                && codec.type_ == ffi::AVMEDIA_TYPE_VIDEO
                && codec
                    .pix_fmts()
                    .is_some_and(|formats| !formats.contains(&ffi::AV_PIX_FMT_YUV420P))
        })
        .expect("a registered video encoder that does not take YUV420P");
    let mut context = AVCodecContext::new(&encoder);
    context.set_width(1_920);
    context.set_height(1_080);
    context.set_pix_fmt(ffi::AV_PIX_FMT_YUV420P);
    context.set_time_base(ra(1, 30));
    let options = video_encoder_options(ColorMetadata::default(), &encoder, true, 1, 640);
    let error = alpha2_ffmpeg_bridge::open_codec_context(&mut context, options)
        .expect_err("a software frame format the encoder does not declare must be refused");
    assert!(format!("{error}").contains("Invalid argument"), "{error}");
}

#[test]
fn encoder_candidates_are_ordered_and_a_missing_codec_names_the_runtime() {
    let capabilities = codec_capabilities();
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Vp9,
        NativeVideoCodec::Av1,
        NativeVideoCodec::Mpeg4Part2,
    ] {
        let advertised = capabilities
            .video
            .iter()
            .filter(|capability| capability.codec == codec)
            .map(|capability| capability.encoder_name.clone())
            .collect::<Vec<_>>();
        match video_encoder_candidates(codec, None) {
            Ok(candidates) => {
                // The fallback walks exactly the advertised list, in the same order.
                assert_eq!(
                    candidates
                        .iter()
                        .map(crate::capabilities::VideoEncoderChoice::name)
                        .collect::<Vec<_>>(),
                    advertised
                );
            }
            Err(EncodeError::Adapter(message) | EncodeError::EncodersRefused { message, .. }) => {
                assert_eq!(advertised, [] as [String; 0]);
                assert!(message.contains("FFmpeg runtime"), "{message}");
                assert!(message.contains(codec.as_str()), "{message}");
            }
            Err(other) => panic!("unexpected candidate error: {other:?}"),
        }
    }
}

#[test]
fn a_hybrid_encoder_set_to_the_gpu_keeps_its_cpu_mode_behind_it() {
    // `MediaFoundation` enumerates hardware or software transforms, never both, so the GPU mode
    // must keep the software encoder behind it.
    assert_eq!(NativeEncoderClass::Hybrid.modes(true), [true, false]);
    assert_eq!(NativeEncoderClass::Hybrid.modes(false), [false]);
    for hybrid_hardware in [false, true] {
        assert_eq!(NativeEncoderClass::Software.modes(hybrid_hardware), [false]);
        assert_eq!(NativeEncoderClass::Hardware.modes(hybrid_hardware), [true]);
    }
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Mpeg4Part2,
    ] {
        let candidates = crate::capabilities::registered_video_encoders(codec);
        assert!(
            candidates
                .windows(2)
                .all(|pair| pair[0].on_hardware >= pair[1].on_hardware)
        );
        for candidate in &candidates {
            let modes =
                encoder_class(&candidate.encoder).modes(crate::hybrid_encoders_use_hardware());
            assert!(
                modes.contains(&candidate.on_hardware),
                "{}",
                candidate.name()
            );
            assert_eq!(
                candidates
                    .iter()
                    .filter(|other| other.encoder.as_ptr() == candidate.encoder.as_ptr())
                    .count(),
                modes.len()
            );
        }
    }
}

/// `FFmpeg` 8.x declares `h264_videotoolbox` with neither hardware nor hybrid bit, though it opens
/// only on hardware. The class corrects misdeclared encoders, only while neither bit is declared.
#[test]
fn an_encoder_the_runtime_misdeclares_takes_the_class_it_runs_as() {
    use crate::capabilities::declared_encoder_class;
    let neither = ffi::AV_CODEC_CAP_DR1 | ffi::AV_CODEC_CAP_DELAY;
    let hardware = neither | ffi::AV_CODEC_CAP_HARDWARE;
    let hybrid = neither | ffi::AV_CODEC_CAP_HYBRID;
    // The pinned runtime's declaration (0x22).
    assert_eq!(
        declared_encoder_class("h264_videotoolbox", neither),
        NativeEncoderClass::Hardware
    );
    assert_eq!(
        declared_encoder_class("h264_videotoolbox", hardware),
        NativeEncoderClass::Hardware
    );
    assert_eq!(
        declared_encoder_class("h264_videotoolbox", hybrid),
        NativeEncoderClass::Hybrid
    );
    assert_eq!(
        declared_encoder_class("hevc_videotoolbox", hardware),
        NativeEncoderClass::Hardware
    );
    assert_eq!(
        declared_encoder_class("h264_mf", hybrid),
        NativeEncoderClass::Hybrid
    );
    // Any other encoder declaring neither is software, whatever its name says.
    for name in [
        "libx264",
        "mpeg4",
        "h264_omx",
        "hevc_videotoolbox",
        "h264_videotoolbox2",
    ] {
        assert_eq!(
            declared_encoder_class(name, neither),
            NativeEncoderClass::Software,
            "{name}"
        );
    }
    let class = declared_encoder_class("h264_videotoolbox", neither);
    for hybrid_hardware in [false, true] {
        assert_eq!(class.modes(hybrid_hardware), [true]);
    }
}

#[test]
fn codec_threading_policy_and_unused_option_guard_are_explicit() {
    // The class comes from the capability bits, so unknown encoders still classify.
    for name in [
        c"h264_nvenc",
        c"hevc_videotoolbox",
        c"h264_videotoolbox",
        c"h264_vulkan",
    ] {
        if let Some(encoder) = AVCodec::find_encoder_by_name(name) {
            assert_eq!(encoder_class(&encoder), NativeEncoderClass::Hardware);
        }
    }
    if let Some(encoder) = AVCodec::find_encoder_by_name(c"h264_vulkan") {
        // Vulkan takes `AV_PIX_FMT_VULKAN` only, so the YUV420P sink must not advertise it.
        assert!(!encoder_is_compatible(
            &crate::capabilities::VideoEncoderCandidate {
                encoder,
                on_hardware: true,
            }
        ));
    }
    for name in [c"libx264", c"mpeg4"] {
        if let Some(encoder) = AVCodec::find_encoder_by_name(name) {
            assert_eq!(encoder_class(&encoder), NativeEncoderClass::Software);
            assert_eq!(encoder_class(&encoder).modes(true), [false]);
        }
    }
    assert_eq!(encoder_max_b_frames("h264_videotoolbox"), 0);
    assert_eq!(encoder_max_b_frames("hevc_videotoolbox"), 0);
    assert_eq!(encoder_max_b_frames("h264_nvenc"), 2);
    assert_eq!(encoder_max_b_frames("mpeg4"), 2);
    assert!((1..=64).contains(&codec_thread_budget()));
    for concurrency in [1, 2, 4] {
        for hardware_decode in [false, true] {
            let allocation = codec_thread_allocation(CodecThreadRequest {
                decode_concurrency: concurrency,
                hardware_decode,
                source_frame_pixels: 3840 * 2160,
                software_video_encoder: true,
                encoder_fixed_threads: None,
            });
            assert!(allocation.decode_concurrency <= concurrency);
            assert!(allocation.decoder_threads >= 1);
            assert!(allocation.encoder_threads >= 1);
            let assigned = usize::try_from(allocation.decoder_threads)
                .expect("decoder threads fit")
                .saturating_mul(allocation.decode_concurrency)
                .saturating_add(
                    usize::try_from(allocation.encoder_threads).expect("encoder threads fit"),
                );
            assert_eq!(assigned, allocation.allocated_threads);
            // A session falling back from hardware keeps the even share; the hardware session
            // itself opens with the fixed count.
            let contexts = allocation.decode_concurrency + 1;
            let even_share = i64::try_from(allocation.total_threads / contexts).expect("fits");
            if hardware_decode {
                assert_eq!(allocation.decoder_threads, even_share.max(1));
            } else {
                assert!(allocation.decoder_threads <= even_share.max(1));
            }
            assert_eq!(
                allocation.hardware_decoder_threads,
                i64::try_from(HARDWARE_DECODER_THREADS.min(allocation.total_threads))
                    .expect("fits")
                    .max(1)
            );
            assert!(allocation.allocated_threads <= allocation.total_threads);
        }
    }
    assert_eq!(
        unused_option_names(Some(video_decoder_options(1))).as_deref(),
        Some("threads, flags")
    );

    let misspelled = AVDictionary::new(c"threadz", c"auto", 0);
    assert!(matches!(
        reject_unused_decode_options(Some(misspelled)),
        Err(DecodeError::Adapter(message)) if message.contains("threadz")
    ));
}

#[test]
fn an_encode_sink_reports_each_encoder_and_muxer_call_to_its_thread_watch() {
    // The host judges a worker stuck by its current call's duration, so a flush must beat per call.
    let path = output_path("watched-calls");
    let _ = fs::remove_file(&path);
    let mut sink = NativeVideoEncodeSink::create(
        &path,
        VideoEncodeConfig {
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
    )
    .expect("encoder");
    let cancel = CancelToken::default();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let counted = std::rc::Rc::clone(&calls);
    let watch = watch_encode_calls(move || counted.set(counted.get() + 1));
    sink.write(
        EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [255, 0, 0, 255])),
        &cancel,
    )
    .expect("frame");
    let after_frame = calls.get();
    assert!(after_frame >= 2, "{after_frame} calls for one frame");
    sink.finish(&cancel).expect("trailer");
    assert!(
        calls.get() >= after_frame + 4,
        "{} calls to finish",
        calls.get() - after_frame
    );
    drop(watch);
    let before = calls.get();
    crate::encode::beat_encode_call();
    assert_eq!(calls.get(), before);
    fs::remove_file(path).expect("remove output");
}

#[test]
fn video_encode_mux_round_trip_and_cleanup_are_transactional() {
    let path = output_path("round-trip");
    let _ = fs::remove_file(&path);
    let mut sink = NativeVideoEncodeSink::create(
        &path,
        VideoEncodeConfig {
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
    )
    .expect("encoder");
    let cancel = CancelToken::default();
    sink.write(
        EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [255, 0, 0, 255])),
        &cancel,
    )
    .expect("first frame");
    sink.write(
        EncodeEvent::Video(rgba_frame(MediaTime::from_ticks(4_000), [0, 255, 0, 255])),
        &cancel,
    )
    .expect("second frame");
    sink.finish(&cancel).expect("trailer");
    let result = probe(&path).expect("probe encoded file");
    assert_eq!(result.selected_video, Some(0));
    let video = result
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video)
        .expect("video metadata");
    assert_eq!(video.frame_rate, Some(FrameRate::new(30, 1).expect("rate")));
    assert_eq!(video.color, Some(ColorMetadata::default()));
    assert_eq!(video.duration, Some(MediaTime::from_ticks(8_000)));
    let mut decoder = NativeDecoder::open(&path).expect("decode encoded file");
    let first_frame = match decoder.decode_next(&cancel).expect("decoded frame") {
        DecodeEvent::Video(frame) => frame,
        event => panic!("unexpected {event:?}"),
    };
    assert_eq!(first_frame.pixel_format, PixelFormat::Yuv(YuvFormat::I420));
    assert_eq!(first_frame.planes.len(), 3);
    let y = first_frame.planes[0].bytes[0];
    let u = first_frame.planes[1].bytes[0];
    let v = first_frame.planes[2].bytes[0];
    assert!(y > 30 && y < 130 && u < 160 && v > 170);
    let seeked = decoder
        .seek(
            DecodeRequest {
                source: alpha2_media::SourceId::new("round-trip").expect("source ID"),
                time: MediaTime::from_ticks(4_000),
                kind: alpha2_media::DecodeRequestKind::Seek,
                generation: 1,
            },
            &cancel,
        )
        .expect("exact seek");
    assert!(
        matches!(seeked.event, DecodeEvent::Video(frame) if frame.pts == MediaTime::from_ticks(4_000))
    );
    assert_final_frame_seek_boundaries(&mut decoder, &cancel);
    // Windows cannot delete a file the decoder still has open.
    drop(decoder);
    fs::remove_file(path).expect("remove fixture");

    let cancelled_path = output_path("cancelled");
    let _ = fs::remove_file(&cancelled_path);
    let mut cancelled = NativeVideoEncodeSink::create(
        &cancelled_path,
        VideoEncodeConfig {
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
    )
    .expect("encoder");
    let token = CancelToken::default();
    token.cancel();
    assert_eq!(
        cancelled.write(
            EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [0, 0, 0, 255])),
            &token,
        ),
        Err(EncodeError::Cancelled)
    );
    assert!(!cancelled_path.exists());
}

#[derive(Debug)]
enum EncodeStage {
    Open,
    Frame,
    Trailer,
}

/// Opens `capability`'s encoder at `rate_control`, writes one 256x144 frame and the
/// trailer, and removes the file.
fn encode_one_frame(
    capability: &crate::NativeVideoEncoderCapability,
    rate_control: VideoRateControl,
    label: &str,
) -> Result<(), (EncodeStage, EncodeError)> {
    let path = output_path(&format!("{label}-{}", capability.encoder_name));
    let _ = fs::remove_file(&path);
    let result = (|| {
        let mut sink = NativeVideoEncodeSink::create_with_encoder(
            &path,
            VideoEncodeConfig {
                width: 256,
                height: 144,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                color: ColorMetadata::default(),
                rate_control,
                codec: capability.codec,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            &capability.encoder_name,
        )
        .map_err(|error| (EncodeStage::Open, error))?;
        let cancel = CancelToken::default();
        sink.write(
            EncodeEvent::Video(rgba_frame_sized(
                MediaTime::ZERO,
                [40, 80, 160, 255],
                256,
                144,
            )),
            &cancel,
        )
        .map_err(|error| (EncodeStage::Frame, error))?;
        sink.finish(&cancel)
            .map_err(|error| (EncodeStage::Trailer, error))
    })();
    let _ = fs::remove_file(&path);
    result
}

#[test]
fn a_constant_rate_opens_on_every_advertised_encoder() {
    // A constant request adds a mode switch the encoder must consume, or it would run variable
    // while reporting constant. A hardware refusal is reported, not failed. Registered encoders may
    // not run here at all, so only one that encodes a variable-rate frame is held to constant.
    for capability in codec_capabilities().video {
        // `ProRes` profiles fix their rate, so there is no bitrate mode to test.
        if capability.codec == NativeVideoCodec::ProRes {
            continue;
        }
        let name = &capability.encoder_name;
        let variable = VideoRateControl::bitrate(500_000, BitrateMode::Variable).expect("rate");
        if let Err((stage, error)) = encode_one_frame(&capability, variable, "variable") {
            let Some(needs) = super::encoder_capability(name) else {
                panic!(
                    "the software encoder {name} does not encode (variable rate, {stage:?}: {error:?})"
                );
            };
            alpha2_test_support::skip!(
                needs,
                "{name} does not encode on this machine at all (variable rate, {stage:?}: {error:?})"
            );
            continue;
        }
        let constant = VideoRateControl::bitrate(500_000, BitrateMode::Constant).expect("rate");
        match encode_one_frame(&capability, constant, "constant") {
            Ok(()) => {}
            Err((EncodeStage::Open, EncodeError::Adapter(message))) => {
                assert!(
                    !message.contains("options-ignored") && !message.contains("did not consume"),
                    "{name} ignored its constant-rate options: {message}"
                );
                assert!(
                    capability.hardware_accelerated,
                    "software encoder {name} encodes variable but refused a constant rate: {message}"
                );
                eprintln!("{name} refused a constant rate: {message}");
            }
            Err((stage, error)) => {
                panic!("{name} encodes variable but failed a constant rate at {stage:?}: {error:?}")
            }
        }
    }
}

#[test]
fn probe_reports_every_rate_control_mechanism_per_encoder() {
    // The probe opens `FFmpeg`'s Vulkan devices for its hardware encoders.
    let _device = vulkan_device_lock();
    // The D3D12VA rows walk the DX12 adapters; this binary encodes no frame for them.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    crate::hwencode::encode_no_rate_control_frames();
    let probes = probe_video_encoders(256, 144);
    for probe in &probes {
        let mechanisms = probe
            .rate_control
            .iter()
            .map(|rate| rate.mechanism)
            .collect::<Vec<_>>();
        assert_eq!(
            mechanisms,
            RateControlMechanism::ALL,
            "{}",
            probe.encoder_name
        );
    }
    // libavcodec's `mpeg4` is in every runtime, so it must read the same everywhere: both bitrate
    // modes open, and constant quantizer has no spelling.
    let mpeg4 = probes
        .iter()
        .find(|probe| probe.encoder_name == "mpeg4")
        .expect("mpeg4 is registered");
    for rate in &mpeg4.rate_control {
        if rate.mechanism == RateControlMechanism::ConstantQuantizer {
            assert_eq!(rate.options, None, "mpeg4 must not claim a quantizer");
            assert!(rate.outcome.is_err());
        } else {
            assert!(
                rate.outcome.is_ok(),
                "mpeg4 {}: {:?} with {:?}",
                rate.mechanism.as_str(),
                rate.outcome,
                rate.options
            );
        }
    }
}

#[test]
fn encoders_that_do_not_read_a_qp_are_sent_no_quantizer() {
    // `MediaFoundation` reads `global_quality` as raw QP, so quantizer levels fall back to bitrate;
    // `VideoToolbox` quality is mapped for H.264 only.
    for name in ["hevc_videotoolbox", "h264_mf", "hevc_mf", "mpeg4"] {
        assert_eq!(
            crate::rate_control::mechanism_request(
                name,
                RateControlMechanism::ConstantQuantizer,
                10_000_000,
                23,
            ),
            None,
            "{name}"
        );
    }
    for name in ["h264_vulkan", "h264_nvenc", "hevc_d3d12va"] {
        assert!(
            crate::rate_control::mechanism_request(
                name,
                RateControlMechanism::ConstantQuantizer,
                10_000_000,
                23,
            )
            .is_some(),
            "{name}"
        );
    }
}

#[test]
fn videotoolbox_quality_maps_levels_monotonically() {
    use crate::rate_control::videotoolbox_quality;

    let levels = [
        alpha2_media::QualityLevel::VeryLow,
        alpha2_media::QualityLevel::Low,
        alpha2_media::QualityLevel::Medium,
        alpha2_media::QualityLevel::High,
        alpha2_media::QualityLevel::VeryHigh,
    ]
    .map(|level| {
        let quantizer = alpha2_media::level_quantizer(alpha2_media::VideoCodec::H264, level)
            .expect("H.264 levels have a quantizer");
        videotoolbox_quality(quantizer)
    });
    assert!(
        levels.windows(2).all(|pair| pair[0] < pair[1]),
        "{levels:?}"
    );
    let all = (0..=51).map(videotoolbox_quality).collect::<Vec<_>>();
    assert!(all.windows(2).all(|pair| pair[0] >= pair[1]), "{all:?}");
    assert!(
        all.iter().all(|quality| (0.01..=1.0).contains(quality)),
        "{all:?}"
    );
    let request = crate::rate_control::mechanism_request(
        "h264_videotoolbox",
        RateControlMechanism::ConstantQuantizer,
        10_000_000,
        27,
    )
    .expect("h264_videotoolbox has a quality spelling");
    let hundredths = (videotoolbox_quality(27) * 100.0).round();
    assert_eq!(
        request.describe(),
        format!("flags=+qscale global_quality={}", hundredths * 118.0)
    );
    assert_eq!(request.bit_rate, 0);
    assert_eq!(
        crate::rate_control::applied_of_quantizer_request("h264_videotoolbox").as_str(),
        "quality"
    );
}

#[test]
fn x264_and_x265_are_sent_a_qp_not_a_crf() {
    for name in ["libx264", "libx265"] {
        let request = crate::rate_control::mechanism_request(
            name,
            RateControlMechanism::ConstantQuantizer,
            10_000_000,
            27,
        )
        .expect("x26x has a constant QP");
        assert_eq!(request.describe(), "qp=27", "{name}");
    }
}

#[test]
fn d3d12va_encoders_are_sent_rc_mode_in_ffmpegs_spelling() {
    // `FFmpeg`'s D3D12VA `rc_mode` constants (`CQP`, `CBR`, `VBR`, `QVBR`) are
    // case-sensitive; `cbr` fails the open.
    let request = |name, mechanism| {
        crate::rate_control::mechanism_request(name, mechanism, 10_000_000, 23)
            .unwrap_or_else(|| panic!("{name} has a spelling for {mechanism:?}"))
            .describe()
    };
    for name in ["h264_d3d12va", "hevc_d3d12va", "av1_d3d12va"] {
        assert_eq!(
            request(name, RateControlMechanism::ConstantBitrate),
            "b=10000000 maxrate=10000000 minrate=10000000 bufsize=10000000 rc_mode=CBR",
            "{name}"
        );
        assert_eq!(
            request(name, RateControlMechanism::VariableBitrate),
            "b=10000000 rc_mode=VBR",
            "{name}"
        );
        assert_eq!(
            request(name, RateControlMechanism::ConstantQuantizer),
            "rc_mode=CQP qp=23",
            "{name}"
        );
        let constant = VideoRateControl::bitrate(10_000_000, BitrateMode::Constant).unwrap();
        let options = crate::rate_control::rate_control_options(
            name,
            constant,
            AVDictionary::new(c"threads", c"0", 0),
        );
        assert_eq!(
            options
                .get(c"rc_mode", None, 0)
                .map(|entry| entry.value().to_owned()),
            Some(c"CBR".to_owned()),
            "{name}"
        );
        // Devices that refuse a constant rate are told what to ask for instead; variable
        // rate is not blamed on it.
        assert!(
            crate::rate_control::constant_rate_hint(name, constant).contains("quantizer"),
            "{name}"
        );
        assert_eq!(
            crate::rate_control::constant_rate_hint(
                name,
                VideoRateControl::bitrate(10_000_000, BitrateMode::Variable).unwrap()
            ),
            "",
            "{name}"
        );
    }
}

#[test]
fn a_runtime_other_than_the_pinned_sdk_says_why() {
    let identity = |version: &str, configuration: &str| crate::FfmpegRuntimeIdentity {
        version: version.to_owned(),
        configuration: configuration.to_owned(),
        pinned_sdk_release: "ffmpeg-8.1.3-shared-build5",
    };
    let pinned = "--disable-gpl --disable-nonfree --disable-autodetect --enable-libdav1d \
                  --enable-libopus --enable-libvpx --enable-libsvtav1 --enable-videotoolbox";
    assert_eq!(identity("n8.1.3", pinned).pinned_sdk_mismatch(), None);
    // The same version without libdav1d.
    let without_dav1d = identity("n8.1.3", &pinned.replace(" --enable-libdav1d", ""))
        .pinned_sdk_mismatch()
        .expect("an SDK without libdav1d is not the pin");
    assert_eq!(without_dav1d, "not configured with --enable-libdav1d");
    // An earlier SDK build without software VP9, AV1 or Opus encoders.
    let earlier = identity(
        "n8.1.3",
        "--disable-gpl --disable-nonfree --disable-autodetect --enable-libdav1d",
    )
    .pinned_sdk_mismatch()
    .expect("an SDK without the software encoders is not the pin");
    assert_eq!(
        earlier,
        "not configured with --enable-libopus, not configured with --enable-libsvtav1, \
         not configured with --enable-libvpx"
    );
    let homebrew = identity(
        "8.1.2",
        "--prefix=/opt/homebrew --enable-gpl --enable-libx264",
    )
    .pinned_sdk_mismatch()
    .expect("Homebrew's FFmpeg is not the pin");
    assert!(homebrew.contains("8.1.2"), "{homebrew}");
    assert!(homebrew.contains("--enable-gpl"), "{homebrew}");
    assert!(homebrew.contains("--enable-libx264"), "{homebrew}");
    assert!(homebrew.contains("--disable-autodetect"), "{homebrew}");
    // The pinned version built elsewhere: an LGPL system package that found an external encoder.
    let system = identity("n8.1.3", "--disable-gpl --enable-libaom")
        .pinned_sdk_mismatch()
        .expect("a system build of the same version is not the pin");
    assert!(system.contains("--enable-libaom"), "{system}");
    assert!(system.contains("--disable-autodetect"), "{system}");
    assert!(!system.contains("version"), "{system}");
    let version3 = identity("n8.1.3", &format!("{pinned} --enable-version3"))
        .pinned_sdk_mismatch()
        .expect("version 3 code is not the pin");
    assert_eq!(version3, "configured with --enable-version3");
}

#[test]
fn these_tests_run_on_the_pinned_sdk() {
    // Helpers call this before most tests reach `FFmpeg`; this catches a run
    // whose tests all skip them.
    crate::require_pinned_ffmpeg_for_tests();
}

#[test]
fn a_quantizer_the_encoder_cannot_hold_falls_back_to_the_bitrate_and_says_so() {
    // MPEG-4 Part 2 is everywhere and has no drivable quantizer, so the bitrate fallback is
    // reachable on every machine.
    let path = output_path("quantizer-fallback-mpeg4");
    let _ = fs::remove_file(&path);
    let mut sink = NativeVideoEncodeSink::create(
        &path,
        VideoEncodeConfig {
            width: 64,
            height: 48,
            frame_rate: FrameRate::new(30, 1).expect("rate"),
            color: ColorMetadata::default(),
            rate_control: VideoRateControl {
                bitrate: 500_000,
                mode: BitrateMode::Variable,
                quantizer: Some(4),
            },
            codec: NativeVideoCodec::Mpeg4Part2,
            display: crate::VideoDisplay::default(),
            container: alpha2_media::Container::Mp4,
            pixels: alpha2_media::PixelLayout::YUV420P,
        },
    )
    .expect("encoder");
    assert_eq!(
        sink.selection().rate_control,
        crate::AppliedRateControl::Bitrate
    );
    assert!(
        sink.selection()
            .quantizer_refused
            .as_deref()
            .is_some_and(|reason| reason.contains("mpeg4")),
        "{:?}",
        sink.selection().quantizer_refused
    );
    let cancel = CancelToken::default();
    sink.write(
        EncodeEvent::Video(rgba_frame_sized(
            MediaTime::ZERO,
            [40, 80, 160, 255],
            64,
            48,
        )),
        &cancel,
    )
    .expect("frame");
    sink.finish(&cancel).expect("trailer");
    fs::remove_file(&path).expect("remove output");
    assert!(
        crate::rate_control::quantizer_request(
            "mpeg4",
            VideoRateControl::bitrate(500_000, BitrateMode::Variable).unwrap()
        )
        .is_none(),
        "no quantizer asked for is no quantizer request"
    );
}

#[test]
fn the_encoders_take_every_yuv_layout_the_decoders_give() {
    for name in ["hdr-pq.mp4", "prores-pq-opaque.mov", "prores-pq-alpha.mov"] {
        let mut decoder = NativeDecoder::open(corpus(name)).expect("decoder");
        let frame = first_frame(&mut decoder, name);
        let PixelFormat::Yuv(format) = frame.pixel_format else {
            panic!("{name} decodes to {:?}", frame.pixel_format);
        };
        assert_ne!(format, YuvFormat::I420, "{name}");
        let (width, height) = (frame.visible_rect.width, frame.visible_rect.height);
        let path = output_path(&format!("transcode-{name}"));
        let _ = fs::remove_file(&path);
        let mut sink = NativeVideoEncodeSink::create(
            &path,
            VideoEncodeConfig {
                width,
                height,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                // The sink writes SDR only; what is tested is the frame it takes.
                color: ColorMetadata::default(),
                rate_control: VideoRateControl::bitrate(500_000, BitrateMode::Variable).unwrap(),
                codec: NativeVideoCodec::Mpeg4Part2,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
        )
        .expect("sink");
        let cancel = CancelToken::default();
        sink.write(EncodeEvent::Video(frame.clone()), &cancel)
            .unwrap_or_else(|error| panic!("{name} ({format:?}): {error}"));
        sink.finish(&cancel).expect("finish");
        let mut encoded = NativeDecoder::open(&path).expect("encoded");
        let back = first_frame(&mut encoded, "encoded");
        let source = row16(&frame.planes[0], height / 2);
        let shift = u32::from(format.bit_depth) - 8;
        let expected = source.iter().map(|code| i32::from(*code >> shift));
        let row = height / 2;
        let actual =
            &back.planes[0].bytes[(row * back.planes[0].stride) as usize..][..width as usize];
        let worst = expected
            .zip(actual)
            .map(|(expected, actual)| (expected - i32::from(*actual)).abs())
            .max()
            .unwrap_or(0);
        assert!(worst <= 8, "{name}: luma off by {worst}");
        // Windows cannot delete a file `FFmpeg` still holds open.
        drop(encoded);
        fs::remove_file(path).expect("cleanup");
    }
}

#[cfg(all(feature = "hwencode", any(target_os = "linux", target_os = "macos")))]
#[test]
fn a_hardware_session_refuses_a_rate_it_cannot_state() {
    let rate = FrameRate::new(4_294_967_291, 143_165_577).expect("about 30 fps");
    let error = crate::hwencode::session_frame_rate(rate).expect_err("past i32");
    assert!(error.to_string().contains("out of range"), "{error}");
    assert_eq!(
        crate::hwencode::session_frame_rate(FrameRate::new(30_000, 1_001).expect("rate")),
        Ok((30_000, 1_001))
    );
}

/// A 4:2:0 output tagged RGB is refused (it would be written with BT.709 under an identity tag),
/// and each refusal names the one failing condition.
#[test]
fn an_encoder_config_is_refused_for_the_reason_that_fails() {
    let config = av_config(48_000).video;
    let refused = |config: VideoEncodeConfig| {
        crate::video_encode::check_video_config(&config)
            .expect_err("refused")
            .to_string()
    };
    let rgb = VideoEncodeConfig {
        color: ColorMetadata {
            matrix: alpha2_media::ColorMatrix::Rgb,
            ..ColorMetadata::default()
        },
        ..config
    };
    assert!(refused(rgb).contains("RGB"));
    assert!(
        refused(VideoEncodeConfig {
            width: 15,
            ..config
        })
        .contains("15x16")
    );
    assert!(
        refused(VideoEncodeConfig {
            color: PQ_BT2020,
            ..config
        })
        .contains("HDR")
    );
    crate::video_encode::check_video_config(&config).expect("an SDR Y'CbCr output");
}

fn named_candidates(names: &[&std::ffi::CStr]) -> Vec<crate::capabilities::VideoEncoderChoice> {
    names
        .iter()
        .map(|name| {
            crate::capabilities::VideoEncoderChoice::Ffmpeg(
                crate::capabilities::VideoEncoderCandidate {
                    encoder: AVCodec::find_encoder_by_name(name).expect("the SDK registers it"),
                    on_hardware: false,
                },
            )
        })
        .collect()
}

/// MPEG-4 Part 2 refuses time base denominators past 16 bits (119.88, 239.76 fps, VFR averages).
/// Such rates get the finest unit it can hold; other rates and codecs keep the frame length.
#[test]
fn the_codecs_that_store_16_bit_time_bases_are_limited() {
    use crate::capabilities::{encoder_time_base, max_time_base_denominator};
    let limit = max_time_base_denominator(ffi::AV_CODEC_ID_MPEG4);
    assert_eq!(limit, Some(65_535));
    assert_eq!(max_time_base_denominator(ffi::AV_CODEC_ID_H264), None);
    let base =
        encoder_time_base(FrameRate::new(120_000, 1_001).expect("rate"), limit).expect("time base");
    assert_eq!((base.num, base.den), (1, 64_000));
}

#[test]
fn both_mp4_sinks_write_mpeg4_at_119_88_fps() {
    let rate = FrameRate::new(120_000, 1_001).expect("rate");
    let mut config = av_config(48_000);
    config.video.frame_rate = rate;
    let cancel = CancelToken::default();
    for kind in ["video", "av"] {
        let path = output_path(&format!("rate-119.88-{kind}"));
        let _ = fs::remove_file(&path);
        let mut sink: Box<dyn EncodeSink> = match kind {
            "video" => Box::new(NativeVideoEncodeSink::create(&path, config.video).expect(kind)),
            _ => Box::new(NativeAvEncodeSink::create(&path, config).expect(kind)),
        };
        let slot = |index: i64| MediaTime::from_ticks(index * 1_001);
        for index in 0..6 {
            sink.write(
                EncodeEvent::Video(rgba_frame(slot(index), [255, 0, 0, 255])),
                &cancel,
            )
            .expect("frame");
        }
        sink.finish(&cancel).expect("trailer");
        drop(sink);
        let mut decoder = NativeDecoder::open(&path).expect("output");
        let mut times = Vec::new();
        while let DecodeEvent::Video(frame) = decoder.decode_next(&cancel).expect("decode") {
            times.push(frame.pts.ticks());
        }
        drop(decoder);
        assert_eq!(times.len(), 6, "{kind}");
        for (index, ticks) in (0_i64..).zip(&times) {
            assert!(
                ticks.abs_diff(index * 1_001) <= 2,
                "{kind} frame {index}: {ticks}"
            );
        }
        let video = probe(&path)
            .expect("probe")
            .selected_video_stream()
            .expect("video")
            .clone();
        let length = video.duration.expect("duration").ticks();
        assert!(
            length.abs_diff(6 * 1_001) <= 2,
            "{kind}: {length} ticks long"
        );
        fs::remove_file(path).expect("remove output");
    }
}

/// The fallback through an export's candidates. PNG and BMP take only RGB, so they stand in for
/// encoders that do not open here and are passed over for their pixel format.
#[test]
fn encoder_candidates_fall_through_in_order_and_say_why() {
    let config = av_config(48_000).video;
    let request = crate::capabilities::VideoEncoderOpenRequest {
        config: &config,
        requested_name: None,
        threads: crate::capabilities::SoftwareEncoderThreads::new(&|_| 1),
        global_header: true,
        width: 16,
        height: 16,
    };
    let (_, selection) = crate::capabilities::open_first_video_encoder(
        &request,
        named_candidates(&[c"png", c"bmp", c"mpeg4"]),
    )
    .expect("the third opens");
    assert_eq!(selection.encoder_name, "mpeg4");
    assert_eq!(
        selection
            .rejected
            .iter()
            .map(|rejected| (rejected.encoder_name.as_str(), rejected.reason))
            .collect::<Vec<_>>(),
        [
            (
                "png",
                crate::capabilities::NativeEncoderRejection::PixelFormat
            ),
            (
                "bmp",
                crate::capabilities::NativeEncoderRejection::PixelFormat
            ),
        ]
    );
    let error = crate::capabilities::open_first_video_encoder(
        &request,
        named_candidates(&[c"png", c"bmp"]),
    )
    .err()
    .expect("none opens")
    .to_string();
    assert!(
        error.contains("after png") && error.contains("bmp"),
        "{error}"
    );
}

/// A registered hardware encoder is advertised only once it has encoded a frame. H.263
/// refuses a 256x144 picture, standing in for a GPU encoder that cannot run here;
/// MPEG-4 Part 2 for one that can.
#[test]
fn a_hardware_encoder_is_advertised_only_once_it_has_encoded_a_frame() {
    let stand_in = |name: &std::ffi::CStr| crate::capabilities::VideoEncoderCandidate {
        encoder: AVCodec::find_encoder_by_name(name).expect("the SDK registers it"),
        on_hardware: true,
    };
    assert!(!encoder_is_compatible(&stand_in(c"h263")));
    assert!(encoder_is_compatible(&stand_in(c"mpeg4")));
}

#[test]
fn a_videotoolbox_encoder_is_probed_whatever_class_its_runtime_declares() {
    use crate::capabilities::needs_open_probe;
    for on_hardware in [false, true] {
        for name in ["h264_videotoolbox", "hevc_videotoolbox", "h264_mf"] {
            assert!(needs_open_probe(on_hardware, name), "{name} {on_hardware}");
        }
    }
    assert!(!needs_open_probe(false, "libx264"));
    assert!(!needs_open_probe(false, "mpeg4"));
}

/// The software thread count is decided only when a software candidate opens, so a software
/// fallback after a failed hardware open gets a software share. PNG stands in for a GPU encoder
/// that does not open, MPEG-4 Part 2 flagged as hardware for one that does.
#[test]
fn a_software_encoder_reached_after_a_hardware_one_opens_with_the_software_share() {
    let config = av_config(48_000).video;
    let asked = std::cell::Cell::new(0_u32);
    let decide = |_| {
        asked.set(asked.get() + 1);
        3
    };
    let request = || crate::capabilities::VideoEncoderOpenRequest {
        config: &config,
        requested_name: None,
        threads: crate::capabilities::SoftwareEncoderThreads::new(&decide),
        global_header: true,
        width: 64,
        height: 48,
    };
    let candidate = |name: &std::ffi::CStr, on_hardware| {
        crate::capabilities::VideoEncoderChoice::Ffmpeg(
            crate::capabilities::VideoEncoderCandidate {
                encoder: AVCodec::find_encoder_by_name(name).expect("the SDK registers it"),
                on_hardware,
            },
        )
    };
    let (encoder, selection) = crate::capabilities::open_first_video_encoder(
        &request(),
        vec![candidate(c"png", true), candidate(c"mpeg4", false)],
    )
    .expect("the software one opens");
    assert_eq!(
        (
            selection.encoder_name.as_str(),
            selection.hardware_accelerated
        ),
        ("mpeg4", false)
    );
    assert_eq!(selection.rejected.len(), 1);
    assert_eq!(selection.software_threads, Some(3));
    let crate::video_backend::VideoEncoderBackend::Ffmpeg(encoder) = encoder else {
        panic!("MPEG-4 Part 2 is an FFmpeg encoder");
    };
    assert_eq!(encoder.thread_count, 3, "the encoder runs what was decided");
    assert_eq!(asked.get(), 1);

    asked.set(0);
    let (_, selection) = crate::capabilities::open_first_video_encoder(
        &request(),
        vec![candidate(c"mpeg4", true), candidate(c"mpeg4", false)],
    )
    .expect("the hardware one opens");
    assert!(selection.hardware_accelerated);
    assert_eq!(selection.software_threads, None);
    assert_eq!(
        asked.get(),
        0,
        "a hardware encoder decides no software split"
    );
}

fn log_line(class: &str, text: &str) -> alpha2_ffmpeg_bridge::LogLine {
    alpha2_ffmpeg_bridge::LogLine {
        seq: 0,
        level: 16,
        class: class.to_owned(),
        text: text.to_owned(),
    }
}

/// A consumer GPU past its session limit answers `NV_ENC_ERR_OUT_OF_MEMORY` or
/// `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY`, which `FFmpeg` logs as
/// `<call> failed: <words> (<status>): <details>`.
#[test]
fn nvenc_session_limit_status_is_busy() {
    use crate::capabilities::classify_open_failure;
    for text in [
        "OpenEncodeSessionEx failed: out of memory (10): (no details)",
        "OpenEncodeSessionEx failed: incompatible client key (21): (no details)",
        "OpenEncodeSessionEx failed: encoder busy (18): (no details)",
    ] {
        // `FFmpeg` logs this after any failed device, a busy one included.
        assert_eq!(
            classify_open_failure(
                "h264_nvenc",
                "Generic error in an external library",
                &[
                    log_line("h264_nvenc", text),
                    log_line("h264_nvenc", "No capable devices found")
                ]
            ),
            OpenFailureKind::Busy,
            "{text}"
        );
    }
}

#[test]
fn nvenc_without_a_device_is_absent() {
    use crate::capabilities::classify_open_failure;
    assert_eq!(
        classify_open_failure(
            "hevc_nvenc",
            "Generic error in an external library",
            &[log_line("hevc_nvenc", "No capable devices found")]
        ),
        OpenFailureKind::Absent
    );
    assert_eq!(
        classify_open_failure(
            "h264_nvenc",
            "Generic error in an external library",
            &[log_line(
                "h264_nvenc",
                "OpenEncodeSessionEx failed: no encode device (1): (no details)"
            )]
        ),
        OpenFailureKind::Absent
    );
    // A status nothing names stays unknown, never busy.
    assert_eq!(
        classify_open_failure(
            "h264_nvenc",
            "Invalid argument",
            &[log_line(
                "h264_nvenc",
                "InitializeEncoder failed: invalid param (8): bad preset"
            )]
        ),
        OpenFailureKind::Unknown
    );
}

#[test]
fn vulkan_session_out_of_memory_is_busy() {
    use crate::capabilities::classify_open_failure;
    for result in ["VK_ERROR_OUT_OF_DEVICE_MEMORY", "VK_ERROR_TOO_MANY_OBJECTS"] {
        assert_eq!(
            classify_open_failure(
                "h264_vulkan",
                "Cannot allocate memory",
                &[log_line(
                    "h264_vulkan",
                    &format!("Unable to create video session: {result}")
                )]
            ),
            OpenFailureKind::Busy,
            "{result}"
        );
    }
    assert_eq!(
        classify_open_failure(
            "h264_vulkan",
            "Cannot allocate memory",
            &[log_line(
                "h264_vulkan",
                "Unable to create video session: VK_ERROR_INITIALIZATION_FAILED"
            )]
        ),
        OpenFailureKind::Unknown
    );
    assert_eq!(
        classify_open_failure(
            "h264_videotoolbox",
            "Error: cannot create compression session: -12915",
            &[]
        ),
        OpenFailureKind::Busy
    );
}

#[test]
fn a_busy_probe_is_not_cached() {
    use crate::capabilities::{ProbeCache, cached_probe};
    let cache = ProbeCache::default();
    let busy = cached_probe(
        &cache,
        "h264_nvenc",
        true,
        Some(|| Err((OpenFailureKind::Busy, "out of memory (10)".to_owned()))),
    );
    assert!(
        matches!(&busy, Some(Err(refusal)) if refusal.kind == OpenFailureKind::Busy),
        "{busy:?}"
    );
    let probed = std::cell::Cell::new(false);
    let runs = cached_probe(
        &cache,
        "h264_nvenc",
        true,
        Some(|| {
            probed.set(true);
            Ok(())
        }),
    );
    assert!(probed.get(), "the busy answer was cached");
    assert_eq!(runs, Some(Ok(())));
    // Running is cached: a third ask does not probe.
    let kept = cached_probe(
        &cache,
        "h264_nvenc",
        true,
        Some(|| -> Result<(), (OpenFailureKind, String)> { panic!("probed a third time") }),
    );
    assert_eq!(kept, Some(Ok(())));
}

#[test]
fn an_absent_probe_is_cached() {
    use crate::capabilities::{ProbeCache, cached_probe};
    let cache = ProbeCache::default();
    let absent = cached_probe(
        &cache,
        "av1_mf",
        false,
        Some(|| {
            Err((
                OpenFailureKind::Absent,
                "could not find any MFT for the given media type".to_owned(),
            ))
        }),
    );
    assert!(matches!(&absent, Some(Err(refusal)) if refusal.kind == OpenFailureKind::Absent));
    let again = cached_probe(
        &cache,
        "av1_mf",
        false,
        Some(|| -> Result<(), (OpenFailureKind, String)> { panic!("probed again") }),
    );
    let Some(Err(refusal)) = again else {
        panic!("{again:?}");
    };
    assert_eq!(refusal.kind, OpenFailureKind::Absent);
    assert!(refusal.reason.contains("MFT"), "{}", refusal.reason);
    // Without a probe to run, a cached verdict still answers and an unknown encoder does not.
    assert!(
        cached_probe(
            &cache,
            "av1_mf",
            false,
            None::<fn() -> Result<(), (OpenFailureKind, String)>>
        )
        .is_some()
    );
    assert!(
        cached_probe(
            &cache,
            "hevc_mf",
            false,
            None::<fn() -> Result<(), (OpenFailureKind, String)>>
        )
        .is_none()
    );
}

#[test]
fn media_foundation_e_fail_on_input_is_absent() {
    use crate::capabilities::classify_open_failure;
    assert_eq!(
        classify_open_failure(
            "hevc_mf",
            "Send frame to a codec context failed. (-542398533)",
            &[log_line("hevc_mf", "failed processing input: 80004005")]
        ),
        OpenFailureKind::Absent
    );
}

#[test]
fn openh264_threads_are_reserved_in_the_split() {
    use crate::convert::codec_thread_allocation_within;

    let fixed = usize::from(crate::openh264::thread_count(crate::openh264::slice_count(
        1080,
    )));
    assert_eq!(fixed, 4);
    let request = |encoder_fixed_threads| CodecThreadRequest {
        decode_concurrency: 2,
        hardware_decode: false,
        source_frame_pixels: 3840 * 2160,
        software_video_encoder: true,
        encoder_fixed_threads,
    };
    let fixed_split = codec_thread_allocation_within(request(Some(fixed)), 22);
    assert_eq!(fixed_split.encoder_threads, 4);
    // 4K decode wants five workers a session; the encoder's fixed four leave room for them.
    assert_eq!(fixed_split.decoder_threads, 5);
    assert_eq!(fixed_split.allocated_threads, 2 * 5 + 4);
    // Without the reservation the encoder would get a remainder it never uses.
    let remainder = codec_thread_allocation_within(request(None), 22);
    assert_eq!(remainder.encoder_threads, 12);
    // A budget below the encoder's count still gives each decoder one worker.
    let tight = codec_thread_allocation_within(request(Some(8)), 6);
    assert_eq!(tight.encoder_threads, 8);
    assert_eq!(tight.decoder_threads, 1);
    // Decoding in hardware, the fallback's share is what the encoder leaves.
    let hardware = codec_thread_allocation_within(
        CodecThreadRequest {
            hardware_decode: true,
            ..request(Some(fixed))
        },
        22,
    );
    assert_eq!(hardware.encoder_threads, 4);
    assert_eq!(hardware.decoder_threads, 9);
}

/// The budget leaves two threads on 16+, one on 6-15, none below six; the encoder floor is four or
/// a third of a small budget, so on 8 threads two 4K software decoders get two workers each.
#[test]
fn the_reserve_scales_with_the_budget() {
    use crate::convert::{codec_thread_allocation_within, codec_thread_budget_for};

    assert_eq!(codec_thread_budget_for(24), 22);
    assert_eq!(codec_thread_budget_for(16), 14);
    assert_eq!(codec_thread_budget_for(8), 7);
    assert_eq!(codec_thread_budget_for(6), 5);
    assert_eq!(codec_thread_budget_for(4), 4);
    assert_eq!(codec_thread_budget_for(1), 1);
    let request = CodecThreadRequest {
        decode_concurrency: 2,
        hardware_decode: false,
        source_frame_pixels: 3840 * 2160,
        software_video_encoder: true,
        encoder_fixed_threads: None,
    };
    let eight_threads = codec_thread_allocation_within(request, codec_thread_budget_for(8));
    assert_eq!(
        (eight_threads.decoder_threads, eight_threads.encoder_threads),
        (2, 3),
        "{eight_threads:?}"
    );
    // One 4K source on 8 threads: the decoder is held to the even share, the encoder keeps four.
    let single = codec_thread_allocation_within(
        CodecThreadRequest {
            decode_concurrency: 1,
            ..request
        },
        codec_thread_budget_for(8),
    );
    assert_eq!(
        (single.decoder_threads, single.encoder_threads),
        (3, 4),
        "{single:?}"
    );
    let workstation = codec_thread_allocation_within(request, codec_thread_budget_for(24));
    assert_eq!(
        (workstation.decoder_threads, workstation.encoder_threads),
        (5, 12),
        "{workstation:?}"
    );
}
