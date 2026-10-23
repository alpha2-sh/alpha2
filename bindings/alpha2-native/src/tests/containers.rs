//! Exports into every container the policy offers, and the software VP9, AV1
//! and Opus encoders the pinned SDK links for them.

use alpha2_media::{AudioCodec, Container};

use super::*;

/// Whether the loaded runtime is the pinned SDK, which links `libvpx-vp9`,
/// `libsvtav1` and `libopus`.
fn pinned() -> bool {
    crate::ffmpeg_runtime_identity()
        .pinned_sdk_mismatch()
        .is_none()
}

/// Whether this run hides the encoder `name` (`ALPHA2_TEST_SKIP_ENCODERS`, as the valgrind run
/// hides `libsvtav1`): a check of it is left out.
fn skipped(name: &str) -> bool {
    crate::capabilities::test_skips_encoder(&std::ffi::CString::new(name).expect("name"))
}

#[test]
fn the_pinned_sdk_registers_software_vp9_av1_and_opus() {
    crate::require_pinned_ffmpeg_for_tests();
    if !pinned() {
        return;
    }
    let capabilities = codec_capabilities();
    for (codec, name) in [
        (NativeVideoCodec::Vp9, "libvpx-vp9"),
        (NativeVideoCodec::Av1, "libsvtav1"),
    ] {
        if skipped(name) {
            continue;
        }
        let entry = capabilities
            .video
            .iter()
            .find(|capability| capability.encoder_name == name)
            .unwrap_or_else(|| panic!("{name} is advertised"));
        assert_eq!(entry.codec, codec);
        assert_eq!(entry.class, crate::NativeEncoderClass::Software);
        assert!(!entry.hardware_accelerated);
    }
    let audio = capabilities
        .audio
        .iter()
        .map(|capability| (capability.codec, capability.encoder_name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        audio,
        [
            (NativeAudioCodec::Aac, "aac"),
            (NativeAudioCodec::Opus, "libopus")
        ]
    );
    // FFmpeg's own `opus` encoder is experimental: never the one opened.
    let opus = crate::capabilities::audio_encoder(NativeAudioCodec::Opus).expect("libopus");
    assert_eq!(opus.name().to_str(), Ok("libopus"));
}

#[test]
fn every_policy_pair_matches_the_muxer() {
    crate::require_pinned_ffmpeg_for_tests();
    let video_id = |codec: alpha2_media::VideoCodec| match codec {
        alpha2_media::VideoCodec::H264 => ffi::AV_CODEC_ID_H264,
        alpha2_media::VideoCodec::Hevc => ffi::AV_CODEC_ID_HEVC,
        alpha2_media::VideoCodec::Vp9 => ffi::AV_CODEC_ID_VP9,
        alpha2_media::VideoCodec::Av1 => ffi::AV_CODEC_ID_AV1,
        alpha2_media::VideoCodec::Mpeg4Part2 => ffi::AV_CODEC_ID_MPEG4,
    };
    let audio_id = |codec: AudioCodec| match codec {
        AudioCodec::Aac => ffi::AV_CODEC_ID_AAC,
        AudioCodec::Opus => ffi::AV_CODEC_ID_OPUS,
    };
    for container in Container::ALL {
        let muxer = crate::convert::muxer_name(container);
        for codec in container.video_codecs() {
            assert_eq!(
                alpha2_ffmpeg_bridge::muxer_accepts_codec(muxer, video_id(*codec)),
                Some(true),
                "{container} {}",
                codec.as_str()
            );
        }
        for codec in container.audio_codecs() {
            assert_eq!(
                alpha2_ffmpeg_bridge::muxer_accepts_codec(muxer, audio_id(*codec)),
                Some(true),
                "{container} {}",
                codec.as_str()
            );
        }
    }
    // The query can refuse a pair: the WebM muxer takes no H.264.
    assert_eq!(
        alpha2_ffmpeg_bridge::muxer_accepts_codec(c"webm", ffi::AV_CODEC_ID_H264),
        Some(false)
    );
}

/// One export case: a container, its codecs, and the timing it runs at.
struct Case {
    container: Container,
    video: NativeVideoCodec,
    audio: NativeAudioCodec,
    frame_rate: FrameRate,
    audio_rate: u32,
}

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

/// A frame whose colour changes with `index`, so every frame is coded.
fn moving_frame(pts: MediaTime, index: u64) -> DecodedVideoFrame {
    let shade = u8::try_from(index * 4 % 256).expect("byte");
    rgba_frame_sized(pts, [shade, 255 - shade, 96, 255], WIDTH, HEIGHT)
}

/// Exports two seconds of `case` into `path`, and returns the frames and
/// the project-rate audio samples it wrote.
fn export_case(path: &Path, case: &Case) -> (u64, u64) {
    let _ = fs::remove_file(path);
    let rate = case.frame_rate;
    let frames = 2 * u64::from(rate.numerator()) / u64::from(rate.denominator());
    let samples = 2 * u64::from(case.audio_rate);
    let mut sink = NativeAvEncodeSink::create(
        path,
        AvEncodeConfig {
            video: VideoEncodeConfig {
                width: WIDTH,
                height: HEIGHT,
                frame_rate: rate,
                color: ColorMetadata::EXPORT,
                rate_control: VideoRateControl::bitrate(1_000_000, BitrateMode::Variable)
                    .expect("rate"),
                codec: case.video,
                display: crate::VideoDisplay::default(),
                container: case.container,
            },
            audio_sample_rate: case.audio_rate,
            audio_channels: 2,
            audio_codec: case.audio,
            audio_bit_rate: 128_000,
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{} {}+{}: {error}",
            case.container,
            case.video.as_str(),
            case.audio.as_str()
        )
    });
    let cancel = CancelToken::default();
    let frame_ticks = |index: u64| {
        MediaTime::from_ticks(
            i64::try_from(
                u128::from(index)
                    * u128::from(rate.denominator())
                    * u128::try_from(alpha2_time::TICKS_PER_SECOND).expect("ticks")
                    / u128::from(rate.numerator()),
            )
            .expect("ticks"),
        )
    };
    let block = u64::from(case.audio_rate / 10);
    let mut audio_written = 0_u64;
    for index in 0..frames {
        let pts = frame_ticks(index);
        // Audio up to this frame's time, in tenth-second blocks, ahead of the frame.
        while audio_written < samples
            && i128::from(audio_written) * i128::from(alpha2_time::TICKS_PER_SECOND)
                <= i128::from(pts.ticks()) * i128::from(case.audio_rate)
        {
            let count = block.min(samples - audio_written);
            sink.write(
                EncodeEvent::Audio(tone_block(
                    i64::try_from(audio_written).expect("small"),
                    usize::try_from(count).expect("small"),
                    case.audio_rate,
                )),
                &cancel,
            )
            .expect("audio");
            audio_written += count;
        }
        sink.write(EncodeEvent::Video(moving_frame(pts, index)), &cancel)
            .expect("video");
    }
    while audio_written < samples {
        let count = block.min(samples - audio_written);
        sink.write(
            EncodeEvent::Audio(tone_block(
                i64::try_from(audio_written).expect("small"),
                usize::try_from(count).expect("small"),
                case.audio_rate,
            )),
            &cancel,
        )
        .expect("audio");
        audio_written += count;
    }
    sink.finish(&cancel).expect("finish");
    (frames, samples)
}

