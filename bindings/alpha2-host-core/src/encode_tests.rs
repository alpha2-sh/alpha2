use std::sync::{Arc, Mutex};

use alpha2_container::MuxError;
use alpha2_media::BitrateMode;

use super::*;
use crate::export::{PlaneBuffer, VideoSink};

/// What the writer saw, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Track(TrackKind, u32),
    Packet(TrackId, i64, bool),
    Finished,
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Event>>>);

impl Log {
    fn push(&self, event: Event) {
        self.0.lock().expect("log").push(event);
    }

    fn events(&self) -> Vec<Event> {
        self.0.lock().expect("log").clone()
    }
}

struct Writer {
    log: Log,
    tracks: u32,
    written: bool,
}

impl ContainerWriter for Writer {
    fn add_track(&mut self, track: &TrackInfo) -> Result<TrackId, MuxError> {
        assert!(!self.written, "tracks come before packets");
        let priming = track.audio.as_ref().map_or(0, |audio| audio.priming);
        self.log.push(Event::Track(track.kind, priming));
        self.tracks += 1;
        Ok(TrackId(self.tracks))
    }

    fn write(&mut self, packet: Packet) -> Result<(), MuxError> {
        self.written = true;
        self.log
            .push(Event::Packet(packet.track, packet.pts, packet.keyframe));
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), MuxError> {
        self.log.push(Event::Finished);
        Ok(())
    }
}

/// Holds two frames, gives each back one frame late, and publishes its configuration with the
/// first packet.
#[derive(Default)]
struct Video {
    queued: VecDeque<(i64, bool)>,
    given: bool,
    ended: bool,
}

impl VideoEncodeBackend for Video {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn hardware(&self) -> bool {
        true
    }

    fn timescale(&self) -> u32 {
        90_000
    }

    fn submit(
        &mut self,
        pts: i64,
        planes: &[PlaneBuffer],
        keyframe: bool,
    ) -> Result<Submit, EncodeError> {
        assert_eq!(planes.len(), 3);
        if self.queued.len() >= 2 {
            return Ok(Submit::Busy);
        }
        self.queued.push_back((pts, keyframe));
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<EncodePoll, EncodeError> {
        if self.queued.len() < 2 && !self.ended {
            return Ok(EncodePoll::Pending);
        }
        Ok(match self.queued.pop_front() {
            Some((pts, keyframe)) => {
                self.given = true;
                EncodePoll::Packet(EncodedPacket {
                    pts,
                    dts: pts,
                    keyframe,
                    data: Bytes::from_static(&[1]),
                })
            }
            None => EncodePoll::Drained,
        })
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        self.ended = true;
        Ok(())
    }

    fn codec_private(&self) -> Option<Bytes> {
        self.given.then(|| Bytes::from_static(&[1, 2, 3]))
    }

    fn applied_rate_control(&self) -> AppliedRateControl {
        AppliedRateControl::Bitrate
    }
}

/// Encodes 1024-sample frames at 48 kHz with 2112 samples of priming, one packet per frame.
#[derive(Default)]
struct Audio {
    packets: VecDeque<(i64, usize)>,
    ended: bool,
}

impl AudioEncodeBackend for Audio {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn sample_rate(&self) -> u32 {
        48_000
    }

    fn frame_size(&self) -> usize {
        1_024
    }

