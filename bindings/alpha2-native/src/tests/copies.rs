//! Copies: when packets are copied instead of encoded, and the copy planner.

use super::*;

/// A copy keeps the source's codec parameters, display matrix and coded
/// pixel aspect included, where a render applies neither a mirror nor a
/// pixel aspect only the codec declares: such a source is not copied.
#[test]
fn a_copy_refuses_a_source_whose_carried_display_a_render_would_not_show() {
    let frame_times = (0..60)
        .map(|frame| MediaTime::from_ticks(frame * alpha2_time::TICKS_PER_SECOND / 30))
        .collect::<Vec<_>>();
    let plan = |name: &str| {
        let source = corpus(name);
        crate::plan_packet_copy(&crate::PacketCopyRequest {
            source: &source,
            frame_times: &frame_times,
            width: 128,
            height: 96,
            codec: NativeVideoCodec::H264,
            color: alpha2_media::ColorMetadata::EXPORT,
            output_start: MediaTime::ZERO,
            audio: None,
            container: alpha2_media::Container::Mp4,
        })
    };
    plan("h264-bt709-copy.mp4").expect("the stream these are made from is copied");
    for (name, expected) in [
        ("h264-bt709-hflip.mp4", "mirrors"),
        ("h264-bt709-vui-sar.mp4", "4:3 pixels"),
        ("h264-bt709-pasp.mp4", "4:3 pixels"),
    ] {
        let reason = plan(name).expect_err(name);
        assert!(reason.contains(expected), "{name}: {reason}");
    }
}

#[test]
fn a_yuv_transcode_takes_any_codec_whose_pictures_already_are_the_output() {
    use alpha2_media::{VideoCodec, YuvEncoding};
    let h264 = YuvEncoding::export(VideoCodec::H264);
    let vp9 = YuvEncoding::export(VideoCodec::Vp9);
    // Neither container nor codec matters: VP9 in WebM qualifies as H.264 in MP4 does, for an
    // output whose chroma sits where its does.
    for (name, width, height, output) in [
        ("h264-bt709-copy.mp4", 128, 96, h264),
        ("vp9-color.webm", 256, 64, vp9),
    ] {
        crate::check_yuv_transcode(&corpus(name), width, height, output)
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));
    }
    let opened = || crate::decode::INPUTS_OPENED.with(std::cell::Cell::get);
    let before = opened();
    crate::check_yuv_transcode(&corpus("h264-bt709-copy.mp4"), 128, 96, h264)
        .expect("the copy fixture qualifies");
    assert_eq!(opened() - before, 1, "inputs opened by one check");
    // An H.264 stream without VUI sites its chroma left, as an H.264 output does.
    crate::check_yuv_transcode(&corpus("h264-bt709-no-vui.mp4"), 128, 96, h264)
        .unwrap_or_else(|reason| panic!("h264-bt709-no-vui.mp4: {reason}"));
    for (name, width, height, output, expected) in [
        (
            "h264-bt709-copy.mp4",
            256,
            192,
            h264,
            "not the 256x192 output",
        ),
        ("h264-high10.mp4", 128, 96, h264, "tagged"),
        ("bt601-full.mp4", 64, 48, h264, "tagged"),
        // Copied as they are, centred chroma would land left of where an H.264 output reads it, and
        // left chroma right of a VP9 output's.
        ("vp9-color.webm", 256, 64, h264, "chroma sits"),
        ("h264-bt709-copy.mp4", 128, 96, vp9, "chroma sits"),
        // A render shows a mirroring display matrix's mirror, which the decoded planes do not have.
        ("h264-bt709-hflip.mp4", 128, 96, h264, "mirrors"),
        // And 4:3 pixels 16:9, whether the container declares them or only the codec does.
        ("h264-bt709-vui-sar.mp4", 128, 96, h264, "4:3 pixels"),
        ("h264-bt709-vui-sar.ts", 128, 96, h264, "4:3 pixels"),
        ("h264-bt709-pasp.mp4", 128, 96, h264, "4:3 pixels"),
    ] {
        let reason =
            crate::check_yuv_transcode(&corpus(name), width, height, output).expect_err(name);
        assert!(reason.contains(expected), "{name}: {reason}");
    }
}