/// The EBML `DocType` a Matroska file names in its header.
fn doc_type(path: &Path) -> Option<&'static str> {
    let bytes = fs::read(path).expect("read output");
    let header = &bytes[..bytes.len().min(64)];
    if !header.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return None;
    }
    let names = |name: &[u8]| header.windows(name.len()).any(|window| window == name);
    if names(b"webm") {
        Some("webm")
    } else if names(b"matroska") {
        Some("matroska")
    } else {
        None
    }
}

/// Checks what `case`'s export at `path` reads back as through alpha2's own
/// probe and demuxer.
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn check_case(path: &Path, case: &Case, frames: u64, samples: u64) {
    let label = format!(
        "{} {}+{}",
        case.container,
        case.video.as_str(),
        case.audio.as_str()
    );
    let probed = probe(path).expect("probe");
    let matroska = matches!(case.container, Container::Mkv | Container::Webm);
    if matroska {
        assert!(
            probed.format.contains("matroska"),
            "{label}: {}",
            probed.format
        );
        let expected = if case.container == Container::Webm {
            "webm"
        } else {
            "matroska"
        };
        assert_eq!(doc_type(path), Some(expected), "{label}");
    } else {
        assert!(probed.format.contains("mp4"), "{label}: {}", probed.format);
        let bytes = fs::read(path).expect("read output");
        let brand = &bytes[8..12];
        if case.container == Container::Mov {
            assert_eq!(brand, b"qt  ", "{label}");
        } else {
            assert_ne!(brand, b"qt  ", "{label}");
        }
    }
    let video = probed.selected_video_stream().expect("video");
    // The probe names the decoder it opens: AV1 is decoded by libdav1d.
    let video_name = match case.video {
        NativeVideoCodec::Mpeg4Part2 => "mpeg4",
        NativeVideoCodec::Av1 => "libdav1d",
        other => other.as_str(),
    };
    assert_eq!(video.codec, video_name, "{label}");
    let audio = probed.selected_audio_stream().expect("audio");
    assert_eq!(audio.codec, case.audio.as_str(), "{label}");
    assert_eq!(audio.channels, Some(2), "{label}");
    let (video_index, audio_index) = (video.index, audio.index);

    let mut input = crate::decode::open_input(path).expect("input");
    let video_base = input.streams()[video_index].time_base;
    let audio_base = input.streams()[audio_index].time_base;
    let mut video_pts = Vec::new();
    let mut audio_packets = Vec::new();
    while let Some(packet) = input.read_packet().expect("packet") {
        let index = usize::try_from(packet.stream_index).expect("index");
        if index == video_index {
            video_pts.push(packet.pts);
        } else if index == audio_index {
            audio_packets.push((packet.pts, packet.duration));
        }
    }
    // Every frame, each in its own slot of the frame grid: exactly in MP4 and MOV, and to the
    // millisecond a Matroska block timestamp is rounded to otherwise.
    assert_eq!(
        u64::try_from(video_pts.len()).expect("count"),
        frames,
        "{label}"
    );
    video_pts.sort_unstable();
    let rate = case.frame_rate;
    for (index, pts) in video_pts.iter().enumerate() {
        let seconds = *pts as f64 * f64::from(video_base.num) / f64::from(video_base.den);
        let slot = index as f64 * f64::from(rate.denominator()) / f64::from(rate.numerator());
        let tolerance = if matroska { 0.000_5 + 1e-9 } else { 1e-9 };
        assert!(
            (seconds - slot).abs() <= tolerance,
            "{label}: frame {index} at {seconds} s, slot {slot} s"
        );
    }
    // The audio starts within one encoder frame of zero, and lasts what was written, give or take
    // the encoder's padding.
    let to_seconds =
        |value: i64| value as f64 * f64::from(audio_base.num) / f64::from(audio_base.den);
    let first = audio_packets
        .iter()
        .map(|(pts, _)| *pts)
        .min()
        .expect("audio");
    let end = audio_packets
        .iter()
        .map(|(pts, duration)| pts + duration)
        .max()
        .expect("audio");
    let encoder_frame = if case.audio == NativeAudioCodec::Opus {
        0.020
    } else {
        1_024.0 / f64::from(case.audio_rate)
    };
    assert!(
        to_seconds(first).abs() <= encoder_frame + 0.001,
        "{label}: audio starts at {} s",
        to_seconds(first)
    );
    let written = samples as f64 / f64::from(case.audio_rate);
    let lasted = to_seconds(end) - to_seconds(first).max(0.0);
    assert!(
        (lasted - written).abs() <= 2.0 * encoder_frame + 0.001,
        "{label}: audio lasts {lasted} s of {written} s"
    );
    // And the first frame decodes.
    let mut decoder = NativeDecoder::open(path).expect("decoder");
    let first_video = first_frame(&mut decoder, &label);
    assert_eq!(first_video.pts, MediaTime::ZERO, "{label}");
}

