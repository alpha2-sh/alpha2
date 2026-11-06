use std::collections::VecDeque;
use std::sync::Mutex;

use alpha2_container::{
    ContainerReader, DemuxError, Packet, TrackId, TrackInfo, TrackKind, VideoTrack,
};
use bytes::Bytes;

use super::*;

/// One packet: presentation and decode time in frames, keyframe, and whether it needs the packet
/// before it (a leading frame of an open GOP).
#[derive(Clone, Copy, Debug)]
struct Script {
    pts: i64,
    dts: i64,
    key: bool,
    leading: bool,
}

const fn packet(pts: i64, dts: i64, key: bool) -> Script {
    Script {
        pts,
        dts,
        key,
        leading: false,
    }
}

const fn leading(pts: i64, dts: i64) -> Script {
    Script {
        pts,
        dts,
        key: false,
        leading: true,
    }
}

/// A container whose index finds keyframes by decode time when `by_dts`, as MPEG-TS indexes
/// do, else by presentation time.
struct FakeReader {
    tracks: Vec<TrackInfo>,
    packets: Vec<Script>,
    next: usize,
    by_dts: bool,
    /// Bytes of every packet.
    bytes: usize,
}

fn track() -> TrackInfo {
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
            codec: VideoCodec::Vp9,
            codec_private: Bytes::new(),
            in_band_parameter_sets: false,
            width: 16,
            height: 16,
            orientation: Orientation::default(),
            sample_aspect_ratio: None,
            color: None,
            clean_aperture: None,
            frame_rate: None,
            alpha: false,
            reorder_depth: None,
        }),
        audio: None,
    }
}

impl FakeReader {
    fn boxed(packets: &[Script], by_dts: bool) -> Box<dyn ContainerReader> {
        Self::of(track(), packets, by_dts, 1)
    }

    fn of(
        track: TrackInfo,
        packets: &[Script],
        by_dts: bool,
        bytes: usize,
    ) -> Box<dyn ContainerReader> {
        Box::new(Self {
            tracks: vec![track],
            packets: packets.to_vec(),
            next: 0,
            by_dts,
            bytes,
        })
    }
}