/// `h264-bt709-copy.mp4` remuxed into fragments, with the first audio
/// fragment's 64-bit `tfdt`, which the demuxer takes as the fragment's first
/// decode time unchecked, set to `audio_start`.
fn fragmented_copy_source(label: &str, audio_start: Option<i64>) -> PathBuf {
    let path = output_path(label);
    let _ = fs::remove_file(&path);
    let mut input = rsmpeg::avformat::AVFormatContextInput::open(
        &std::ffi::CString::new(corpus("h264-bt709-copy.mp4").to_str().expect("UTF-8"))
            .expect("no NUL"),
    )
    .expect("source");
    let name = std::ffi::CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
    let mut output = rsmpeg::avformat::AVFormatContextOutput::create(&name).expect("output");
    for stream in input.streams() {
        let mut copied = output.new_stream();
        copied.set_codecpar(stream.codecpar().clone());
        copied.set_time_base(stream.time_base);
        alpha2_ffmpeg_bridge::set_stream_codec_tag(&mut copied, 0);
    }
    let mut options = Some(AVDictionary::new(
        c"movflags",
        c"frag_keyframe+empty_moov+default_base_moof",
        0,
    ));
    output.write_header(&mut options).expect("header");
    let bases = input
        .streams()
        .iter()
        .zip(output.streams())
        .map(|(from, to)| (from.time_base, to.time_base))
        .collect::<Vec<_>>();
    while let Some(mut packet) = input.read_packet().expect("packet") {
        let (from, to) = bases[usize::try_from(packet.stream_index).expect("index")];
        packet.rescale_ts(from, to);
        packet.set_pos(-1);
        output.interleaved_write_frame(&mut packet).expect("write");
    }
    output.write_trailer().expect("trailer");
    drop(output);
    let Some(audio_start) = audio_start else {
        return path;
    };
    let mut bytes = fs::read(&path).expect("read");
    let boxes = |bytes: &[u8], from: usize, to: usize| {
        let mut found = Vec::new();
        let mut at = from;
        while at + 8 <= to {
            let size =
                usize::try_from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())).unwrap();
            found.push((bytes[at + 4..at + 8].to_vec(), at, at + size));
            at += size.max(8);
        }
        found
    };
    let audio_tfdt = boxes(&bytes, 0, bytes.len())
        .into_iter()
        .filter(|(kind, ..)| kind == b"moof")
        .flat_map(|(_, from, to)| boxes(&bytes, from + 8, to))
        .filter(|(kind, ..)| kind == b"traf")
        .find_map(|(_, from, to)| {
            let children = boxes(&bytes, from + 8, to);
            let (_, tfhd, _) = children.iter().find(|(kind, ..)| kind == b"tfhd")?;
            // The audio track is the second: `track_ID` 2.
            (bytes[tfhd + 12..tfhd + 16] == 2_u32.to_be_bytes()).then_some(())?;
            let (_, tfdt, _) = children.iter().find(|(kind, ..)| kind == b"tfdt")?;
            assert_eq!(bytes[tfdt + 8], 1, "a 64-bit tfdt");
            Some(*tfdt)
        })
        .expect("an audio fragment");
    bytes[audio_tfdt + 12..audio_tfdt + 20].copy_from_slice(&audio_start.to_be_bytes());
    fs::write(&path, bytes).expect("patched");
    path
}

/// Copied audio is moved by the shift that puts the video in place, from
/// timestamps the file states unchecked: a fragment's could overflow the
/// shift. Audio whose timestamps cannot be moved, or would land outside the
/// video, is not the render's mix, so the plan encodes the mix instead of
/// copying it.
#[test]
fn copied_audio_whose_timestamps_land_outside_the_video_is_not_copied() {
    // The remux drops the edit list that put the first frame at zero: two frames of B-frame delay
    // come first.
    let frame_times = (2..62)
        .map(|frame| MediaTime::from_ticks(frame * alpha2_time::TICKS_PER_SECOND / 30))
        .collect::<Vec<_>>();
    let plan = |source: &Path| {
        crate::plan_packet_copy(&crate::PacketCopyRequest {
            source,
            frame_times: &frame_times,
            width: 128,
            height: 96,
            codec: NativeVideoCodec::H264,
            color: alpha2_media::ColorMetadata::EXPORT,
            output_start: MediaTime::ZERO,
            audio: Some(crate::PacketCopyAudio {
                sample_rate: 48_000,
                channels: 2,
            }),
            container: alpha2_media::Container::Mp4,
        })
        .expect("the video is copied")
    };
    let untouched = fragmented_copy_source("fragmented", None);
    let copied = plan(&untouched);
    assert!(copied.copies_audio(), "{:?}", copied.audio_copy_refusal());
    fs::remove_file(&untouched).expect("cleanup");
    for (label, start) in [
        ("tfdt-min", i64::MIN + 1),
        ("tfdt-early", -48_000 * 60),
        ("tfdt-late", 48_000 * 60),
    ] {
        let source = fragmented_copy_source(label, Some(start));
        let refused = plan(&source);
        fs::remove_file(&source).expect("cleanup");
        assert!(!refused.copies_audio(), "{label}");
        assert!(
            refused
                .audio_copy_refusal()
                .is_some_and(|reason| reason.contains("outside its video")),
            "{label}: {:?}",
            refused.audio_copy_refusal()
        );
    }
}