#[test]
fn exports_every_container_codec_pair_and_reads_it_back() {
    crate::require_pinned_ffmpeg_for_tests();
    let capabilities = codec_capabilities();
    // The software sink feeds CPU frames, so only an encoder that takes them stands in for a
    // codec; on the pinned Linux SDK that leaves H.264 and HEVC without one.
    let encodes_video = |codec: alpha2_media::VideoCodec| {
        capabilities
            .video
            .iter()
            .find(|capability| capability.codec.model() == codec)
            .map(|capability| capability.codec)
    };
    let encodes_audio = |codec: AudioCodec| {
        capabilities
            .audio
            .iter()
            .find(|capability| capability.codec.model() == codec)
            .map(|capability| capability.codec)
    };
    let rates = [
        FrameRate::new(30_000, 1_001).expect("rate"),
        FrameRate::new(25, 1).expect("rate"),
    ];
    let mut cases = Vec::new();
    for container in Container::ALL {
        let audio_codecs = container
            .audio_codecs()
            .iter()
            .filter_map(|codec| encodes_audio(*codec))
            .collect::<Vec<_>>();
        let video_codecs = container
            .video_codecs()
            .iter()
            .filter_map(|codec| encodes_video(*codec))
            .collect::<Vec<_>>();
        let (Some(first_video), Some(first_audio)) = (video_codecs.first(), audio_codecs.first())
        else {
            eprintln!("skipped {container}: this runtime encodes none of its codecs");
            continue;
        };
        // Each offered video codec beside the container's first audio one, and each offered
        // audio codec beside its first video one, alternating the frame and sample rates.
        let pairs = video_codecs
            .iter()
            .map(|video| (*video, *first_audio))
            .chain(
                audio_codecs
                    .iter()
                    .skip(1)
                    .map(|audio| (*first_video, *audio)),
            );
        for (video, audio) in pairs {
            let index = cases.len();
            cases.push(Case {
                container,
                video,
                audio,
                frame_rate: rates[index % 2],
                audio_rate: if index % 2 == 0 { 48_000 } else { 44_100 },
            });
        }
    }
    if pinned() {
        // The pinned SDK reaches every container, WebM included.
        for container in Container::ALL {
            assert!(
                cases.iter().any(|case| case.container == container),
                "{container}"
            );
        }
    }
    for case in &cases {
        let path = output_path(&format!(
            "pair-{}-{}-{}.{}",
            case.container,
            case.video.as_str(),
            case.audio.as_str(),
            case.container.extensions()[0]
        ));
        let (frames, samples) = export_case(&path, case);
        check_case(&path, case, frames, samples);
        fs::remove_file(&path).expect("remove output");
    }
    let verified = cases
        .iter()
        .map(|case| {
            format!(
                "{} {}+{}",
                case.container,
                case.video.as_str(),
                case.audio.as_str()
            )
        })
        .collect::<Vec<_>>();
    eprintln!("verified exports: {}", verified.join(", "));
}

