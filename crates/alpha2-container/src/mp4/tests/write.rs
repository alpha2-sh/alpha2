use std::io::Cursor;

use bytes::Bytes;

use super::{Shared, packets};
use crate::{
    ContainerReader, ContainerWriter, FourCc, Mp4Flavor, Mp4Reader, Mp4Writer, MuxError, Packet,
    TrackId, TrackInfo, TrackKind, VideoCodec, VideoTrack,
};

fn video_info(codec: VideoCodec) -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        kind: TrackKind::Video,
        timescale: 30,
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
            orientation: alpha2_media::Orientation::UPRIGHT,
            sample_aspect_ratio: None,
            color: None,
            clean_aperture: None,
            frame_rate: alpha2_time::FrameRate::new(30, 1).ok(),
            alpha: false,
            reorder_depth: None,
        }),
        audio: None,
    }
}

fn frame(track: TrackId, dts: i64) -> Packet {
    Packet {
        track,
        pts: dts,
        dts,
        duration: 0,
        keyframe: true,
        data: Bytes::from_static(b"frame"),
        discard: (0, 0),
    }
}

#[test]
fn the_writer_refuses_what_mp4_cannot_carry() {
    let mut writer = Mp4Writer::new(Shared::default(), Mp4Flavor::Mp4);
    let prores = VideoCodec::ProRes(FourCc(*b"apch"));
    assert!(matches!(
        writer.add_track(&video_info(prores)),
        Err(MuxError::Unsupported(_))
    ));
    assert!(matches!(
        writer.add_track(&video_info(VideoCodec::Vp8)),
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
        writer.write(frame(TrackId(7), 0)),
        Err(MuxError::Invalid(_))
    ));
    writer.write(frame(id, 1)).expect("write");
    assert!(matches!(
        writer.write(frame(id, 1)),
        Err(MuxError::Invalid(_))
    ));
    assert!(matches!(
        writer.add_track(&video_info(VideoCodec::H264)),
        Err(MuxError::Invalid(_))
    ));
    let mut mov = Mp4Writer::new(Shared::default(), Mp4Flavor::Mov);
    assert!(mov.add_track(&video_info(prores)).is_ok());
}

#[test]
fn packets_without_durations_last_one_frame_in_a_doubled_timescale() {
    let buffer = Shared::default();
    let mut writer = Box::new(Mp4Writer::new(buffer.clone(), Mp4Flavor::Mp4));
    let id = writer
        .add_track(&video_info(VideoCodec::H264))
        .expect("track");
    for dts in 0..3 {
        writer.write(frame(id, dts)).expect("write");
    }
    writer.finish().expect("finish");
    let mut reader = Mp4Reader::new(Cursor::new(buffer.contents())).expect("open");
    // 30 doubles to 15360, its first power-of-two multiple at or above 10000.
    assert_eq!(reader.tracks()[0].timescale, 15_360);
    let read = packets(&mut reader, TrackId(1));
    let times = read.iter().map(|packet| (packet.dts, packet.duration));
    assert_eq!(
        times.collect::<Vec<_>>(),
        [(0, 512), (512, 512), (1024, 512)]
    );
}

#[test]
fn an_empty_movie_is_still_a_readable_file() {
    let buffer = Shared::default();
    Box::new(Mp4Writer::new(buffer.clone(), Mp4Flavor::Mov))
        .finish()
        .expect("finish");
    let reader = Mp4Reader::new(Cursor::new(buffer.contents())).expect("open");
    assert_eq!(reader.tracks(), []);
}
