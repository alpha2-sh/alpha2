use std::io::Cursor;

use alpha2_media::{
    ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, Orientation, Rotation,
    SampleAspectRatio, TransferFunction,
};
use bytes::Bytes;

use crate::mp4::tests::{Shared, packets};
use crate::{
    AudioCodec, AudioTrack, ContainerReader, ContainerWriter, FourCc, MatroskaFlavor,
    MatroskaReader, MatroskaWriter, MuxError, Packet, PcmFormat, TrackId, TrackInfo, TrackKind,
    VideoCodec, VideoTrack,
};

fn video_info(codec: VideoCodec) -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        kind: TrackKind::Video,
        timescale: 90_000,
        duration: None,
        language: None,
        title: None,
        default: true,
        enabled: true,
        video: Some(VideoTrack {
            codec,
            codec_private: Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0]),
            in_band_parameter_sets: false,
            width: 64,
            height: 48,
            orientation: Orientation::UPRIGHT,
            sample_aspect_ratio: Some(SampleAspectRatio { num: 1, den: 1 }),
            color: None,
            clean_aperture: None,
            frame_rate: alpha2_time::FrameRate::new(30, 1).ok(),
            alpha: false,
            reorder_depth: None,
        }),
        audio: None,
    }
}

fn audio_info(codec: AudioCodec, priming: u32) -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        kind: TrackKind::Audio,
        timescale: 48_000,
        duration: None,
        language: Some("eng".into()),
        title: Some("Commentary".into()),
        default: false,
        enabled: true,
        video: None,
        audio: Some(AudioTrack {
            codec,
            codec_private: Bytes::from_static(b"OpusHead\x01\x01\x38\x01\x80\xbb\0\0\0\0\0"),
            sample_rate: 48_000,
            channels: 1,
            speakers: None,
            bits_per_sample: None,
            priming,
            seek_preroll: None,
        }),
    }
}

fn frame(track: TrackId, pts: i64, dts: i64, keyframe: bool) -> Packet {
    Packet {
        track,
        pts,
        dts,
        duration: 3000,
        keyframe,
        data: Bytes::from_static(b"\0\0\0\x01frame"),
        discard: (0, 0),
    }
}

fn read_back(buffer: &Shared) -> MatroskaReader {
    MatroskaReader::new(Cursor::new(buffer.contents())).expect("read back")
}

#[test]
fn the_writer_refuses_what_matroska_cannot_carry() {
    let mut webm = MatroskaWriter::new(Shared::default(), MatroskaFlavor::WebM);
    for codec in [
        VideoCodec::H264,
        VideoCodec::Hevc,
        VideoCodec::ProRes(FourCc(*b"apch")),
    ] {
        assert!(matches!(
            webm.add_track(&video_info(codec)),
            Err(MuxError::Unsupported(_))
        ));
    }
    assert!(matches!(
        webm.add_track(&audio_info(AudioCodec::Aac, 0)),
        Err(MuxError::Unsupported(_))
    ));
    assert!(webm.add_track(&video_info(VideoCodec::Vp9)).is_ok());
    assert!(webm.add_track(&audio_info(AudioCodec::Opus, 312)).is_ok());

    let mut writer = MatroskaWriter::new(Shared::default(), MatroskaFlavor::Matroska);
    assert!(matches!(
        writer.add_track(&video_info(VideoCodec::Other(FourCc(*b"FFV1")))),
        Err(MuxError::Unsupported(_))
    ));
    let float_big_endian = AudioCodec::Pcm(PcmFormat {
        bits: 32,
        float: true,
        big_endian: true,
        signed: true,
    });
    assert!(matches!(
        writer.add_track(&audio_info(float_big_endian, 0)),
        Err(MuxError::Unsupported(_))
    ));
    let mut other = video_info(VideoCodec::H264);
    other.kind = TrackKind::Other;
    assert!(matches!(
        writer.add_track(&other),
        Err(MuxError::Unsupported(_))
    ));
    let id = writer
        .add_track(&video_info(VideoCodec::H264))
        .expect("track");
    assert!(matches!(
        writer.write(frame(TrackId(7), 0, 0, true)),
        Err(MuxError::Invalid(_))
    ));
    writer.write(frame(id, 3000, 3000, true)).expect("write");
    assert!(matches!(
        writer.write(frame(id, 0, 0, true)),
        Err(MuxError::Invalid(_))
    ));
    assert!(matches!(
        writer.add_track(&video_info(VideoCodec::H264)),
        Err(MuxError::Invalid(_))
    ));
}

#[test]
fn reordered_video_keeps_its_presentation_times_in_milliseconds() {
    let buffer = Shared::default();
    let mut writer = Box::new(MatroskaWriter::new(
        buffer.clone(),
        MatroskaFlavor::Matroska,
    ));
    let id = writer
        .add_track(&video_info(VideoCodec::H264))
        .expect("track");
    // I0 P3 B1 B2 at 30 fps in 1/90000, decode times one frame early.
    for (pts, dts, key) in [
        (0, -3000, true),
        (9000, 0, false),
        (3000, 3000, false),
        (6000, 6000, false),
    ] {
        writer.write(frame(id, pts, dts, key)).expect("write");
    }
    writer.finish().expect("finish");
    let mut reader = read_back(&buffer);
    let track = reader.tracks()[0].clone();
    assert_eq!(track.timescale, 1000);
    let read = packets(&mut reader, track.id);
    let times = read
        .iter()
        .map(|packet| (packet.pts, packet.duration, packet.keyframe))
        .collect::<Vec<_>>();
    // A 30 fps default duration is 33 ms; packets of 33 ms need no block duration.
    assert_eq!(
        times,
        [
            (0, 33, true),
            (100, 33, false),
            (33, 33, false),
            (67, 33, false)
        ]
    );
}