#[test]
fn a_codec_the_container_does_not_offer_is_refused_before_any_file() {
    let path = output_path("refused.webm");
    let _ = fs::remove_file(&path);
    let mut config = av_config(48_000);
    config.video.container = Container::Webm;
    let error = NativeAvEncodeSink::create(&path, config)
        .err()
        .expect("mpeg4 is not offered in webm")
        .to_string();
    assert!(
        error.contains("webm does not offer video codec mpeg4-part2"),
        "{error}"
    );
    let mut config = av_config(48_000);
    config.video.container = Container::Mov;
    config.audio_codec = NativeAudioCodec::Opus;
    let error = NativeAvEncodeSink::create(&path, config)
        .err()
        .expect("opus is not offered in mov")
        .to_string();
    assert!(
        error.contains("mov does not offer audio codec opus"),
        "{error}"
    );
    assert!(!path.exists());
    assert!(!partial_beside(&path).exists());
}

#[test]
fn matroska_writes_no_mp4_codec_tag() {
    let tag = |bytes: &[u8; 4]| u32::from_le_bytes(*bytes);
    for container in [Container::Mkv, Container::Webm] {
        for copied in [false, true] {
            assert_eq!(
                crate::convert::container_codec_tag(
                    container,
                    ffi::AV_CODEC_ID_HEVC,
                    tag(b"hvc1"),
                    copied
                ),
                0
            );
        }
    }
    assert_eq!(
        crate::convert::container_codec_tag(Container::Mov, ffi::AV_CODEC_ID_HEVC, 0, false),
        tag(b"hvc1")
    );
    // An encoded stream whose encoder set an MP4 tag still writes into Matroska.
    let path = output_path("tagged.mkv");
    let _ = fs::remove_file(&path);
    let mut config = av_config(48_000);
    config.video.container = Container::Mkv;
    let mut sink = NativeAvEncodeSink::create(&path, config).expect("mkv sink");
    let cancel = CancelToken::default();
    sink.write(
        EncodeEvent::Video(rgba_frame(MediaTime::ZERO, [10, 200, 30, 255])),
        &cancel,
    )
    .expect("frame");
    sink.finish(&cancel).expect("finish");
    assert_eq!(doc_type(&path), Some("matroska"));
    assert_eq!(
        probe(&path)
            .expect("probe")
            .selected_video_stream()
            .expect("video")
            .codec,
        "mpeg4"
    );
    fs::remove_file(&path).expect("remove");
}