/// The 30 fps frame times of frames `frames` of the copy fixtures.
fn copy_frame_times(frames: std::ops::Range<i64>) -> Vec<MediaTime> {
    frames
        .map(|frame| MediaTime::from_ticks(frame * alpha2_time::TICKS_PER_SECOND / 30))
        .collect()
}

/// A copy plan of `name`'s frames `frames`, as the host asks for one, with
/// audio offered when `audio`.
fn copy_plan(
    name: &str,
    frames: std::ops::Range<i64>,
    output_start: MediaTime,
    audio: bool,
) -> Result<crate::PacketCopyPlan, String> {
    let source = corpus(name);
    let frame_times = copy_frame_times(frames);
    crate::plan_packet_copy(&crate::PacketCopyRequest {
        source: &source,
        frame_times: &frame_times,
        width: 128,
        height: 96,
        codec: NativeVideoCodec::H264,
        color: alpha2_media::ColorMetadata::EXPORT,
        output_start,
        audio: audio.then_some(crate::PacketCopyAudio {
            sample_rate: 48_000,
            channels: 2,
        }),
        container: alpha2_media::Container::Mp4,
    })
}

/// A copy plan reads its whole source in one operation; a host watching it
/// for a stall sees it alive packet by packet, as a seek is frame by frame.
#[test]
fn a_copy_plan_reports_progress_for_each_packet_it_reads() {
    let reported = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let counter = std::rc::Rc::clone(&reported);
    let watch = crate::watch_decode_progress(move || counter.set(counter.get() + 1));
    copy_plan("h264-bt709-copy.mp4", 0..60, MediaTime::ZERO, true).expect("copy");
    drop(watch);
    assert!(reported.get() >= 60, "{} reports", reported.get());
}

/// A whole-file copy carries audio only as far as a render mixes, exactly
/// the range: with 2.5 s of audio beside 2 s of video, every audio packet to
/// the end of the file would run the copy's audio and its duration past it.
#[test]
fn copied_audio_that_outlasts_the_video_is_not_copied() {
    let plan = copy_plan("h264-bt709-copy.mp4", 0..60, MediaTime::ZERO, true).expect("copy");
    assert!(plan.copies_audio(), "{:?}", plan.audio_copy_refusal());
    let plan = copy_plan("h264-bt709-audio-longer.mp4", 0..60, MediaTime::ZERO, true)
        .expect("video copies");
    assert!(!plan.copies_audio());
    assert!(
        plan.audio_copy_refusal()
            .is_some_and(|reason| reason.contains("outlasts")),
        "{:?}",
        plan.audio_copy_refusal()
    );
}

/// A copy keeps the source's colour tags, so they must be exactly the
/// output's. A stream that tags only its range reads as BT.709 in the
/// probe's summary, which fills in the rest, but is not copied: the copy
/// would be untagged, where a render tags BT.709.
#[test]
fn a_copy_takes_only_a_source_tagged_exactly_as_the_output() {
    let reason = copy_plan("h264-bt709-range-only.mp4", 0..60, MediaTime::ZERO, false)
        .expect_err("primaries, transfer and matrix are unspecified");
    assert!(reason.contains("tagged"), "{reason}");
}

/// A track whose second half is described by another sample description
/// (other parameter sets) is not copied under the first one's.
#[test]
fn a_copy_refuses_a_run_another_sample_description_describes() {
    copy_plan("h264-bt709-two-stsd.mp4", 0..30, MediaTime::ZERO, false)
        .expect("the first description's frames copy");
    let reason = copy_plan("h264-bt709-two-stsd.mp4", 30..60, MediaTime::ZERO, false)
        .expect_err("the second description's frames");
    assert!(reason.contains("sample description"), "{reason}");
}

