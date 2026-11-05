use std::io::Cursor;

use alpha2_media::{Orientation, Rotation};
use bytes::Bytes;

use super::{corpus_dir, expected, packets};
use crate::{
    AudioCodec, ContainerReader, ContainerWriter, DemuxError, FourCc, Mp4Flavor, Mp4Reader,
    Mp4Writer, Packet, TrackId, TrackInfo, TrackKind, VideoCodec, VideoTrack,
};

fn open(name: &str) -> Mp4Reader {
    Mp4Reader::open(&corpus_dir().join(name)).expect(name)
}

fn track_of(reader: &Mp4Reader, kind: TrackKind) -> TrackInfo {
    reader
        .tracks()
        .iter()
        .find(|track| track.kind == kind)
        .cloned()
        .expect("track")
}

#[test]
fn seeking_lands_on_the_latest_keyframe_at_or_before_the_time() {
    let mut reader = open("h264-bt709-copy.mp4");
    let video = TrackId(1);
    // IDR frames at 0 and frame 30, 512 ticks a frame.
    assert_eq!(reader.seek(video, 23_040).expect("seek"), Some(15_360));
    let packet = reader.next_packet(video).expect("packet").expect("some");
    assert_eq!((packet.pts, packet.keyframe), (15_360, true));
    assert_eq!(reader.seek(video, 15_359).expect("seek"), Some(0));
    assert_eq!(reader.seek(video, -5_000).expect("seek"), Some(0));
    assert_eq!(reader.seek(video, i64::MAX).expect("seek"), Some(15_360));
    assert!(matches!(
        reader.seek(TrackId(9), 0),
        Err(DemuxError::NoSuchTrack(9))
    ));
}

#[test]
fn seeking_audio_to_its_start_restores_the_priming_skip() {
    let mut reader = open("audio-only-aac-priming.m4a");
    let audio = TrackId(1);
    let all = packets(&mut reader, audio);
    assert_eq!((all[0].pts, all[0].discard), (-1024, (1024, 0)));
    assert_eq!(reader.seek(audio, -1024).expect("seek"), Some(-1024));
    let first = reader.next_packet(audio).expect("packet").expect("some");
    assert_eq!(first.discard, (1024, 0));
    assert_eq!(reader.seek(audio, 2048).expect("seek"), Some(2048));
    let later = reader.next_packet(audio).expect("packet").expect("some");
    assert_eq!((later.pts, later.discard), (2048, (0, 0)));
}

#[test]
fn audio_priming_comes_from_the_edit_list() {
    let reader = open("audio-only-aac-priming.m4a");
    let audio = track_of(&reader, TrackKind::Audio).audio.expect("audio");
    assert_eq!((audio.codec, audio.priming), (AudioCodec::Aac, 1024));
    // AudioSpecificConfig: AAC LC, 44.1 kHz, mono.
    assert_eq!(&audio.codec_private[..2], &[0x12, 0x08]);
    let opus = open("audio-opus.mp4");
    let audio = track_of(&opus, TrackKind::Audio).audio.expect("audio");
    assert_eq!((audio.codec, audio.priming), (AudioCodec::Opus, 120));
    assert_eq!(&audio.codec_private[..8], b"OpusHead");
    assert_eq!(
        u16::from_le_bytes([audio.codec_private[10], audio.codec_private[11]]),
        120
    );
}

#[test]
fn sample_entries_set_codec_configuration_and_alpha() {
    let video = |name: &str| {
        track_of(&open(name), TrackKind::Video)
            .video
            .expect("video")
    };
    let hev1 = video("hevc-topleft.mp4");
    assert_eq!(
        (hev1.codec, hev1.in_band_parameter_sets),
        (VideoCodec::Hevc, true)
    );
    assert_eq!(hev1.codec_private[0], 1);
    let hvc1 = video("hevc-hvc1.mp4");
    assert!(!hvc1.in_band_parameter_sets && !hvc1.alpha);
    assert!(video("hevc-alpha.mov").alpha);
    assert_eq!(
        video("prores-pq-alpha.mov").codec,
        VideoCodec::ProRes(FourCc(*b"ap4h"))
    );
    let avc = video("h264-bt709-copy.mp4");
    assert_eq!((avc.codec, avc.codec_private[0]), (VideoCodec::H264, 1));
    assert_eq!(
        avc.frame_rate
            .map(|rate| (rate.numerator(), rate.denominator())),
        Some((30, 1))
    );
    assert_eq!(video("av1.mp4").codec, VideoCodec::Av1);
    assert_eq!(video("recovery-vp9.mp4").codec, VideoCodec::Vp9);
    let mpeg4 = video("rotation.mp4");
    assert_eq!(mpeg4.codec, VideoCodec::Mpeg4Part2);
    assert_eq!(&mpeg4.codec_private[..4], &[0, 0, 1, 0xb0]);
    assert_eq!(mpeg4.orientation, Orientation::rotated(Rotation::Deg270));
}

