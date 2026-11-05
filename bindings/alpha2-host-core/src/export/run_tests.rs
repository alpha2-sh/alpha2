use std::collections::VecDeque;

use alpha2_container::{
    ContainerReader, DemuxError, Packet, TrackId, TrackInfo, TrackKind, VideoCodec, VideoTrack,
};
use alpha2_engine::{EngineProject, Exporter, QualityProfile, TextTextures};
use alpha2_media::{Orientation, Plane, YuvFormat};
use alpha2_model::{
    Clip, ClipId, MediaKind, MediaSource, Project, ProjectId, ProjectSettings, Timeline, Track,
    TrackId as ModelTrackId, TrackKind as ModelTrackKind,
};
use alpha2_time::{FrameRate, TimeRange};
use bytes::Bytes;

use super::*;
use crate::video_stream::{
    BackendError, BackendFrame, BackendImage, BackendKind, Poll, StreamOptions, Submit,
    VideoBackends, VideoDecodeBackend, VideoStream,
};

const SIZE: u32 = 16;

/// 30 intra frames at 30 fps; frame `i` is presented at `i` and has luma `16 + 6 i`.
struct Frames {
    tracks: Vec<TrackInfo>,
    next: i64,
}

impl ContainerReader for Frames {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(&mut self, _track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError> {
        self.next = pts.clamp(0, 29);
        Ok(Some(self.next))
    }

    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError> {
        if self.next >= 30 {
            return Ok(None);
        }
        let pts = self.next;
        self.next += 1;
        Ok(Some(Packet {
            track,
            pts,
            dts: pts,
            duration: 1,
            keyframe: true,
            data: Bytes::new(),
            discard: (0, 0),
        }))
    }
}

struct Session(VecDeque<i64>, bool);

impl VideoDecodeBackend for Session {
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        self.0.push_back(packet.pts);
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<Poll, BackendError> {
        Ok(match self.0.pop_front() {
            Some(pts) => {
                let luma = u8::try_from(16 + 6 * pts).expect("small");
                let plane = |width: u32, value: u8| Plane {
                    bytes: Bytes::from(vec![value; (width * width) as usize]),
                    stride: width,
                    width,
                    height: width,
                };
                Poll::Frame(BackendFrame {
                    pts,
                    image: BackendImage::Cpu {
                        format: YuvFormat::I420,
                        planes: vec![
                            plane(SIZE, luma),
                            plane(SIZE / 2, 128),
                            plane(SIZE / 2, 128),
                        ],
                    },
                    coded: (SIZE, SIZE),
                    visible: None,
                    sample_aspect_ratio: None,
                    color: Some(alpha2_media::ColorMetadata::EXPORT),
                    chroma_location: Some(alpha2_media::ChromaLocation::Left),
                })
            }
            None if self.1 => Poll::Drained,
            None => Poll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.1 = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.0.clear();
        self.1 = false;
        Ok(())
    }
}

struct Sessions;

impl VideoBackends for Sessions {
    fn open(
        &self,
        _track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        if kind == BackendKind::Hardware {
            return Err(BackendError::Unsupported("software only".into()));
        }
        Ok(Box::new(Session(VecDeque::new(), false)))
    }
}

fn reader() -> Box<dyn ContainerReader> {
    Box::new(Frames {
        tracks: vec![TrackInfo {
            id: TrackId(1),
            kind: TrackKind::Video,
            timescale: 30,
            duration: Some(30),
            language: None,
            title: None,
            default: true,
            enabled: true,
            video: Some(VideoTrack {
                codec: VideoCodec::Vp9,
                codec_private: Bytes::new(),
                in_band_parameter_sets: false,
                width: SIZE,
                height: SIZE,
                orientation: Orientation::default(),
                sample_aspect_ratio: None,
                color: None,
                clean_aperture: None,
                frame_rate: None,
                alpha: false,
            }),
            audio: None,
        }],
        next: 0,
    })
}

/// A 16x16 project at 30 fps showing source `v` from `offset` frames in, for `frames` frames.
fn project(offset: i64, frames: i64) -> EngineProject {
    let frame = |count: i64| MediaTime::from_ticks(count * alpha2_time::TICKS_PER_SECOND / 30);
    let source = SourceId::new("v").expect("id");
    let clip = Clip::new(
        ClipId::new("c").expect("id"),
        source.clone(),
        TimeRange::new(frame(0), frame(frames)).expect("range"),
        TimeRange::new(frame(offset), frame(offset + frames)).expect("range"),
    );
    let mut track = Track::new(
        ModelTrackId::new("t").expect("id"),
        "video",
        ModelTrackKind::Video,
    );
    track.clips.push(clip);
    EngineProject::new(Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("p").expect("id"),
        name: "export".into(),
        settings: ProjectSettings::new(SIZE, SIZE, FrameRate::new(30, 1).expect("rate"), 48_000)
            .expect("settings"),
        sources: vec![MediaSource::new(source, "asset://v", MediaKind::Video)],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline {
            tracks: vec![track],
        },
        audio: alpha2_model::MasterAudio::default(),
    })
    .expect("project")
}

#[derive(Default)]
struct Collect(Vec<(u64, u8)>);

impl VideoSink for Collect {
    fn write_video(
        &mut self,
        index: u64,
        _time: MediaTime,
        _layout: PixelLayout,
        planes: &[PlaneBuffer],
    ) -> Result<(), EncodeError> {
        let luma = &planes[0];
        let middle = luma.bytes[luma.stride * (luma.height as usize / 2) + luma.width as usize / 2];
        self.0.push((index, middle));
        Ok(())
    }
}

fn export(offset: i64, frames: i64) -> Vec<(u64, u8)> {
    let snapshot = project(offset, frames);
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let renderer = CpuRenderer::new(2);
    let mut sources = StreamSources::new(|_: &SourceId| {
        VideoStream::open(reader(), 0, Arc::new(Sessions), StreamOptions::default())
            .map_err(|error| error.to_string())
    });
    let mut sink = Collect::default();
    let mut reports = Vec::new();
    export_video(
        &snapshot,
        &mut exporter,
        ExportRenderer::Cpu(&renderer),
        &mut TextTextures::new(),
        &mut sources,
        &mut sink,
        &CancelToken::default(),
        &mut |done, total| reports.push((done, total)),
    )
    .expect("export");
    assert_eq!(
        reports.last(),
        Some(&(sink.0.len() as u64, sink.0.len() as u64))
    );
    sink.0
}

#[test]
fn every_frame_renders_once_in_order_with_its_source_frame() {
    let frames = export(5, 10);
    assert_eq!(frames.len(), 10);
    let indices = frames.iter().map(|(index, _)| *index).collect::<Vec<_>>();
    assert_eq!(indices, (0..10).collect::<Vec<_>>());
    // Each output frame shows source frame 5 + i, a distinct luma; the renderer converts to its
    // own range, so only order and distinctness are checked.
    let lumas = frames.iter().map(|(_, luma)| *luma).collect::<Vec<_>>();
    assert!(lumas.windows(2).all(|pair| pair[0] < pair[1]), "{lumas:?}");
}

#[test]
fn a_cancelled_export_stops() {
    let snapshot = project(0, 5);
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let renderer = CpuRenderer::new(1);
    let mut sources = StreamSources::new(|_: &SourceId| {
        VideoStream::open(reader(), 0, Arc::new(Sessions), StreamOptions::default())
            .map_err(|error| error.to_string())
    });
    let cancel = CancelToken::default();
    cancel.cancel();
    let result = export_video(
        &snapshot,
        &mut exporter,
        ExportRenderer::Cpu(&renderer),
        &mut TextTextures::new(),
        &mut sources,
        &mut Collect::default(),
        &cancel,
        &mut |_, _| {},
    );
    assert!(matches!(result, Err(ExportError::Cancelled)), "{result:?}");
}