#[test]
fn opus_priming_becomes_codec_delay_and_padding_becomes_discard_padding() {
    let buffer = Shared::default();
    let mut writer = Box::new(MatroskaWriter::new(buffer.clone(), MatroskaFlavor::WebM));
    let id = writer
        .add_track(&audio_info(AudioCodec::Opus, 312))
        .expect("track");
    // An encoder's packets, shifted back by the priming as the export sink hands them over.
    for index in 0..4_i64 {
        let start = index * 960 - 312;
        let mut packet = frame(id, start, start, true);
        packet.duration = 960;
        packet.data = Bytes::from_static(&[0xf8, 0xff, 0xfe]);
        if index == 3 {
            packet.discard = (0, 500);
        }
        writer.write(packet).expect("write");
    }
    writer.finish().expect("finish");
    let mut reader = read_back(&buffer);
    let track = reader.tracks()[0].clone();
    let audio = track.audio.clone().expect("audio");
    assert_eq!(audio.priming, 312);
    assert_eq!(track.language.as_deref(), Some("eng"));
    assert_eq!(track.title.as_deref(), Some("Commentary"));
    assert!(!track.default);
    let read = packets(&mut reader, track.id);
    let times = read
        .iter()
        .map(|packet| (packet.pts, packet.duration, packet.discard))
        .collect::<Vec<_>>();
    // Halves round away from zero: -6.5 ms to -7, 13.5 ms to 14, as in libopus WebM files. The
    // first packet carries the codec delay as its skip-samples.
    assert_eq!(
        times,
        [
            (-7, 20, (312, 0)),
            (14, 20, (0, 0)),
            (34, 20, (0, 0)),
            (54, 20, (0, 500))
        ]
    );
}

#[test]
fn picture_facts_survive_a_round_trip() {
    let pq = ColorMetadata {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferFunction::Pq,
        matrix: ColorMatrix::Bt2020Ncl,
        range: ColorRange::Full,
        dynamic_range: DynamicRange::Hdr,
    };
    for flavor in [MatroskaFlavor::Matroska, MatroskaFlavor::WebM] {
        for (rotation, mirror) in [
            (Rotation::Deg90, false),
            (Rotation::Deg180, true),
            (Rotation::Deg270, false),
            (Rotation::Deg0, true),
        ] {
            let mut info = video_info(VideoCodec::Vp9);
            let video = info.video.as_mut().expect("video");
            video.orientation = Orientation { rotation, mirror };
            video.sample_aspect_ratio = Some(SampleAspectRatio { num: 4, den: 3 });
            video.color = Some(pq);
            video.alpha = true;
            let buffer = Shared::default();
            let mut writer = Box::new(MatroskaWriter::new(buffer.clone(), flavor));
            let id = writer.add_track(&info).expect("track");
            writer.write(frame(id, 0, 0, true)).expect("write");
            writer.finish().expect("finish");
            let reader = read_back(&buffer);
            let read = reader.tracks()[0].video.clone().expect("video");
            assert_eq!(
                read.orientation,
                Orientation { rotation, mirror },
                "{flavor:?}"
            );
            assert_eq!(read.color, Some(pq), "{flavor:?}");
            assert!(read.alpha, "{flavor:?}");
            // WebM stores a display width rounded to pixels: 64 * 4 / 3 = 85.3.
            let expected = if flavor == MatroskaFlavor::WebM {
                SampleAspectRatio { num: 85, den: 64 }
            } else {
                SampleAspectRatio { num: 4, den: 3 }
            };
            assert_eq!(read.sample_aspect_ratio, Some(expected), "{flavor:?}");
        }
    }
    let mut info = video_info(VideoCodec::H264);
    info.video.as_mut().expect("video").sample_aspect_ratio = None;
    let buffer = Shared::default();
    let mut writer = Box::new(MatroskaWriter::new(
        buffer.clone(),
        MatroskaFlavor::Matroska,
    ));
    writer.add_track(&info).expect("track");
    writer.finish().expect("finish");
    let reader = read_back(&buffer);
    assert_eq!(
        reader.tracks()[0]
            .video
            .as_ref()
            .expect("video")
            .sample_aspect_ratio,
        None
    );
}

#[test]
fn a_silent_track_holds_back_others_only_for_ten_seconds() {
    let buffer = Shared::default();
    let mut writer = Box::new(MatroskaWriter::new(
        buffer.clone(),
        MatroskaFlavor::Matroska,
    ));
    let video = writer
        .add_track(&video_info(VideoCodec::H264))
        .expect("video");
    let pcm = AudioCodec::Pcm(PcmFormat {
        bits: 16,
        float: false,
        big_endian: false,
        signed: true,
    });
    writer.add_track(&audio_info(pcm, 0)).expect("audio");
    let before = buffer.contents().len();
    for index in 0..(12 * 30) {
        writer
            .write(frame(video, index * 3000, index * 3000, index % 30 == 0))
            .expect("write");
    }
    assert!(
        buffer.contents().len() > before,
        "video waited for audio past ten seconds"
    );
    writer.finish().expect("finish");
    let mut reader = read_back(&buffer);
    assert_eq!(packets(&mut reader, TrackId(1)).len(), 360);
    assert_eq!(packets(&mut reader, TrackId(2)).len(), 0);
}