    fn submit(&mut self, pts: i64, planes: &[Vec<f32>]) -> Result<Submit, EncodeError> {
        assert_eq!(planes.len(), 2);
        self.packets.push_back((pts, planes[0].len()));
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<EncodePoll, EncodeError> {
        Ok(match self.packets.pop_front() {
            Some((pts, _)) => EncodePoll::Packet(EncodedPacket {
                pts,
                dts: pts,
                keyframe: true,
                data: Bytes::from_static(&[2]),
            }),
            None if self.ended => EncodePoll::Drained,
            None => EncodePoll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        self.ended = true;
        Ok(())
    }

    fn codec_private(&self) -> Option<Bytes> {
        Some(Bytes::from_static(&[0x11, 0x90]))
    }

    fn priming(&self) -> u32 {
        2_112
    }
}

fn video_config() -> VideoEncodeConfig {
    VideoEncodeConfig {
        codec: alpha2_media::VideoCodec::H264,
        width: 16,
        height: 16,
        frame_rate: FrameRate::new(30, 1).expect("rate"),
        layout: PixelLayout::YUV420P,
        rate: VideoRateControl {
            bitrate: 1_000_000,
            mode: BitrateMode::Variable,
            quantizer: None,
        },
        color: ColorMetadata::EXPORT,
        keyframe_interval: 2,
        software: false,
    }
}

fn audio_config() -> AudioEncodeConfig {
    AudioEncodeConfig {
        codec: alpha2_media::AudioCodec::Aac,
        project_rate: 48_000,
        speakers: SpeakerLayout::STEREO,
        bitrate: 128_000,
    }
}

fn planes() -> Vec<PlaneBuffer> {
    [16, 8, 8]
        .into_iter()
        .map(|size| PlaneBuffer {
            bytes: vec![0; size * size],
            stride: size,
            width: u32::try_from(size).expect("small"),
            height: u32::try_from(size).expect("small"),
        })
        .collect()
}

fn sink(log: &Log, audio: bool) -> EncodeSink {
    let writer = Box::new(Writer {
        log: log.clone(),
        tracks: 0,
        written: false,
    });
    let audio = audio.then(|| {
        (
            Box::new(Audio::default()) as Box<dyn AudioEncodeBackend>,
            audio_config(),
        )
    });
    EncodeSink::new(writer, Box::new(Video::default()), video_config(), audio)
}

fn write_frames(sink: &mut EncodeSink, count: u64) {
    for index in 0..count {
        sink.write_video(index, MediaTime::ZERO, PixelLayout::YUV420P, &planes())
            .expect("frame");
    }
}

#[test]
fn video_waits_for_its_configuration_and_is_timed_by_frame_index() {
    let log = Log::default();
    let mut sink = sink(&log, false);
    write_frames(&mut sink, 1);
    assert!(log.events().is_empty(), "no track before the configuration");
    for index in 1..5 {
        sink.write_video(index, MediaTime::ZERO, PixelLayout::YUV420P, &planes())
            .expect("frame");
    }
    sink.finish().expect("finish");
    let events = log.events();
    assert_eq!(events[0], Event::Track(TrackKind::Video, 0));
    let packets = events
        .iter()
        .filter_map(|event| match event {
            Event::Packet(_, pts, keyframe) => Some((*pts, *keyframe)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        packets,
        [
            (0, true),
            (3_000, false),
            (6_000, true),
            (9_000, false),
            (12_000, true)
        ]
    );
    assert_eq!(events.last(), Some(&Event::Finished));
}

#[test]
fn audio_is_cut_into_encoder_frames_and_shifted_back_by_its_priming() {
    let log = Log::default();
    let mut sink = sink(&log, true);
    let cancel = CancelToken::default();
    write_frames(&mut sink, 3);
    for start in [0_i64, 1_500] {
        sink.write_audio(
            &AudioBlock {
                start_sample: start,
                sample_rate: 48_000,
                speakers: SpeakerLayout::STEREO,
                channels: vec![vec![0.25; 1_500]; 2],
            },
            &cancel,
        )
        .expect("audio");
    }
    sink.finish().expect("finish");
    let events = log.events();
    assert_eq!(
        events[..2],
        [
            Event::Track(TrackKind::Video, 0),
            Event::Track(TrackKind::Audio, 2_112)
        ]
    );
    let audio = events
        .iter()
        .filter_map(|event| match event {
            Event::Packet(TrackId(2), pts, _) => Some(*pts),
            _ => None,
        })
        .collect::<Vec<_>>();
    // 3000 samples: two whole frames and a short last one.
    assert_eq!(audio, [-2_112, -1_088, -64]);
}

#[test]
fn a_cancelled_export_stops_at_the_audio() {
    let log = Log::default();
    let mut sink = sink(&log, true);
    let cancel = CancelToken::default();
    cancel.cancel();
    let block = AudioBlock {
        start_sample: 0,
        sample_rate: 48_000,
        speakers: SpeakerLayout::STEREO,
        channels: vec![vec![0.0; 16]; 2],
    };
    assert!(matches!(
        sink.write_audio(&block, &cancel),
        Err(EncodeError::Cancelled)
    ));
}