/// The software VP9 and AV1 encoders the pinned SDK links, by name.
const SOFTWARE_MODERN: [&str; 2] = ["libvpx-vp9", "libsvtav1"];

#[test]
fn software_vp9_and_av1_open_at_every_mechanism() {
    crate::require_pinned_ffmpeg_for_tests();
    if !pinned() {
        return;
    }
    // The probe opens FFmpeg's Vulkan devices for its hardware encoders.
    let _device = vulkan_device_lock();
    // The D3D12VA rows walk the DX12 adapters; this binary encodes no frame for them.
    #[cfg(all(target_os = "windows", feature = "hwencode"))]
    crate::hwencode::encode_no_rate_control_frames();
    let probes = probe_video_encoders(640, 360);
    for name in SOFTWARE_MODERN.into_iter().filter(|name| !skipped(name)) {
        let probe = probes
            .iter()
            .find(|probe| probe.encoder_name == name)
            .unwrap_or_else(|| panic!("{name} is probed"));
        assert!(probe.advertised, "{name}");
        assert_eq!(probe.software_open_error, None, "{name}");
        for rate in &probe.rate_control {
            assert!(
                rate.outcome.is_ok(),
                "{name} {} with {:?}: {:?}",
                rate.mechanism.as_str(),
                rate.options,
                rate.outcome
            );
        }
    }
}

/// Encodes ten frames with `name` at `threads` workers and returns the
/// packets' bytes.
fn encode_ten(name: &str, threads: i64) -> Vec<u8> {
    let encoder = rsmpeg::avcodec::AVCodec::find_encoder_by_name(
        &std::ffi::CString::new(name).expect("name"),
    )
    .expect("encoder");
    let mut context = AVCodecContext::new(&encoder);
    context.set_width(320);
    context.set_height(180);
    context.set_pix_fmt(ffi::AV_PIX_FMT_YUV420P);
    context.set_time_base(ra(1, 30));
    context.set_framerate(ra(30, 1));
    context.set_bit_rate(500_000);
    context.set_gop_size(60);
    let options =
        crate::convert::video_encoder_options(ColorMetadata::EXPORT, &encoder, false, threads, 320);
    let unused = alpha2_ffmpeg_bridge::open_codec_context(&mut context, options).expect("open");
    crate::convert::reject_unused_encode_options(unused).expect("every option consumed");
    let mut bytes = Vec::new();
    let mut drain = |context: &mut AVCodecContext| {
        while let Ok(packet) = context.receive_packet() {
            bytes.extend(alpha2_ffmpeg_bridge::packet_bytes(&packet));
        }
    };
    for index in 0..10_i64 {
        let mut frame = alpha2_ffmpeg_bridge::Yuv420pFrameBuffer::new(320, 180).expect("frame");
        {
            let planes = frame.begin_write().expect("planes");
            let shade = u8::try_from(index * 20).expect("byte");
            planes.y.fill(shade);
            planes.u.fill(128);
            planes.v.fill(255 - shade);
        }
        frame.set_presentation(index, ra(1, 30));
        context.send_frame(Some(frame.frame())).expect("frame");
        drain(&mut context);
    }
    context.send_frame(None).expect("flush");
    drain(&mut context);
    bytes
}

