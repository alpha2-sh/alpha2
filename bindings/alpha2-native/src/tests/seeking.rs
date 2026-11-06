//! Probe, seek, and stream selection: which frame a seek answers with and what a decoder reads.

use super::*;

/// A seek to a keyframe stored a few ticks after the time (30 fps in milliseconds) lands on it
/// rather than decoding the previous GOP.
#[test]
fn a_seek_lands_on_a_keyframe_stored_up_to_the_slack_after_its_time() {
    let path = corpus("frame-rounding-30-gop32.mkv");
    let presented = presented_ticks(&path);
    let mut decoder = NativeDecoder::open(&path).expect("decoder");
    let time = alpha2_time::FrameRate::new(30, 1)
        .expect("rate")
        .frame_start(32)
        .expect("frame start");
    assert!(presented[32] > time.ticks());
    let frames_decoded = || crate::decode::VIDEO_FRAMES_DECODED.with(std::cell::Cell::get);
    let before = frames_decoded();
    let result = decoder
        .seek(
            DecodeRequest {
                source: alpha2_media::SourceId::new("gop").expect("source ID"),
                time,
                kind: alpha2_media::DecodeRequestKind::Seek,
                generation: 1,
            },
            &CancelToken::default(),
        )
        .expect("seek");
    let DecodeEvent::Video(frame) = result.event else {
        panic!("video");
    };
    assert_eq!(frame.pts.ticks(), presented[32]);
    let work = frames_decoded() - before;
    assert!(
        work <= 2,
        "{work} frames decoded for a seek onto a keyframe"
    );
}