#[test]
fn the_sample_entry_size_is_reported_where_ffmpeg_reports_the_cropped_size() {
    let expected = expected();
    for (name, field, coded) in [
        ("h264-bt709-crop-left.mp4", "width", 128),
        ("h264-bt709-crop-bottom.mp4", "height", 96),
    ] {
        let video = track_of(&open(name), TrackKind::Video)
            .video
            .expect("video");
        let ours = if field == "width" {
            video.width
        } else {
            video.height
        };
        let theirs = expected["files"][name]["tracks"][0][field].as_u64();
        assert_eq!(ours, coded, "{name}");
        assert_ne!(
            Some(u64::from(ours)),
            theirs,
            "{name}: the exception is no longer needed"
        );
    }
}

fn video_track(width: u32, height: u32) -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        kind: TrackKind::Video,
        timescale: 15_360,
        duration: None,
        language: None,
        title: None,
        default: true,
        enabled: true,
        video: Some(VideoTrack {
            codec: VideoCodec::H264,
            codec_private: Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0]),
            in_band_parameter_sets: false,
            width,
            height,
            orientation: Orientation::UPRIGHT,
            sample_aspect_ratio: None,
            color: None,
            clean_aperture: None,
            frame_rate: None,
            alpha: false,
        }),
        audio: None,
    }
}

fn written(track: &TrackInfo, frames: i64) -> Vec<u8> {
    let path = std::env::temp_dir().join(format!(
        "alpha2-container-demux-{}-{}x{}.mp4",
        std::process::id(),
        track.video.as_ref().map_or(0, |video| video.width),
        frames
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("file");
    let mut writer = Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4));
    let id = writer.add_track(track).expect("track");
    for frame in 0..frames {
        writer
            .write(Packet {
                track: id,
                pts: frame * 512,
                dts: frame * 512,
                duration: 512,
                keyframe: frame == 0,
                data: Bytes::from(vec![u8::try_from(frame % 256).unwrap_or(0); 10]),
                discard: (0, 0),
            })
            .expect("write");
    }
    writer.finish().expect("finish");
    let bytes = std::fs::read(&path).expect("read");
    let _ = std::fs::remove_file(&path);
    bytes
}

#[test]
fn pictures_beyond_the_engine_limits_are_refused() {
    let bytes = written(&video_track(16_385, 64), 2);
    assert!(matches!(
        Mp4Reader::new(Cursor::new(bytes)),
        Err(DemuxError::Invalid(_))
    ));
    let bytes = written(&video_track(9_000, 8_000), 2);
    assert!(matches!(
        Mp4Reader::new(Cursor::new(bytes)),
        Err(DemuxError::Invalid(_))
    ));
    let bytes = written(&video_track(16_384, 4_096), 2);
    assert!(Mp4Reader::new(Cursor::new(bytes)).is_ok());
}

#[test]
fn a_file_written_to_disk_reads_back_with_its_index_first() {
    let bytes = written(&video_track(64, 48), 40);
    assert_eq!(&bytes[4..8], b"ftyp");
    let moov = bytes
        .windows(4)
        .position(|window| window == b"moov")
        .expect("moov");
    let mdat = bytes
        .windows(4)
        .position(|window| window == b"mdat")
        .expect("mdat");
    assert!(moov < mdat);
    let mut reader = Mp4Reader::new(Cursor::new(bytes)).expect("open");
    let read = packets(&mut reader, TrackId(1));
    assert_eq!(read.len(), 40);
    for (frame, packet) in (0_u8..).zip(&read) {
        assert_eq!(packet.dts, i64::from(frame) * 512);
        assert_eq!((packet.keyframe, packet.data[0]), (frame == 0, frame));
    }
}

#[test]
fn broken_and_truncated_files_fail_cleanly() {
    assert!(matches!(
        Mp4Reader::new(Cursor::new(b"\0\0\0\x10ftypisom\0\0\0\0".to_vec())),
        Err(DemuxError::Invalid(_))
    ));
    assert!(Mp4Reader::new(Cursor::new(Vec::new())).is_err());
    let whole = std::fs::read(corpus_dir().join("h264-bt709-copy.mp4")).expect("file");
    for cut in [whole.len() / 2, whole.len() - 1, 40, 9] {
        if let Ok(mut reader) = Mp4Reader::new(Cursor::new(whole[..cut].to_vec())) {
            for track in reader.tracks().to_vec() {
                while let Ok(Some(_)) = reader.next_packet(track.id) {}
            }
        }
    }
}

#[test]
fn files_open_by_their_sniffed_format() {
    let reader = crate::open(&corpus_dir().join("audio-two-streams.mov")).expect("open");
    assert_eq!(reader.tracks().len(), 2);
    assert_eq!(reader.default_audio(), Some(0));
    assert_eq!(reader.tracks()[1].language.as_deref(), Some("eng"));
}