#[test]
fn software_vp9_and_av1_are_byte_reproducible() {
    // libvpx-vp9 writes the same bytes at any thread count. SVT-AV1 does at parallelism level 1,
    // which a one-worker budget gives it and `ALPHA2_DETERMINISTIC_ENCODE=1` forces: measured at
    // 1080p, two encodes at levels 3 and 5 differed every time, idle or loaded.
    crate::require_pinned_ffmpeg_for_tests();
    if !pinned() {
        return;
    }
    for (name, threads) in [("libvpx-vp9", 1), ("libvpx-vp9", 8), ("libsvtav1", 1)] {
        if skipped(name) {
            continue;
        }
        let first = encode_ten(name, threads);
        assert!(!first.is_empty(), "{name}");
        assert_eq!(
            first,
            encode_ten(name, threads),
            "{name} at {threads} threads is not reproducible"
        );
    }
}

#[test]
fn software_encoder_profiles_follow_the_frame_and_the_budget() {
    assert_eq!(crate::convert::vp9_tile_columns_log2(320), 0);
    assert_eq!(crate::convert::vp9_tile_columns_log2(640), 1);
    assert_eq!(crate::convert::vp9_tile_columns_log2(1_920), 2);
    assert_eq!(crate::convert::vp9_tile_columns_log2(3_840), 3);
    assert_eq!(crate::convert::vp9_tile_columns_log2(16_384), 6);
    let levels = [1, 2, 3, 5, 6, 11, 12, 23, 24, 64].map(crate::convert::svt_av1_parallelism);
    assert_eq!(levels, [1, 2, 3, 3, 4, 4, 5, 5, 6, 6]);
    // A constant rate joins the parallelism level in the one list SVT-AV1 parses.
    let options = rsmpeg::avutil::AVDictionary::new(c"svtav1-params", c"lp=4", 0);
    let constant = crate::rate_control::rate_control_options(
        "libsvtav1",
        VideoRateControl::bitrate(1_000_000, BitrateMode::Constant).expect("rate"),
        options,
    );
    assert_eq!(
        constant
            .get(c"svtav1-params", None, 0)
            .map(|entry| entry.value().to_owned()),
        Some(c"lp=4:rc=2:pred-struct=1".to_owned())
    );
    assert!(constant.get(c"maxrate", None, 0).is_none());
}

/// A copy plan of the corpus H.264 stream's 60 frames into `container`,
/// with its audio offered.
fn copy_into(container: Container) -> Result<crate::PacketCopyPlan, String> {
    let source = corpus("h264-bt709-copy.mp4");
    let frame_times = (0..60)
        .map(|frame| MediaTime::from_ticks(frame * alpha2_time::TICKS_PER_SECOND / 30))
        .collect::<Vec<_>>();
    crate::plan_packet_copy(&crate::PacketCopyRequest {
        source: &source,
        frame_times: &frame_times,
        width: 128,
        height: 96,
        codec: NativeVideoCodec::H264,
        color: ColorMetadata::EXPORT,
        output_start: MediaTime::ZERO,
        audio: Some(crate::PacketCopyAudio {
            sample_rate: 48_000,
            channels: 2,
        }),
        container,
    })
}

/// The video packets of `path`'s selected video stream, in file order.
fn video_packets(path: &Path) -> Vec<Vec<u8>> {
    let index = probe(path).expect("probe").selected_video.expect("video");
    let mut input = crate::decode::open_input(path).expect("input");
    let mut packets = Vec::new();
    while let Some(packet) = input.read_packet().expect("packet") {
        if usize::try_from(packet.stream_index).ok() == Some(index) {
            packets.push(alpha2_ffmpeg_bridge::packet_bytes(&packet));
        }
    }
    packets
}

#[test]
fn a_copy_into_webm_of_h264_is_ineligible() {
    let reason = copy_into(Container::Webm).expect_err("webm offers no H.264");
    assert_eq!(reason, "the source's h264 is not offered in webm");
}

