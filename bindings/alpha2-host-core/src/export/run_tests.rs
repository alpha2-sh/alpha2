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
    BackendError, BackendFrame, BackendImage, BackendKind, FrameFacts, Poll, StreamOptions, Submit,
    SurfaceLease, VideoBackends, VideoDecodeBackend, VideoStream,
};

const SIZE: u32 = 16;

/// Intra frames presented at `first..end` in track units; the one at `t` has luma `16 + 6 t % 216`.
struct Frames {
    tracks: Vec<TrackInfo>,
    next: i64,
    first: i64,
    end: i64,
}

impl ContainerReader for Frames {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(&mut self, _track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError> {
        self.next = pts.clamp(self.first, self.end - 1);
        Ok(Some(self.next))
    }

    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError> {
        if self.next >= self.end {
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

/// Queued packet times, whether input ended, and whether pictures are surfaces.
struct Session(VecDeque<i64>, bool, bool);

#[derive(Debug)]
struct Surface;

impl SurfaceLease for Surface {
    fn retained_bytes(&self) -> u64 {
        0
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct NoImport;

/// Imports nothing and counts its collections.
#[derive(Default)]
struct Collecting(std::sync::atomic::AtomicUsize);

impl SurfaceImport for Collecting {
    fn prepare(
        &self,
        _renderer: &Renderer,
        _size: RenderSize,
        _color: ColorPolicy,
        _facts: &FrameFacts,
        _lease: &Arc<dyn SurfaceLease>,
    ) -> Result<GpuSource, String> {
        Err("not imported".into())
    }

    fn collect(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl SurfaceImport for NoImport {
    fn prepare(
        &self,
        _renderer: &Renderer,
        _size: RenderSize,
        _color: ColorPolicy,
        _facts: &FrameFacts,
        _lease: &Arc<dyn SurfaceLease>,
    ) -> Result<GpuSource, String> {
        Err("not imported".into())
    }
}

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
                let luma = u8::try_from(16 + 6 * pts % 216).expect("small");
                let plane = |width: u32, value: u8| Plane {
                    bytes: Bytes::from(vec![value; (width * width) as usize]),
                    stride: width,
                    width,
                    height: width,
                };
                let image = if self.2 {
                    BackendImage::Surface(Arc::new(Surface))
                } else {
                    BackendImage::Cpu {
                        format: YuvFormat::I420,
                        planes: vec![
                            plane(SIZE, luma),
                            plane(SIZE / 2, 128),
                            plane(SIZE / 2, 128),
                        ],
                    }
                };
                Poll::Frame(BackendFrame {
                    pts,
                    image,
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

/// Software sessions; their pictures are surfaces when set.
struct Sessions(bool);

impl VideoBackends for Sessions {
    fn open(
        &self,
        _track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        if kind == BackendKind::Hardware {
            return Err(BackendError::Unsupported("software only".into()));
        }
        Ok(Box::new(Session(VecDeque::new(), false, self.0)))
    }
}

/// 30 frames at 30 fps from time 0.
fn reader() -> Box<dyn ContainerReader> {
    late_reader(30, 0, 30)
}

/// Frames at `first..end` on a clock of `timescale` units per second, one frame per unit.
fn late_reader(timescale: u32, first: i64, end: i64) -> Box<dyn ContainerReader> {
    Box::new(frames(timescale, first, end))
}

fn frames(timescale: u32, first: i64, end: i64) -> Frames {
    Frames {
        tracks: vec![TrackInfo {
            id: TrackId(1),
            kind: TrackKind::Video,
            timescale,
            duration: Some(end),
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
                reorder_depth: None,
            }),
            audio: None,
        }],
        next: first,
        first,
        end,
    }
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
        data: Vec::new(),
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
    export_from(reader, offset, frames)
}

fn export_from(
    reader: impl Fn() -> Box<dyn ContainerReader> + 'static,
    offset: i64,
    frames: i64,
) -> Vec<(u64, u8)> {
    let snapshot = project(offset, frames);
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let renderer = CpuRenderer::new(2);
    let mut sources = StreamSources::new(|_: &SourceId| {
        VideoStream::open(
            reader(),
            0,
            Arc::new(Sessions(false)),
            StreamOptions::default(),
        )
        .map(|stream| SourceStream::Video(Box::new(stream)))
        .map_err(|error| error.to_string())
    });
    let mut sink = Collect::default();
    let mut reports = Vec::new();
    // Memory warnings during the export: the budget falls to its floor and every frame still
    // renders.
    let budget = FrameBudget::new(1 << 30);
    export_video(
        &snapshot,
        &mut exporter,
        ExportRenderer::Cpu(&renderer),
        &mut TextTextures::new(),
        &mut sources,
        &mut sink,
        (&budget, &CancelToken::default()),
        &mut |_| Ok(()),
        &mut |done, total| {
            budget.halve();
            reports.push((done, total));
        },
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

/// The output frame index at which each run of equal lumas in `frames` starts.
fn changes(frames: &[(u64, u8)]) -> Vec<u64> {
    let mut starts = vec![frames[0].0];
    for pair in frames.windows(2) {
        assert!(pair[0].1 <= pair[1].1, "{frames:?}");
        if pair[1].1 != pair[0].1 {
            starts.push(pair[1].0);
        }
    }
    starts
}

#[test]
fn a_source_whose_video_starts_late_holds_its_first_frame_until_its_time() {
    let frames = export_from(|| late_reader(30, 15, 30), 0, 30);
    assert_eq!(frames.len(), 30);
    assert_eq!(
        changes(&frames),
        [0].into_iter().chain(16..30).collect::<Vec<_>>()
    );
}

#[test]
fn a_late_source_at_twice_the_output_rate_holds_its_first_frame_until_its_time() {
    // Output frame `i` wants source frame `2 i`; the first is at 10, so outputs 0 to 5 show it.
    let frames = export_from(|| late_reader(60, 10, 40), 0, 15);
    assert_eq!(frames.len(), 15);
    assert_eq!(
        changes(&frames),
        [0].into_iter().chain(6..15).collect::<Vec<_>>()
    );
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
        VideoStream::open(
            reader(),
            0,
            Arc::new(Sessions(false)),
            StreamOptions::default(),
        )
        .map(|stream| SourceStream::Video(Box::new(stream)))
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
        (&FrameBudget::new(1 << 30), &cancel),
        &mut |_| Ok(()),
        &mut |_, _| {},
    );
    assert!(matches!(result, Err(ExportError::Cancelled)), "{result:?}");
}

#[test]
fn the_frame_hook_runs_before_every_turn_and_its_failure_stops_the_export() {
    let snapshot = project(0, 5);
    let run = |fail_at: Option<usize>| {
        let mut exporter = Exporter::for_project(
            &snapshot,
            QualityProfile::export(),
            alpha2_media::VideoCodec::H264,
        )
        .expect("exporter");
        let renderer = CpuRenderer::new(1);
        let mut sources = StreamSources::new(|_: &SourceId| {
            VideoStream::open(
                reader(),
                0,
                Arc::new(Sessions(false)),
                StreamOptions::default(),
            )
            .map(|stream| SourceStream::Video(Box::new(stream)))
            .map_err(|error| error.to_string())
        });
        let mut sink = Collect::default();
        let mut calls = 0;
        let result = export_video(
            &snapshot,
            &mut exporter,
            ExportRenderer::Cpu(&renderer),
            &mut TextTextures::new(),
            &mut sources,
            &mut sink,
            (&FrameBudget::new(1 << 30), &CancelToken::default()),
            &mut |sink| {
                sink.settle()?;
                if fail_at == Some(calls) {
                    return Err(EncodeError::Adapter("held".into()));
                }
                calls += 1;
                Ok(())
            },
            &mut |_, _| {},
        );
        (result, calls, sink.0.len())
    };
    let (result, calls, frames) = run(None);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(frames, 5);
    assert!(calls > frames, "{calls} turns for {frames} frames");
    let (result, _, frames) = run(Some(2));
    assert!(matches!(result, Err(ExportError::Encode(_))), "{result:?}");
    assert_eq!(frames, 2);
}

/// Exports a project whose source decodes into surfaces, with the CPU renderer.
fn export_surfaces(import: Option<Arc<dyn SurfaceImport>>) -> Result<(), ExportError> {
    let snapshot = project(0, 3);
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let renderer = CpuRenderer::new(1);
    let mut sources = StreamSources::new(|_: &SourceId| {
        VideoStream::open(
            reader(),
            0,
            Arc::new(Sessions(true)),
            StreamOptions::default(),
        )
        .map(|stream| SourceStream::Video(Box::new(stream)))
        .map_err(|error| error.to_string())
    });
    if let Some(import) = import {
        sources = sources.with_surface_import(import);
    }
    export_video(
        &snapshot,
        &mut exporter,
        ExportRenderer::Cpu(&renderer),
        &mut TextTextures::new(),
        &mut sources,
        &mut Collect::default(),
        (&FrameBudget::new(1 << 30), &CancelToken::default()),
        &mut |_| Ok(()),
        &mut |_, _| {},
    )
}

#[test]
fn a_surface_frame_nothing_imports_fails_its_source() {
    let result = export_surfaces(None);
    assert!(
        matches!(&result, Err(ExportError::Source { reason, .. }) if reason.contains("nothing here imports")),
        "{result:?}"
    );
}

#[test]
fn the_cpu_renderer_refuses_surface_frames() {
    let result = export_surfaces(Some(Arc::new(NoImport)));
    assert!(
        matches!(&result, Err(ExportError::Source { reason, .. }) if reason.contains("CPU renderer cannot draw")),
        "{result:?}"
    );
}

/// Pictures in presentation order, one per packet at the packet's time, released once a window
/// deeper than any corpus file's reordering is full.
#[derive(Default)]
struct Ordered {
    held: std::collections::BinaryHeap<std::cmp::Reverse<i64>>,
    finished: bool,
}

impl VideoDecodeBackend for Ordered {
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }

    fn presentation_order(&self) -> bool {
        true
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        self.held.push(std::cmp::Reverse(packet.pts));
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<Poll, BackendError> {
        if self.held.len() <= 16 && !self.finished {
            return Ok(Poll::Pending);
        }
        let Some(std::cmp::Reverse(pts)) = self.held.pop() else {
            return Ok(Poll::Drained);
        };
        let plane = |width: u32| Plane {
            bytes: Bytes::from(vec![128; (width * width) as usize]),
            stride: width,
            width,
            height: width,
        };
        Ok(Poll::Frame(BackendFrame {
            pts,
            image: BackendImage::Cpu {
                format: YuvFormat::I420,
                planes: vec![plane(SIZE), plane(SIZE / 2), plane(SIZE / 2)],
            },
            coded: (SIZE, SIZE),
            visible: None,
            sample_aspect_ratio: None,
            color: Some(alpha2_media::ColorMetadata::EXPORT),
            chroma_location: Some(alpha2_media::ChromaLocation::Left),
        }))
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.finished = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.held.clear();
        self.finished = false;
        Ok(())
    }
}

struct OrderedSessions;

impl VideoBackends for OrderedSessions {
    fn open(
        &self,
        _track: &TrackInfo,
        _kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        Ok(Box::new(Ordered::default()))
    }
}

fn corpus_stream(path: &std::path::Path) -> VideoStream {
    VideoStream::open_file(path, Arc::new(OrderedSessions), StreamOptions::default())
        .expect("opens")
}

/// The source frame the stream cursor shows at each of `times`, asked in the given order.
fn cursor_picks(path: &std::path::Path, frames: &[i64], times: &[MediaTime]) -> Vec<usize> {
    let path = path.to_owned();
    let mut sources = StreamSources::new(move |_: &SourceId| {
        Ok(SourceStream::Video(Box::new(corpus_stream(&path))))
    });
    let need = |media_time| FrameNeed {
        reader: None,
        clip_id: ClipId::new("c").expect("id"),
        source_id: SourceId::new("v").expect("id"),
        media_time,
    };
    times
        .iter()
        .zip(0_u64..)
        .map(|(&time, index)| {
            let Some(SourceFrame::Cpu(frame)) = sources
                .frame(&need(time), index, &CancelToken::default())
                .expect("decodes")
            else {
                panic!("a CPU frame");
            };
            frames
                .iter()
                .position(|&ticks| ticks == frame.pts.ticks())
                .expect("a source frame")
        })
        .collect()
}

#[test]
fn the_stream_cursor_shows_the_frames_of_the_frame_selection_table() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let table: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("tests/golden/frame-selection-corpus.json"))
            .expect("tests/golden/frame-selection-corpus.json"),
    )
    .expect("json");
    let int = |value: &serde_json::Value| value.as_i64().expect("an integer");
    let mut replayed = 0;
    for source in table["sources"].as_array().expect("sources") {
        let name = source["file"].as_str().expect("file");
        // No in-house reader opens MPEG-TS.
        if name == "frame-rounding-23976.ts" {
            continue;
        }
        let path = root.join("tests/corpus").join(name);
        let frames = source["frameTicks"]
            .as_array()
            .expect("frameTicks")
            .iter()
            .map(int)
            .collect::<Vec<_>>();
        assert_eq!(
            corpus_stream(&path).presentation_slack().ticks(),
            int(&source["slackTicks"]),
            "{name}: slack"
        );
        for rate in source["rates"].as_array().expect("rates") {
            let frame_rate = FrameRate::new(
                u32::try_from(int(&rate["rate"]["num"])).expect("num"),
                u32::try_from(int(&rate["rate"]["den"])).expect("den"),
            )
            .expect("rate");
            let expected = rate["frames"]
                .as_array()
                .expect("frames")
                .iter()
                .map(|index| usize::try_from(int(index)).expect("index"))
                .collect::<Vec<_>>();
            let times = (0..expected.len())
                .map(|index| {
                    frame_rate
                        .frame_start(i64::try_from(index).expect("index"))
                        .expect("time")
                })
                .collect::<Vec<_>>();
            let label = format!("{name} at {frame_rate:?}");
            assert_eq!(
                cursor_picks(&path, &frames, &times),
                expected,
                "{label}, in order"
            );
            let reversed = times.iter().rev().copied().collect::<Vec<_>>();
            let mut sought = cursor_picks(&path, &frames, &reversed);
            sought.reverse();
            assert_eq!(sought, expected, "{label}, in reverse");
            replayed += 1;
        }
    }
    assert_eq!(replayed, 13 * 5);
}

/// Software sessions over [`Session`] that count how many are alive.
#[derive(Clone, Default)]
struct Counted {
    live: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
    opened: Arc<std::sync::atomic::AtomicUsize>,
}

struct Tracked(Session, Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Tracked {
    fn drop(&mut self) {
        self.1.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl VideoDecodeBackend for Tracked {
    fn kind(&self) -> BackendKind {
        self.0.kind()
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        self.0.submit(packet)
    }

    fn receive(&mut self, wait: bool) -> Result<Poll, BackendError> {
        self.0.receive(wait)
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.0.finish_input()
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.0.flush()
    }
}

impl VideoBackends for Counted {
    fn open(
        &self,
        _track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        use std::sync::atomic::Ordering;
        if kind == BackendKind::Hardware {
            return Err(BackendError::Unsupported("software only".into()));
        }
        let live = self.live.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(live, Ordering::Relaxed);
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Tracked(
            Session(VecDeque::new(), false, false),
            Arc::clone(&self.live),
        )))
    }
}

/// A 16x16 project at 30 fps of video clips of source `v`: `(start, frames, offset)` in frames.
fn clips_project(clips: &[(i64, i64, i64)]) -> EngineProject {
    let frame = |count: i64| MediaTime::from_ticks(count * alpha2_time::TICKS_PER_SECOND / 30);
    let source = SourceId::new("v").expect("id");
    let mut track = Track::new(
        ModelTrackId::new("t").expect("id"),
        "video",
        ModelTrackKind::Video,
    );
    let mut overlay = Track::new(
        ModelTrackId::new("o").expect("id"),
        "overlay",
        ModelTrackKind::Video,
    );
    let mut end = 0;
    for (index, &(start, frames, offset)) in clips.iter().enumerate() {
        let clip = Clip::new(
            ClipId::new(format!("c{index}")).expect("id"),
            source.clone(),
            TimeRange::new(frame(start), frame(start + frames)).expect("range"),
            TimeRange::new(frame(offset), frame(offset + frames)).expect("range"),
        );
        // Clips that overlap the one before go to the second track.
        if start < end {
            overlay.clips.push(clip);
        } else {
            track.clips.push(clip);
        }
        end = end.max(start + frames);
    }
    EngineProject::new(Project {
        data: Vec::new(),
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("p").expect("id"),
        name: "clips".into(),
        settings: ProjectSettings::new(SIZE, SIZE, FrameRate::new(30, 1).expect("rate"), 48_000)
            .expect("settings"),
        sources: vec![MediaSource::new(source, "asset://v", MediaKind::Video)],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline {
            tracks: vec![track, overlay],
        },
        audio: alpha2_model::MasterAudio::default(),
    })
    .expect("project")
}

/// Exports `snapshot` with sources from [`Counted`] sessions over 60 frames at 30 fps; `managed`
/// closes finished cursors and limits the open streams as a mobile export does.
fn export_counted(snapshot: &EngineProject, sessions: &Counted, managed: bool) -> Vec<(u64, u8)> {
    export_counted_with(snapshot, sessions, managed, None)
}

fn export_counted_with(
    snapshot: &EngineProject,
    sessions: &Counted,
    managed: bool,
    import: Option<Arc<dyn SurfaceImport>>,
) -> Vec<(u64, u8)> {
    let mut exporter = Exporter::for_project(
        snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let renderer = CpuRenderer::new(2);
    let backends = sessions.clone();
    let mut sources = StreamSources::new(move |_: &SourceId| {
        VideoStream::open(
            late_reader(30, 0, 60),
            0,
            Arc::new(backends.clone()),
            StreamOptions::default(),
        )
        .map(|stream| SourceStream::Video(Box::new(stream)))
        .map_err(|error| error.to_string())
    });
    if managed {
        let range = snapshot.timeline_range().expect("range");
        let (peak, reads) = crate::cursor::visual_decode_reads(snapshot.project(), range);
        sources = sources.with_reads(reads).with_session_limit(peak + 2);
    }
    if let Some(import) = import {
        sources = sources.with_surface_import(import);
    }
    let mut sink = Collect::default();
    export_video(
        snapshot,
        &mut exporter,
        ExportRenderer::Cpu(&renderer),
        &mut TextTextures::new(),
        &mut sources,
        &mut sink,
        (&FrameBudget::new(1 << 30), &CancelToken::default()),
        &mut |_| Ok(()),
        &mut |_, _| {},
    )
    .expect("export");
    drop(sources);
    sink.0
}

fn peak(sessions: &Counted) -> usize {
    sessions.peak.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn back_to_back_clips_of_one_source_keep_one_session_open() {
    let snapshot = clips_project(&(0..40).map(|clip| (clip, 1, clip)).collect::<Vec<_>>());
    let managed = Counted::default();
    let frames = export_counted(&snapshot, &managed, true);
    assert_eq!(frames.len(), 40);
    assert_eq!(peak(&managed), 1, "sessions open at once");
    assert_eq!(
        frames,
        export_counted(&snapshot, &Counted::default(), false)
    );
}

#[test]
fn closing_a_finished_cursor_lets_the_surface_import_collect() {
    let snapshot = clips_project(&(0..40).map(|clip| (clip, 1, clip)).collect::<Vec<_>>());
    let import = Arc::new(Collecting::default());
    let frames = export_counted_with(
        &snapshot,
        &Counted::default(),
        true,
        Some(Arc::clone(&import) as Arc<dyn SurfaceImport>),
    );
    assert_eq!(frames.len(), 40);
    let collected = import.0.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        collected >= 39,
        "{collected} collections for 39 closed cursors"
    );
}

#[test]
fn overlapping_clips_of_one_source_keep_a_session_each() {
    let snapshot = clips_project(&[(0, 10, 0), (5, 10, 20), (15, 10, 5)]);
    let managed = Counted::default();
    let frames = export_counted(&snapshot, &managed, true);
    assert_eq!(frames.len(), 25);
    assert_eq!(peak(&managed), 2, "sessions open at once");
    assert_eq!(
        frames,
        export_counted(&snapshot, &Counted::default(), false)
    );
}

#[test]
fn random_reads_over_more_cursors_than_the_limit_show_the_same_frames() {
    let sources = |sessions: &Counted, limit: Option<usize>| {
        let backends = sessions.clone();
        let sources = StreamSources::new(move |_: &SourceId| {
            VideoStream::open(
                late_reader(30, 0, 60),
                0,
                Arc::new(backends.clone()),
                StreamOptions::default(),
            )
            .map(|stream| SourceStream::Video(Box::new(stream)))
            .map_err(|error| error.to_string())
        });
        match limit {
            Some(limit) => sources.with_session_limit(limit),
            None => sources,
        }
    };
    let (limited, unlimited) = (Counted::default(), Counted::default());
    let (mut few, mut many) = (sources(&limited, Some(3)), sources(&unlimited, None));
    // A fixed pseudo-random walk: one cursor of ten per output frame, at a time 0 to 59 frames in.
    let mut state = 0x2545_f491_u64;
    let mut next = |below: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % below
    };
    let shown = |sources: &mut StreamSources<_>, need: &FrameNeed, index| match sources
        .frame(need, index, &CancelToken::default())
        .expect("decodes")
    {
        Some(SourceFrame::Cpu(frame)) => frame.pts,
        other => panic!("{other:?}"),
    };
    for index in 0..300 {
        let need = FrameNeed {
            reader: None,
            clip_id: ClipId::new(format!("c{}", next(10))).expect("id"),
            source_id: SourceId::new("v").expect("id"),
            media_time: MediaTime::from_ticks(
                i64::try_from(next(60)).expect("small") * alpha2_time::TICKS_PER_SECOND / 30,
            ),
        };
        assert_eq!(
            shown(&mut few, &need, index),
            shown(&mut many, &need, index)
        );
        assert!(few.open_streams() <= 3, "{} streams", few.open_streams());
    }
    assert!(peak(&limited) <= 3, "{} sessions", peak(&limited));
    assert_eq!(peak(&unlimited), 10);
}

#[test]
fn every_clip_of_an_image_source_shares_one_decoded_still() {
    let opened = std::rc::Rc::new(std::cell::Cell::new(0));
    let count = std::rc::Rc::clone(&opened);
    let still = Arc::new(DecodedVideoFrame {
        pts: MediaTime::ZERO,
        dts: None,
        duration: None,
        visible_rect: alpha2_media::Rect {
            x: 0,
            y: 0,
            width: SIZE,
            height: SIZE,
        },
        sample_aspect_ratio: alpha2_media::SampleAspectRatio { num: 1, den: 1 },
        orientation: Orientation::default(),
        pixel_format: alpha2_media::PixelFormat::Yuv(YuvFormat::I420),
        chroma_location: alpha2_media::ChromaLocation::Left,
        color: Some(alpha2_media::ColorMetadata::EXPORT),
        planes: Vec::new(),
    });
    let mut sources = StreamSources::new(move |_: &SourceId| {
        count.set(count.get() + 1);
        Ok(SourceStream::Still(Arc::new((*still).clone())))
    })
    .with_session_limit(3);
    for clip in 0..50_u64 {
        let need = FrameNeed {
            reader: None,
            clip_id: ClipId::new(format!("c{clip}")).expect("id"),
            source_id: SourceId::new("v").expect("id"),
            media_time: MediaTime::ZERO,
        };
        sources
            .frame(&need, clip, &CancelToken::default())
            .expect("frame");
    }
    assert_eq!(opened.get(), 1);
}

#[test]
fn the_frames_a_cursor_holds_count_against_the_budget() {
    let mut sources = StreamSources::new(|_: &SourceId| {
        VideoStream::open(
            reader(),
            0,
            Arc::new(Sessions(false)),
            StreamOptions::default(),
        )
        .map(|stream| SourceStream::Video(Box::new(stream)))
        .map_err(|error| error.to_string())
    });
    assert_eq!(sources.held_bytes(), 0);
    let need = FrameNeed {
        reader: None,
        clip_id: ClipId::new("c").expect("id"),
        source_id: SourceId::new("v").expect("id"),
        media_time: MediaTime::ZERO,
    };
    sources
        .frame(&need, 0, &CancelToken::default())
        .expect("frame");
    // The frame answered and the one decoded after it, 16x16 I420 each.
    assert_eq!(sources.held_bytes(), 2 * (16 * 16 + 2 * 8 * 8));
}

#[test]
fn a_track_that_declares_alpha_is_reported_as_dropped_once_per_source() {
    use alpha2_media::{Decoder as _, SourceAlpha};
    let open = |_: &SourceId| {
        let mut alpha = frames(30, 0, 60);
        if let Some(video) = alpha.tracks[0].video.as_mut() {
            video.alpha = true;
        }
        VideoStream::open(
            Box::new(alpha),
            0,
            Arc::new(Counted::default()),
            StreamOptions::default(),
        )
        .map_err(|error| error.to_string())
    };
    let stream = open(&SourceId::new("v").expect("id")).expect("stream");
    assert_eq!(stream.streams()[0].alpha, Some(SourceAlpha::Vp9SideData));
    assert_eq!(
        stream.dropped_layer().map(|dropped| dropped.code),
        Some("alpha-dropped")
    );
    // Three cursors over one stream slot: every cursor reopens the source many times.
    let snapshot = clips_project(&[(0, 10, 0), (5, 10, 20), (15, 10, 5)]);
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let mut sources = StreamSources::new(move |source: &SourceId| {
        open(source).map(|stream| SourceStream::Video(Box::new(stream)))
    })
    .with_session_limit(1);
    export_video(
        &snapshot,
        &mut exporter,
        ExportRenderer::Cpu(&CpuRenderer::new(2)),
        &mut TextTextures::new(),
        &mut sources,
        &mut Collect::default(),
        (&FrameBudget::new(1 << 30), &CancelToken::default()),
        &mut |_| Ok(()),
        &mut |_, _| {},
    )
    .expect("export");
    let dropped = sources.dropped_layers();
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(
        dropped[&SourceId::new("v").expect("id")].code,
        "alpha-dropped"
    );
}

#[test]
fn an_opaque_track_drops_nothing() {
    let stream = VideoStream::open(
        reader(),
        0,
        Arc::new(Counted::default()),
        StreamOptions::default(),
    )
    .expect("stream");
    assert_eq!(stream.dropped_layer(), None);
}