/// The refusals a copy's correctness rests on, each named: frames off the
/// source's grid, a range past the source's end, a run whose packets are not
/// exactly its frames (a B picture's reference left out), and a join of
/// streams one sample description cannot describe.
#[test]
fn a_copy_names_each_refusal_its_correctness_rests_on() {
    let source = corpus("h264-bt709-copy.mp4");
    let off_grid = (0..50)
        .map(|frame| MediaTime::from_ticks(frame * alpha2_time::TICKS_PER_SECOND / 25))
        .collect::<Vec<_>>();
    let reason = crate::plan_packet_copy(&crate::PacketCopyRequest {
        source: &source,
        frame_times: &off_grid,
        width: 128,
        height: 96,
        codec: NativeVideoCodec::H264,
        color: alpha2_media::ColorMetadata::EXPORT,
        output_start: MediaTime::ZERO,
        audio: None,
        container: alpha2_media::Container::Mp4,
    })
    .expect_err("25 fps of a 30 fps source");
    assert!(
        reason.contains("time base") || reason.contains("not exactly its frames"),
        "{reason}"
    );
    let reason =
        copy_plan("h264-bt709-copy.mp4", 30..70, MediaTime::ZERO, false).expect_err("past the end");
    assert!(reason.contains("ends before"), "{reason}");
    // Frames 30 to 44: the P picture shown at 45 is decoded before the B pictures at 43 and 44.
    let reason = copy_plan("h264-bt709-copy.mp4", 30..45, MediaTime::ZERO, false)
        .expect_err("a run whose packets are not its frames");
    assert!(reason.contains("not exactly its frames"), "{reason}");
    let first = copy_plan("h264-bt709-copy.mp4", 0..30, MediaTime::ZERO, false).expect("first");
    let second = copy_plan(
        "h264-bt709-level.mp4",
        30..60,
        MediaTime::from_ticks(alpha2_time::TICKS_PER_SECOND),
        false,
    )
    .expect("second");
    let reason = crate::PacketCopy::join(vec![first, second]).expect_err("other parameter sets");
    assert!(reason.contains("not encoded like"), "{reason}");
}

/// A copy cancelled while it writes leaves neither its output nor a
/// partial file.
#[test]
fn a_cancelled_copy_leaves_nothing() {
    let plan = copy_plan("h264-bt709-copy.mp4", 0..60, MediaTime::ZERO, false).expect("copy");
    let copy = crate::PacketCopy::join(vec![plan]).expect("one run");
    let output = output_path("cancelled-copy");
    let _ = fs::remove_file(&output);
    let cancel = CancelToken::default();
    let mut progress = |written: u64| {
        if written == 10 {
            cancel.cancel();
        }
    };
    let error = crate::copy_packets(
        &copy,
        &output,
        alpha2_media::Container::Mp4,
        crate::CopyAudio::None,
        &cancel,
        &mut progress,
    )
    .expect_err("cancelled");
    assert_eq!(error, EncodeError::Cancelled);
    assert!(!output.exists() && !partial_beside(&output).exists());
}

/// One planner reads each source once however many runs it serves (forty
/// cuts from one long recording read it once, not forty times), and a
/// cancel stops it.
#[test]
fn a_planner_reads_each_source_once_and_stops_when_cancelled() {
    fn request<'a>(
        source: &'a Path,
        first: i64,
        times: &'a [MediaTime],
    ) -> crate::PacketCopyRequest<'a> {
        crate::PacketCopyRequest {
            source,
            frame_times: times,
            width: 128,
            height: 96,
            codec: NativeVideoCodec::H264,
            color: alpha2_media::ColorMetadata::EXPORT,
            output_start: MediaTime::from_ticks(first * alpha2_time::TICKS_PER_SECOND / 30),
            audio: None,
            container: alpha2_media::Container::Mp4,
        }
    }
    let opened = || crate::decode::INPUTS_OPENED.with(std::cell::Cell::get);
    let source = corpus("h264-bt709-copy.mp4");
    let (early, late) = (copy_frame_times(0..30), copy_frame_times(30..60));
    let mut planner = crate::PacketCopyPlanner::new(CancelToken::default());
    let before = opened();
    planner
        .plan(&request(&source, 0, &early))
        .expect("first run");
    let one = opened() - before;
    planner
        .plan(&request(&source, 30, &late))
        .expect("second run");
    assert_eq!(opened() - before, one, "the second run reads nothing again");
    let cancel = CancelToken::default();
    cancel.cancel();
    let reason = crate::PacketCopyPlanner::new(cancel)
        .plan(&request(&source, 0, &early))
        .expect_err("cancelled");
    assert!(reason.contains("cancelled"), "{reason}");
}