fn presented_ticks(path: &Path) -> Vec<i64> {
    let cancel = CancelToken::default();
    let mut decoder = NativeDecoder::open(path).expect("linear decoder");
    let mut presented = Vec::new();
    loop {
        match decoder.decode_next(&cancel).expect("linear decode") {
            DecodeEvent::Video(frame) => presented.push(frame.pts.ticks()),
            DecodeEvent::EndOfStream => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    presented
}

#[test]
fn the_pinned_sdk_decodes_av1_in_software() {
    // `FFmpeg`'s own AV1 decoder is hardware-only; the pinned SDK's
    // libdav1d lets software read AV1.
    let path = corpus("av1.mp4");
    let probed = probe(&path).expect("an AV1 stream probes");
    let video = probed
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video)
        .expect("video stream");
    assert_eq!(crate::software_decode_unsupported(video), None);
    let decoder = crate::decode::software_video_decoder(ffi::AV_CODEC_ID_AV1).expect("av1");
    assert_eq!(decoder.name().to_string_lossy(), "libdav1d");
    let mut decoder = NativeDecoder::open(&path).expect("a software session opens AV1");
    match decoder.decode_next(&CancelToken::default()) {
        Ok(DecodeEvent::Video(frame)) => assert_eq!(
            (frame.visible_rect.width, frame.visible_rect.height),
            (64, 48)
        ),
        other => panic!("decoded {other:?}"),
    }
    // A hardware session still finds `FFmpeg`'s own decoder, which alone
    // has hardware configurations.
    assert!(
        crate::decode::registered_video_decoders(ffi::AV_CODEC_ID_AV1)
            .any(|decoder| decoder.name().to_string_lossy() == "av1")
    );
}

#[test]
fn timestamp_conversion_is_exact_at_common_time_bases() {
    assert_eq!(
        timestamp(90_000, ra(1, 90_000)),
        Some(MediaTime::from_seconds(1))
    );
    assert_eq!(
        timestamp(1_001, ra(1, 30_000)),
        Some(MediaTime::from_ticks(4_004))
    );
}

#[test]
fn a_rounded_stream_tail_ends_at_the_container_duration() {
    let stream_end = MediaTime::from_ticks(309_613_304);
    let container_end = MediaTime::from_ticks(309_613_680);
    assert_eq!(
        effective_media_duration(Some(stream_end), Some(container_end)),
        Some(container_end)
    );
    assert_eq!(
        effective_media_duration(Some(stream_end), None),
        Some(stream_end)
    );
}

/// A transport stream whose duration `FFmpeg` estimates from its last raw packet ends at its last
/// parsed frame, as `ffprobe -show_packets` reports. A stated container duration is kept.
#[test]
fn a_transport_stream_ends_at_its_last_parsed_frame() {
    for (name, end) in [
        ("audio-mpegts.ts", 219_200),
        ("audio-late-start.ts", 1_481_593_600),
        ("h264-bt709-copy.ts", 416_000),
        ("h264-bt709-copy.mp4", 240_000),
    ] {
        let probed = probe(corpus(name)).expect("probe");
        let ended =
            probed.start.map_or(0, MediaTime::ticks) + probed.duration.expect("duration").ticks();
        assert_eq!(ended, end, "{name}");
    }
}

#[test]
fn checked_in_corpus_covers_probe_decode_seek_and_metadata() {
    for name in [
        "cfr-bt709-limited.mp4",
        "vfr.mp4",
        "long-gop.mp4",
        "edit-list.mp4",
        "rotation.mp4",
        "video-only.mp4",
        "odd-dimensions.avi",
        "bt601-full.mp4",
        "missing-color-metadata.mp4",
    ] {
        let result = probe(corpus(name)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert!(result.selected_video.is_some(), "{name}");
        let mut decoder =
            NativeDecoder::open(corpus(name)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert!(
            matches!(
                decoder.decode_next(&CancelToken::default()),
                Ok(DecodeEvent::Video(_))
            ),
            "{name}"
        );
    }
    // `-display_rotation 90` is counter-clockwise, so the frame is shown
    // turned a quarter clockwise.
    let rotated = probe(corpus("rotation.mp4")).expect("rotation probe");
    assert_eq!(
        rotated
            .selected_video_stream()
            .expect("video")
            .orientation
            .rotation,
        Rotation::Deg270
    );
    let odd = probe(corpus("odd-dimensions.avi")).expect("odd probe");
    assert_eq!(
        odd.selected_video_stream().expect("video").dimensions,
        Some((65, 49))
    );
    let bt601 = probe(corpus("bt601-full.mp4")).expect("601 probe");
    let color = bt601
        .selected_video_stream()
        .expect("video")
        .color
        .expect("color");
    assert_eq!(color.matrix, ColorMatrix::Bt601);
    assert_eq!(color.range, ColorRange::Full);
    let missing = probe(corpus("missing-color-metadata.mp4")).expect("fallback probe");
    assert!(
        missing
            .selected_video_stream()
            .expect("video")
            .color
            .is_none()
    );

    let mut cached = NativeDecoder::open(corpus("long-gop.mp4")).expect("cached decoder");
    assert!(cached.video_scaler.is_empty());
    let first = cached
        .decode_next(&CancelToken::default())
        .expect("decode direct YUV frame");
    assert!(matches!(
        first,
        DecodeEvent::Video(DecodedVideoFrame {
            pixel_format: PixelFormat::Yuv(YuvFormat::I420),
            ..
        })
    ));
    assert!(cached.video_scaler.is_empty());
    assert!(matches!(
        cached.decode_next(&CancelToken::default()),
        Ok(DecodeEvent::Video(DecodedVideoFrame {
            pixel_format: PixelFormat::Yuv(YuvFormat::I420),
            ..
        }))
    ));
    assert!(cached.video_scaler.is_empty());
}

#[test]
fn a_seek_that_keeps_decoding_frames_is_not_cut_off() {
    // On the budget's own clock, advanced only by the injected frame delay, so
    // slow builds do not matter.
    let limit = std::time::Duration::from_millis(100);
    crate::recovery::WORK_LIMIT.with(|cell| cell.set(limit));
    crate::recovery::FRAME_DELAY.with(|cell| cell.set(std::time::Duration::from_millis(10)));
    crate::recovery::VIRTUAL_CLOCK.with(|cell| cell.set(Some(std::time::Duration::ZERO)));
    let frames = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let counted = std::rc::Rc::clone(&frames);
    let watch = crate::watch_decode_progress(move || counted.set(counted.get() + 1));
    let mut decoder = NativeDecoder::open(corpus("long-gop.mp4")).expect("open");
    // Frame 28 of the first 30-frame GOP: 29 frames from its keyframe.
    let result = decoder.seek(seek_request(112_000), &CancelToken::default());
    let took = crate::recovery::VIRTUAL_CLOCK
        .with(std::cell::Cell::get)
        .expect("the clock");
    drop(watch);
    crate::recovery::WORK_LIMIT.with(|cell| cell.set(crate::DECODE_WORK_LIMIT));
    crate::recovery::FRAME_DELAY.with(|cell| cell.set(std::time::Duration::ZERO));
    crate::recovery::VIRTUAL_CLOCK.with(|cell| cell.set(None));
    result.expect("a seek that keeps producing frames");
    assert!(took > limit * 2, "the seek outlasted the limit: {took:?}");
    assert!(
        frames.get() >= 28,
        "progress reported {} times",
        frames.get()
    );
}

/// A probe names streams by `AVStream` index and lists only audio and video, so with a subtitle
/// first the selected video is stream 1 at position 0. An undecodable stream leaves its type
/// unselected rather than failing the probe, and cover art is not video.
#[test]
fn stream_selection_names_streams_by_index_and_skips_what_does_not_play() {
    let subtitled = probe(corpus("subtitle-first.mkv")).expect("probe");
    assert_eq!(
        (subtitled.selected_video, subtitled.selected_audio),
        (Some(1), Some(2))
    );
    assert_eq!(
        subtitled
            .selected_video_stream()
            .map(|stream| (stream.index, stream.kind)),
        Some((1, StreamKind::Video))
    );
    assert_eq!(
        subtitled
            .selected_audio_stream()
            .map(|stream| (stream.index, stream.kind)),
        Some((2, StreamKind::Audio))
    );

    let undecodable = probe(corpus("audio-undecodable.mp4")).expect("an undecodable track probes");
    assert_eq!(undecodable.selected_video, Some(0));
    assert_eq!(undecodable.selected_audio, None);
    assert_eq!(
        undecodable.streams.len(),
        2,
        "the AC-4 track is still listed"
    );
    NativeDecoder::open(corpus("audio-undecodable.mp4")).expect("its video opens");
    assert!(matches!(
        NativeAudioDecoder::open(corpus("audio-undecodable.mp4"), 48_000),
        Err(DecodeError::Unsupported(_))
    ));

    let song = probe(corpus("cover-art.m4a")).expect("probe");
    assert_eq!((song.selected_video, song.selected_audio), (None, Some(0)));
    assert!(matches!(
        NativeDecoder::open(corpus("cover-art.m4a")),
        Err(DecodeError::Unsupported(_))
    ));
    NativeAudioDecoder::open(corpus("cover-art.m4a"), 48_000).expect("its audio opens");
}

#[test]
fn a_decoder_has_the_demuxer_skip_the_streams_it_does_not_read() {
    let audio = NativeAudioDecoder::open(corpus("h264-bt709-copy.mp4"), 48_000).expect("audio");
    let video = NativeDecoder::open(corpus("h264-bt709-copy.mp4")).expect("video");
    let discards = |input: &alpha2_ffmpeg_bridge::DemuxInput| {
        input
            .streams()
            .iter()
            .map(|stream| stream.discard)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        discards(&audio.input),
        [ffi::AVDISCARD_ALL, ffi::AVDISCARD_DEFAULT]
    );
    assert_eq!(
        discards(&video.input),
        [ffi::AVDISCARD_DEFAULT, ffi::AVDISCARD_ALL]
    );
}

/// A raw ADTS stream past a rate change seeks by decoding from the start. Placed frames count as
/// progress and unread output is dropped, so neither the budget nor memory grows with distance.
/// With a 2,000-step budget, a 50 s seek stands for a 500 s one at the default.
#[test]
fn a_seek_past_a_raw_adts_rate_change_reaches_a_distant_target_holding_little() {
    let bytes = std::fs::read(corpus("audio-adts-rate-change.aac")).expect("fixture");
    let frame_length = |at: usize| {
        assert_eq!((bytes[at], bytes[at + 1] & 0xF0), (0xFF, 0xF0), "ADTS sync");
        (usize::from(bytes[at + 3] & 3) << 11)
            | (usize::from(bytes[at + 4]) << 3)
            | usize::from(bytes[at + 5] >> 5)
    };
    let rate_index = |at: usize| (bytes[at + 2] >> 2) & 0xF;
    let mut change = 0;
    while rate_index(change) == rate_index(0) {
        change += frame_length(change);
    }
    // The 48 kHz half's frames after its priming, repeated: 61 s at 48 kHz after 0.5 s at 44.1 kHz.
    let mut frames = Vec::new();
    let mut at = change;
    while at < bytes.len() {
        let length = frame_length(at);
        frames.push(&bytes[at..at + length]);
        at += length;
    }
    let mut long = bytes[..change].to_vec();
    for frame in frames[2..].iter().cycle().take(61 * 48_000 / 1_024) {
        long.extend_from_slice(frame);
    }
    let path = output_path("adts-rate-change-long").with_extension("aac");
    std::fs::write(&path, &long).expect("long fixture");
    crate::recovery::WORK_STEPS.with(|steps| steps.set(2_000));
    let cancel = CancelToken::default();
    let seeks = [10_i64, 50].map(|seconds| {
        let mut decoder = NativeAudioDecoder::open(&path, 48_000).expect("decoder");
        let seeked = decoder.seek(
            DecodeRequest {
                source: alpha2_media::SourceId::new("adts").expect("source ID"),
                time: MediaTime::from_ticks(seconds * alpha2_time::TICKS_PER_SECOND),
                kind: alpha2_media::DecodeRequestKind::Seek,
                generation: 1,
            },
            &cancel,
        );
        (seconds, seeked, decoder.peak_held_samples())
    });
    crate::recovery::WORK_STEPS.with(|steps| steps.set(65_536));
    std::fs::remove_file(&path).expect("remove");
    for (seconds, seeked, peak) in seeks {
        let event = seeked.expect("a seek reaches its target").event;
        let DecodeEvent::Audio(block) = event else {
            panic!("{seconds} s: {event:?}");
        };
        assert_eq!(block.start_sample, seconds * 48_000);
        assert!(block.channels[0].iter().any(|sample| sample.abs() > 0.05));
        assert!(peak < 8_192, "{seconds} s: {peak} samples held at once");
    }
}

/// Other streams' packets do not count against the decode budget, for demuxers that ignore the
/// discard; the test reads them on purpose.
#[test]
fn another_streams_packets_do_not_spend_the_decode_budget() {
    let mut video = NativeDecoder::open(corpus("video-short-audio-long.mkv")).expect("video");
    video.input.keep_only(&[0, 1]);
    crate::recovery::WORK_STEPS.with(|steps| steps.set(100));
    let cancel = CancelToken::default();
    let first = video.decode_next(&cancel);
    let end = video.decode_next(&cancel);
    crate::recovery::WORK_STEPS.with(|steps| steps.set(65_536));
    assert!(matches!(first, Ok(DecodeEvent::Video(_))), "{first:?}");
    assert!(matches!(end, Ok(DecodeEvent::EndOfStream)), "{end:?}");
}

/// Writes raw 8-bit grey frames into `path` (extension picks the muxer) at `timestamps` in
/// a 1/`time_base` time base.
fn write_raw_grey_video(path: &Path, width: i32, height: i32, time_base: i32, timestamps: &[i64]) {
    let codec = AVCodec::find_encoder(ffi::AV_CODEC_ID_RAWVIDEO).expect("rawvideo encoder");
    let mut context = AVCodecContext::new(&codec);
    context.set_width(width);
    context.set_height(height);
    context.set_pix_fmt(ffi::AV_PIX_FMT_GRAY8);
    context.set_time_base(ra(1, time_base));
    context.open(None).expect("open encoder");
    let name = std::ffi::CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
    let mut output = rsmpeg::avformat::AVFormatContextOutput::create(&name).expect("output");
    {
        let mut stream = output.new_stream();
        stream.set_codecpar(context.extract_codecpar());
        stream.set_time_base(ra(1, time_base));
    }
    output.write_header(&mut None).expect("header");
    let stream_base = output.streams()[0].time_base;
    let write = |context: &mut AVCodecContext,
                 output: &mut rsmpeg::avformat::AVFormatContextOutput| {
        while let Ok(mut packet) = context.receive_packet() {
            packet.rescale_ts(ra(1, time_base), stream_base);
            packet.set_stream_index(0);
            output.interleaved_write_frame(&mut packet).expect("packet");
        }
    };
    let columns = u32::try_from(width).expect("positive width");
    let rows = u32::try_from(height).expect("positive height");
    let grey = vec![128_u8; usize::try_from(columns * rows).expect("frame size")];
    for &pts in timestamps {
        let mut frame = alpha2_ffmpeg_bridge::planar_frame(
            ffi::AV_PIX_FMT_GRAY8,
            &[(&grey, columns)],
            columns,
            rows,
        )
        .expect("grey frame");
        frame.set_pts(pts);
        context.send_frame(Some(&frame)).expect("send frame");
        write(&mut context, &mut output);
    }
    context.send_frame(None).expect("flush");
    write(&mut context, &mut output);
    output.write_trailer().expect("trailer");
}

/// The presentation slack scan stops at 64 packets or 16 MiB. Eight exact 2 MiB frames come first,
/// then millisecond-rounded 30 fps frames, so the slack reflects only the time base.
#[test]
fn the_presentation_slack_reads_no_more_than_16_mib_of_first_packets() {
    let path = output_path("rounding-scan-bytes").with_extension("mov");
    let (width, height) = (2048, 1024);
    let frame_bytes = u64::try_from(width * height).expect("size");
    let exact = usize::try_from(alpha2_time::ROUNDING_SCAN_BYTES / frame_bytes).expect("frames");
    assert_eq!(exact, 8);
    let mut timestamps = (0..exact)
        .map(|index| i64::try_from(index).expect("index") * 640)
        .collect::<Vec<_>>();
    let mut last = timestamps[exact - 1];
    for step in [528, 544, 528, 544] {
        last += step;
        timestamps.push(last);
    }
    assert!(timestamps.len() < alpha2_time::ROUNDING_SCAN_FRAMES);
    write_raw_grey_video(&path, width, height, 16_000, &timestamps);
    let decoder = NativeDecoder::open(&path).expect("decoder");
    let slack = decoder.presentation_deadline(MediaTime::ZERO);
    let scanned = alpha2_time::presentation_slack(
        1,
        16_000,
        alpha2_time::rounding_quantum(&timestamps[..exact]),
    );
    let unbounded =
        alpha2_time::presentation_slack(1, 16_000, alpha2_time::rounding_quantum(&timestamps));
    assert_ne!(scanned, unbounded, "the frames past 16 MiB show a rounding");
    assert_eq!(slack, scanned, "the slack of the first 16 MiB");
    drop(decoder);
    let _ = fs::remove_file(&path);
}

#[test]
fn the_probe_reports_decoded_frame_bytes() {
    for (name, bytes_per_two_pixels) in [
        ("h264-1080p.mp4", 3),
        ("h264-high10.mp4", 6),
        ("hevc-main10-padded.mp4", 6),
    ] {
        let probed = probe(corpus(name)).expect("probe");
        let video = probed.selected_video_stream().expect("video");
        let (width, height) = video.dimensions.expect("size");
        assert_eq!(
            video.frame_bytes,
            Some(u64::from(width) * u64::from(height) * bytes_per_two_pixels / 2),
            "{name}"
        );
    }
    let audio = probe(corpus("audio-adts.aac")).expect("probe");
    assert!(
        audio
            .streams
            .iter()
            .all(|stream| stream.frame_bytes.is_none())
    );
}