#[test]
fn a_copy_into_matroska_or_mov_keeps_every_packet() {
    let source = corpus("h264-bt709-copy.mp4");
    let source_packets = video_packets(&source);
    for container in [Container::Mkv, Container::Mov] {
        let plan = copy_into(container).expect("copied");
        assert!(plan.copies_audio(), "{:?}", plan.audio_copy_refusal());
        let copy = crate::PacketCopy::join(vec![plan]).expect("one run");
        let output = output_path(&format!("copy.{}", container.extensions()[0]));
        let _ = fs::remove_file(&output);
        let report = crate::copy_packets(
            &copy,
            &output,
            container,
            crate::CopyAudio::Source,
            &CancelToken::default(),
            &mut |_| {},
        )
        .expect("copy");
        assert_eq!(report.video_packets, 60, "{container}");
        let probed = probe(&output).expect("probe");
        assert_eq!(probed.selected_video_stream().expect("video").codec, "h264");
        assert_eq!(probed.selected_audio_stream().expect("audio").codec, "aac");
        if container == Container::Mkv {
            assert_eq!(doc_type(&output), Some("matroska"));
        }
        assert_eq!(video_packets(&output), source_packets, "{container}");
        fs::remove_file(&output).expect("remove");
    }
}

#[test]
fn a_turned_picture_reads_back_turned_from_every_container() {
    crate::require_pinned_ffmpeg_for_tests();
    for container in Container::ALL {
        let codec = if container == Container::Webm {
            NativeVideoCodec::Vp9
        } else {
            NativeVideoCodec::Mpeg4Part2
        };
        if !codec_capabilities()
            .video
            .iter()
            .any(|capability| capability.codec == codec)
        {
            continue;
        }
        let path = output_path(&format!("turned.{}", container.extensions()[0]));
        let _ = fs::remove_file(&path);
        let orientation = alpha2_media::Orientation {
            rotation: alpha2_media::Rotation::Deg90,
            mirror: false,
        };
        let mut sink = NativeVideoEncodeSink::create(
            &path,
            VideoEncodeConfig {
                width: 64,
                height: 48,
                frame_rate: FrameRate::new(30, 1).expect("rate"),
                color: ColorMetadata::EXPORT,
                rate_control: VideoRateControl::bitrate(200_000, BitrateMode::Variable)
                    .expect("rate"),
                codec,
                display: crate::VideoDisplay {
                    orientation,
                    sample_aspect_ratio: alpha2_media::SampleAspectRatio::default(),
                },
                container,
            },
        )
        .expect("sink");
        let cancel = CancelToken::default();
        sink.write(
            EncodeEvent::Video(rgba_frame_sized(
                MediaTime::ZERO,
                [200, 20, 20, 255],
                64,
                48,
            )),
            &cancel,
        )
        .expect("frame");
        sink.finish(&cancel).expect("finish");
        let probed = probe(&path).expect("probe");
        assert_eq!(
            probed.selected_video_stream().expect("video").orientation,
            orientation,
            "{container}"
        );
        fs::remove_file(&path).expect("remove");
    }
}

#[test]
fn a_matroska_export_is_byte_reproducible() {
    crate::require_pinned_ffmpeg_for_tests();
    let capabilities = codec_capabilities();
    let has_video = |codec| capabilities.video.iter().any(|entry| entry.codec == codec);
    let has_audio = |codec| capabilities.audio.iter().any(|entry| entry.codec == codec);
    for case in [
        Case {
            container: Container::Mkv,
            video: NativeVideoCodec::Mpeg4Part2,
            audio: NativeAudioCodec::Aac,
            frame_rate: FrameRate::new(25, 1).expect("rate"),
            audio_rate: 48_000,
        },
        Case {
            container: Container::Webm,
            video: NativeVideoCodec::Vp9,
            audio: NativeAudioCodec::Opus,
            frame_rate: FrameRate::new(25, 1).expect("rate"),
            audio_rate: 48_000,
        },
    ] {
        if !has_video(case.video) || !has_audio(case.audio) {
            continue;
        }
        // Without bitexact flags matroskaenc gives each track and segment a random UID.
        let first = output_path(&format!("repeat-1.{}", case.container.extensions()[0]));
        let second = output_path(&format!("repeat-2.{}", case.container.extensions()[0]));
        export_case(&first, &case);
        export_case(&second, &case);
        assert_eq!(
            fs::read(&first).expect("first"),
            fs::read(&second).expect("second"),
            "{}",
            case.container
        );
        fs::remove_file(first).expect("remove");
        fs::remove_file(second).expect("remove");
    }
}
