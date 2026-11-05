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
        }),
        audio: None,
    }
}

impl FakeReader {
    fn boxed(packets: &[Script], by_dts: bool) -> Box<dyn ContainerReader> {
        Box::new(Self {
            tracks: vec![track()],
            packets: packets.to_vec(),
            next: 0,
            by_dts,
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
            data: Bytes::from(vec![u8::from(script.leading)]),
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
    let backends = Arc::new(FakeBackends {
        behavior,
        opened: Mutex::default(),
    });
    let stream = VideoStream::open(
        FakeReader::boxed(packets, by_dts),
        0,
        backends.clone(),
        StreamOptions::default(),
    )
    .expect("open");
    (stream, backends)
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
    assert_eq!(answer(&mut stream, MediaTime::from_ticks(ticks(5).ticks() - slack.ticks())), 5);
    assert_eq!(
        answer(&mut stream, MediaTime::from_ticks(ticks(5).ticks() - slack.ticks() - 1)),
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