impl ContainerReader for FakeReader {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(&mut self, _track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError> {
        let key = |script: &Script| if self.by_dts { script.dts } else { script.pts };
        let found = self
            .packets
            .iter()
            .enumerate()
            .filter(|(_, script)| script.key && key(script) <= pts)
            .map(|(index, _)| index)
            .next_back()
            .unwrap_or(0);
        self.next = found;
        Ok(self.packets.get(found).map(|script| script.pts))
    }

    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError> {
        let Some(script) = self.packets.get(self.next).copied() else {
            return Ok(None);
        };
        self.next += 1;
        Ok(Some(Packet {
            track,
            pts: script.pts,
            dts: script.dts,
            duration: 1,
            keyframe: script.key,
            data: Bytes::from(vec![u8::from(script.leading); self.bytes.max(1)]),
            discard: (0, 0),
        }))
    }
}

/// What a fake session does.
#[derive(Clone, Default)]
struct Behavior {
    /// Pictures out in presentation order, as `MediaCodec` gives them, else decode order.
    presentation_order: bool,
    /// Hardware sessions refuse to open.
    no_hardware: bool,
    /// The session fails with this error after this many packets.
    fail_after: Option<(usize, BackendError)>,
    /// Packets with this presentation time are damaged.
    corrupt_pts: Option<i64>,
}

struct FakeSession {
    kind: BackendKind,
    behavior: Behavior,
    out: VecDeque<i64>,
    held: Vec<i64>,
    since_flush: usize,
    /// Whether the GOP being decoded has the GOP before it decoded too, which its leading
    /// pictures reference.
    has_previous_gop: bool,
    submitted: usize,
    finished: bool,
}

impl VideoDecodeBackend for FakeSession {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        if let Some((after, error)) = &self.behavior.fail_after
            && self.submitted >= *after
        {
            return Err(error.clone());
        }
        if self.out.len() > 4 {
            return Ok(Submit::Busy);
        }
        self.submitted += 1;
        if self.behavior.corrupt_pts == Some(packet.pts) {
            return Err(BackendError::Corrupt(format!("packet {}", packet.pts)));
        }
        let leading = packet.data.first() == Some(&1);
        if packet.keyframe {
            self.has_previous_gop = self.since_flush > 0;
        }
        if leading && !self.has_previous_gop {
            return Ok(Submit::Accepted);
        }
        self.since_flush += 1;
        if self.behavior.presentation_order {
            self.held.push(packet.pts);
            self.held.sort_unstable();
            if self.held.len() > 2 {
                let first = self.held.remove(0);
                self.out.push_back(first);
            }
        } else {
            self.out.push_back(packet.pts);
        }
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<Poll, BackendError> {
        if let Some(pts) = self.out.pop_front() {
            return Ok(Poll::Frame(picture(pts)));
        }
        if self.finished {
            if self.held.is_empty() {
                return Ok(Poll::Drained);
            }
            let first = self.held.remove(0);
            return Ok(Poll::Frame(picture(first)));
        }
        Ok(Poll::Pending)
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.finished = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.out.clear();
        self.held.clear();
        self.since_flush = 0;
        self.has_previous_gop = false;
        self.finished = false;
        Ok(())
    }
}

fn picture(pts: i64) -> BackendFrame {
    BackendFrame {
        pts,
        image: BackendImage::Cpu {
            format: YuvFormat::I420,
            planes: Vec::new(),
        },
        coded: (16, 16),
        visible: None,
        sample_aspect_ratio: None,
        color: None,
        chroma_location: None,
    }
}

#[derive(Default)]
struct FakeBackends {
    behavior: Behavior,
    opened: Mutex<Vec<BackendKind>>,
}

impl VideoBackends for FakeBackends {
    fn open(
        &self,
        _track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        if kind == BackendKind::Hardware && self.behavior.no_hardware {
            return Err(BackendError::Unsupported("no hardware decoder".into()));
        }
        let mut opened = self.opened.lock().expect("lock");
        let mut behavior = self.behavior.clone();
        // Only the first session fails; its replacement works.
        if !opened.is_empty() {
            behavior.fail_after = None;
        }
        opened.push(kind);
        Ok(Box::new(FakeSession {
            kind,
            behavior,
            out: VecDeque::new(),
            held: Vec::new(),
            since_flush: 0,
            has_previous_gop: false,
            submitted: 0,
            finished: false,
        }))
    }
}

/// I P B B P B B in decode order, presenting 0..=6.
const B_FRAMES: [Script; 7] = [
    packet(0, -1, true),
    packet(3, 0, false),
    packet(1, 1, false),
    packet(2, 2, false),
    packet(6, 3, false),
    packet(4, 4, false),
    packet(5, 5, false),
];

fn open(packets: &[Script], by_dts: bool, behavior: Behavior) -> (VideoStream, Arc<FakeBackends>) {
    open_reader(FakeReader::boxed(packets, by_dts), behavior)
}

fn open_reader(
    reader: Box<dyn ContainerReader>,
    behavior: Behavior,
) -> (VideoStream, Arc<FakeBackends>) {
    let backends = Arc::new(FakeBackends {
        behavior,
        opened: Mutex::default(),
    });
    let stream =
        VideoStream::open(reader, 0, backends.clone(), StreamOptions::default()).expect("open");
    (stream, backends)
}

/// The fake track reordered at most `depth` pictures deep.
fn track_with_depth(depth: usize) -> TrackInfo {
    let mut track = track();
    if let Some(video) = track.video.as_mut() {
        video.reorder_depth = Some(depth);
    }
    track
}

fn frames(stream: &mut VideoStream) -> Vec<i64> {
    let cancel = CancelToken::default();
    let mut shown = Vec::new();
    while let Some(frame) = stream.next(&cancel).expect("next") {
        shown.push(stream.track_units(frame.facts.pts));
    }
    shown
}

fn ticks(frames: i64) -> MediaTime {
    MediaTime::from_ticks(frames * TICKS_PER_SECOND / 30)
}

#[test]
fn decode_order_output_is_put_back_in_presentation_order() {
    for presentation_order in [false, true] {
        let (mut stream, _) = open(
            &B_FRAMES,
            false,
            Behavior {
                presentation_order,
                ..Behavior::default()
            },
        );
        assert_eq!(frames(&mut stream), [0, 1, 2, 3, 4, 5, 6]);
    }
}

#[test]
fn a_seek_answers_with_the_frame_shown_at_its_time_and_keeps_the_next() {
    let (mut stream, _) = open(&B_FRAMES, false, Behavior::default());
    let cancel = CancelToken::default();
    let slack = stream.presentation_slack();
    let answer = |stream: &mut VideoStream, time: MediaTime| {
        let frame = stream.seek(time, &cancel).expect("seek");
        stream.track_units(frame.facts.pts)
    };
    assert_eq!(answer(&mut stream, ticks(4)), 4);
    assert_eq!(
        stream
            .peek_pts(&cancel)
            .expect("peek")
            .map(|pts| stream.track_units(pts)),
        Some(5)
    );
    assert_eq!(
        answer(&mut stream, MediaTime::from_ticks(ticks(4).ticks() + 1_000)),
        4
    );
    assert_eq!(
        answer(
            &mut stream,
            MediaTime::from_ticks(ticks(5).ticks() - slack.ticks())
        ),
        5
    );
    assert_eq!(
        answer(
            &mut stream,
            MediaTime::from_ticks(ticks(5).ticks() - slack.ticks() - 1)
        ),
        4
    );
    // Before the first frame, the first; after the last, the last.
    assert_eq!(answer(&mut stream, MediaTime::from_ticks(-10_000)), 0);
    assert_eq!(answer(&mut stream, ticks(100)), 6);
    assert_eq!(frames(&mut stream), Vec::<i64>::new());
    assert_eq!(answer(&mut stream, ticks(2)), 2);
    assert_eq!(frames(&mut stream), [3, 4, 5, 6]);
}

#[test]
fn a_seek_steps_back_when_its_keyframe_leads_frames_that_come_later() {
    // Two GOPs; the second keyframe (presented at 12) leads two frames presented before it
    // that need the first GOP. An index by decode time finds it for a time at 11.
    let mut packets = (0..10)
        .map(|index| packet(index, index, index == 0))
        .collect::<Vec<_>>();
    packets.extend([
        packet(12, 10, true),
        leading(10, 11),
        leading(11, 12),
        packet(13, 13, false),
    ]);
    let (mut stream, _) = open(&packets, true, Behavior::default());
    let cancel = CancelToken::default();
    let frame = stream.seek(ticks(11), &cancel).expect("seek");
    assert_eq!(stream.track_units(frame.facts.pts), 11);
    assert_eq!(frames(&mut stream), [12, 13]);
    // Decoding from the second keyframe alone drops the leading pictures.
    let frame = stream.seek(ticks(12), &cancel).expect("seek");
    assert_eq!(stream.track_units(frame.facts.pts), 12);
}

#[test]
fn a_hardware_session_that_cannot_open_is_replaced_by_software() {
    let (mut stream, backends) = open(
        &B_FRAMES,
        false,
        Behavior {
            no_hardware: true,
            ..Behavior::default()
        },
    );
    assert!(!stream.on_hardware());
    assert_eq!(frames(&mut stream), [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(
        *backends.opened.lock().expect("lock"),
        [BackendKind::Software]
    );
}

#[test]
fn a_hardware_session_failing_mid_stream_is_replaced_once_without_gaps_or_repeats() {
    let (mut stream, backends) = open(
        &B_FRAMES,
        false,
        Behavior {
            fail_after: Some((4, BackendError::Failed("decoder fault".into()))),
            ..Behavior::default()
        },
    );
    assert_eq!(frames(&mut stream), [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(
        *backends.opened.lock().expect("lock"),
        [BackendKind::Hardware, BackendKind::Software]
    );
}

#[test]
fn a_lost_session_is_reopened_of_the_same_kind_where_the_stream_was() {
    let (mut stream, backends) = open(
        &B_FRAMES,
        false,
        Behavior {
            fail_after: Some((5, BackendError::SessionLost("backgrounded".into()))),
            presentation_order: true,
            ..Behavior::default()
        },
    );
    assert_eq!(frames(&mut stream), [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(
        *backends.opened.lock().expect("lock"),
        [BackendKind::Hardware, BackendKind::Hardware]
    );
}

#[test]
fn a_damaged_packet_fails_a_strict_decode_and_is_skipped_by_a_tolerant_one() {
    let packets = (0..8)
        .map(|index| packet(index, index, index == 0))
        .collect::<Vec<_>>();
    let behavior = Behavior {
        corrupt_pts: Some(3),
        ..Behavior::default()
    };
    let (mut strict, _) = open(&packets, false, behavior.clone());
    let cancel = CancelToken::default();
    let mut result = Ok(Some(()));
    while let Ok(Some(())) = result {
        result = strict.next(&cancel).map(|frame| frame.map(|_| ()));
    }
    assert!(matches!(result, Err(DecodeError::Adapter(_))), "{result:?}");

    let backends = Arc::new(FakeBackends {
        behavior,
        opened: Mutex::default(),
    });
    let mut tolerant = VideoStream::open(
        FakeReader::boxed(&packets, false),
        0,
        backends,
        StreamOptions {
            policy: DecodePolicy::Tolerant,
            ..StreamOptions::default()
        },
    )
    .expect("open");
    assert_eq!(frames(&mut tolerant), [0, 1, 2, 4, 5, 6, 7]);
    assert_eq!(tolerant.recovery().diagnostics.corrupt_packets.len(), 1);
}

#[test]
fn the_reorder_buffer_releases_a_run_once_a_later_picture_arrives() {
    let mut buffer = ReorderBuffer::default();
    for pts in [0, 3, 1, 2] {
        buffer.arrive(picture(pts));
    }
    assert_eq!(buffer.take().map(|frame| frame.pts), Some(0));
    assert_eq!(buffer.take().map(|frame| frame.pts), None);
    buffer.arrive(picture(6));
    let released = std::iter::from_fn(|| buffer.take().map(|frame| frame.pts)).collect::<Vec<_>>();
    assert_eq!(released, [1, 2, 3]);
    buffer.drain();
    assert_eq!(buffer.take().map(|frame| frame.pts), Some(6));
    assert!(buffer.is_empty());
}

#[test]
fn track_units_round_to_ticks_as_ffmpeg_does() {
    assert_eq!(to_media_time(1_001, 30_000), MediaTime::from_ticks(4_004));
    assert_eq!(to_media_time(1, 90_000), MediaTime::from_ticks(1));
    assert_eq!(to_media_time(-1_024, 44_100), MediaTime::from_ticks(-2_786));
}

#[test]
fn a_seek_steps_back_keyframe_by_keyframe_however_far_the_frames_lead() {
    // The second keyframe (decoded at 200, presented at 330) leads 130 frames presented before it
    // that need the first GOP, more than four seconds of them.
    let mut packets = (0..200)
        .map(|index| packet(index, index, index == 0))
        .collect::<Vec<_>>();
    packets.push(packet(330, 200, true));
    packets.extend((200..330).map(|pts| leading(pts, pts + 1)));
    let (mut stream, _) = open(&packets, true, Behavior::default());
    let cancel = CancelToken::default();
    let frame = stream.seek(ticks(329), &cancel).expect("seek");
    assert_eq!(stream.track_units(frame.facts.pts), 329);
    assert_eq!(frames(&mut stream), [330]);
}

#[test]
fn a_declared_reorder_depth_orders_pictures_a_run_cannot() {
    // I P P B B B B in decode order: two anchors ahead of their B-pictures, two deep.
    let script = [
        packet(0, -2, true),
        packet(3, -1, false),
        packet(6, 0, false),
        packet(1, 1, false),
        packet(2, 2, false),
        packet(4, 3, false),
        packet(5, 4, false),
    ];
    let (mut stream, _) = open_reader(
        FakeReader::of(track_with_depth(2), &script, false, 1),
        Behavior::default(),
    );
    assert_eq!(frames(&mut stream), [0, 1, 2, 3, 4, 5, 6]);
    let mut buffer = ReorderBuffer::new(Some(2));
    for pts in [0, 3, 6, 1] {
        buffer.arrive(picture(pts));
    }
    let released = std::iter::from_fn(|| buffer.take().map(|frame| frame.pts)).collect::<Vec<_>>();
    assert_eq!(released, [0, 1]);
}

#[test]
fn a_session_in_presentation_order_is_not_held_back() {
    struct InOrder(Box<dyn VideoDecodeBackend>);
    impl VideoDecodeBackend for InOrder {
        fn kind(&self) -> BackendKind {
            self.0.kind()
        }
        fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
            self.0.submit(packet)
        }
        fn receive(&mut self, wait: bool) -> Result<Poll, BackendError> {
            self.0.receive(wait)
        }
        fn presentation_order(&self) -> bool {
            true
        }
        fn finish_input(&mut self) -> Result<(), BackendError> {
            self.0.finish_input()
        }
        fn flush(&mut self) -> Result<(), BackendError> {
            self.0.flush()
        }
    }
    let session = FakeBackends::default()
        .open(&track(), BackendKind::Hardware)
        .expect("session");
    let session = InOrder(session);
    assert_eq!(reorder_depth(&session, &track_with_depth(4)), Some(0));
    assert_eq!(
        reorder_depth(session.0.as_ref(), &track_with_depth(4)),
        Some(4)
    );
}

#[test]
fn the_presentation_slack_reads_no_more_than_16_mib_of_first_packets() {
    // Eight exact 2 MiB frames, then millisecond-rounded 30 fps frames in a 1/16000 time base.
    let bytes = 2 << 20;
    let exact = usize::try_from(alpha2_time::ROUNDING_SCAN_BYTES).expect("bytes") / bytes;
    assert_eq!(exact, 8);
    let mut timestamps = (0..8).map(|index| index * 640).collect::<Vec<i64>>();
    let mut last = timestamps[exact - 1];
    for step in [528, 544, 528, 544] {
        last += step;
        timestamps.push(last);
    }
    let script = timestamps
        .iter()
        .enumerate()
        .map(|(index, &pts)| packet(pts, pts, index == 0))
        .collect::<Vec<_>>();
    let mut track = track();
    track.timescale = 16_000;
    let (stream, _) = open_reader(
        FakeReader::of(track.clone(), &script, false, bytes),
        Behavior::default(),
    );
    let scanned = alpha2_time::presentation_slack(
        1,
        16_000,
        alpha2_time::rounding_quantum(&timestamps[..exact]),
    );
    let unbounded =
        alpha2_time::presentation_slack(1, 16_000, alpha2_time::rounding_quantum(&timestamps));
    assert_ne!(scanned, unbounded, "the frames past 16 MiB show a rounding");
    assert_eq!(stream.presentation_slack(), scanned);
    // Small frames: the scan sees the millisecond rounding.
    let (stream, _) = open_reader(
        FakeReader::of(track, &script, false, 1),
        Behavior::default(),
    );
    assert_eq!(stream.presentation_slack(), unbounded);
}

#[test]
fn the_sps_window_applies_only_to_a_buffer_of_the_sps_coded_size() {
    let (mut stream, _) = open(&B_FRAMES, false, Behavior::default());
    stream.crop = Some(CropWindow {
        coded_width: 1920,
        coded_height: 1088,
        left: 0,
        right: 0,
        top: 0,
        bottom: 8,
    });
    let shown = |stream: &VideoStream, coded: (u32, u32), visible: Option<Rect>| {
        let mut frame = picture(0);
        frame.coded = coded;
        frame.visible = visible;
        stream.facts(frame).facts.visible_rect
    };
    let rect = |width, height| Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    assert_eq!(shown(&stream, (1920, 1088), None), rect(1920, 1080));
    assert_eq!(
        shown(&stream, (1920, 1088), Some(rect(1920, 1088))),
        rect(1920, 1080)
    );
    // Cropped by the session already.
    assert_eq!(shown(&stream, (1920, 1080), None), rect(1920, 1080));
    // Another size (an SPS sent in band): the session's rect, else the whole buffer.
    assert_eq!(
        shown(&stream, (1280, 736), Some(rect(1280, 720))),
        rect(1280, 720)
    );
    assert_eq!(shown(&stream, (1280, 736), None), rect(1280, 736));
}

#[test]
fn a_hardware_picture_failure_keeps_the_decoder_text() {
    let mut recovery = Recovery::new((1, 30));
    recovery.policy = DecodePolicy::Tolerant;
    recovery
        .corrupt_with_detail("video receive", None, Some("surface lost".into()))
        .expect("tolerated");
    assert_eq!(
        recovery.diagnostics.corrupt_packets[0].detail.as_deref(),
        Some("surface lost")
    );
}
