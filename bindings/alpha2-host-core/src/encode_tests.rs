use std::sync::{Arc, Mutex};

use alpha2_container::MuxError;
use alpha2_media::BitrateMode;

use super::*;

/// What the writer saw, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Track(TrackKind, u32),
    /// Track, presentation time, duration, keyframe.
    Packet(TrackId, i64, i64, bool),
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
    /// Every packet write fails.
    fails: bool,
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
        if self.fails {
            return Err(MuxError::Io(std::io::Error::other("the disk is full")));
        }
        self.written = true;
        self.log.push(Event::Packet(
            packet.track,
            packet.pts,
            packet.duration,
            packet.keyframe,
        ));
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
#[allow(clippy::struct_excessive_bools)]
struct Video {
    queued: VecDeque<(i64, bool)>,
    given: bool,
    ended: bool,
    /// Every frame is refused as busy and nothing comes out.
    busy: bool,
    /// Frame `n` fails to submit.
    fails_at: Option<usize>,
    submitted: usize,
    /// The first waiting receive blocks this long and gives nothing.
    blocks_once: Option<std::time::Duration>,
    /// Gives every queued frame until the next submit.
    settled: bool,
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
        if self.fails_at == Some(self.submitted) {
            return Err(EncodeError::Adapter("the session broke".into()));
        }
        if self.busy || self.queued.len() >= 2 {
            return Ok(Submit::Busy);
        }
        self.submitted += 1;
        self.settled = false;
        self.queued.push_back((pts, keyframe));
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError> {
        if wait && let Some(delay) = self.blocks_once.take() {
            std::thread::sleep(delay);
            return Ok(EncodePoll::Pending);
        }
        if self.busy || (self.queued.len() < 2 && !self.ended && !self.settled) {
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

    fn settle(&mut self) -> Result<(), EncodeError> {
        self.settled = true;
        Ok(())
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
    /// Where the frame after the last one submitted starts, in samples.
    next: i64,
    ended: bool,
    /// The configuration record, when not the AAC one.
    private: Option<Bytes>,
    /// It never publishes a configuration record.
    no_record: bool,
    /// Every submit fails.
    fails: bool,
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

    fn bitrate(&self) -> u32 {
        128_000
    }

    fn submit(&mut self, pts: i64, planes: &[Vec<f32>]) -> Result<Submit, EncodeError> {
        assert_eq!(planes.len(), 2);
        if self.fails {
            return Err(EncodeError::Adapter("the audio session broke".into()));
        }
        self.packets.push_back((pts, planes[0].len()));
        self.next = pts + 1_024;
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
        // Flushing gives the priming's worth of padding frames, as transform codecs do.
        for frame in 0..3 {
            self.packets.push_back((self.next + frame * 1_024, 1_024));
        }
        self.ended = true;
        Ok(())
    }

    fn codec_private(&self) -> Option<Bytes> {
        if self.no_record {
            return None;
        }
        Some(
            self.private
                .clone()
                .unwrap_or(Bytes::from_static(&[0x11, 0x90])),
        )
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
    sink_of(
        log,
        Video::default(),
        audio.then(Audio::default),
        CancelToken::default(),
    )
}

fn sink_of(log: &Log, video: Video, audio: Option<Audio>, cancel: CancelToken) -> EncodeSink {
    let writer = Box::new(Writer {
        log: log.clone(),
        tracks: 0,
        written: false,
        fails: false,
    });
    let audio = audio.map(|encoder| {
        (
            Box::new(encoder) as Box<dyn AudioEncodeBackend>,
            audio_config(),
        )
    });
    EncodeSink::new(writer, Box::new(video), video_config(), audio, cancel)
}

fn write_frames(sink: &mut EncodeSink, count: u64) {
    for index in 0..count {
        sink.write_frame(index, &planes()).expect("frame");
    }
}

#[test]
fn settling_writes_every_frame_the_video_encoder_holds() {
    let log = Log::default();
    let mut sink = sink(&log, false);
    write_frames(&mut sink, 1);
    assert!(log.events().is_empty(), "the encoder holds the frame");
    sink.settle().expect("settled");
    assert_eq!(
        log.events(),
        [
            Event::Track(TrackKind::Video, 0),
            Event::Packet(TrackId(1), 0, 3_000, true)
        ]
    );
}

#[test]
fn video_waits_for_its_configuration_and_is_timed_by_frame_index() {
    let log = Log::default();
    let mut sink = sink(&log, false);
    write_frames(&mut sink, 1);
    assert!(log.events().is_empty(), "no track before the configuration");
    for index in 1..5 {
        sink.write_frame(index, &planes()).expect("frame");
    }
    sink.finish().expect("finish");
    let events = log.events();
    assert_eq!(events[0], Event::Track(TrackKind::Video, 0));
    let packets = events
        .iter()
        .filter_map(|event| match event {
            Event::Packet(_, pts, _, keyframe) => Some((*pts, *keyframe)),
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
            Event::Packet(TrackId(2), pts, duration, _) => Some((*pts, *duration)),
            _ => None,
        })
        .collect::<Vec<_>>();
    // 3000 samples after 2112 of priming: the packets up to the last input sample, the last one
    // cut short there; the padding packets after it are dropped.
    assert_eq!(
        audio,
        [
            (-2_112, 1_024),
            (-1_088, 1_024),
            (-64, 1_024),
            (960, 1_024),
            (1_984, 1_016)
        ]
    );
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

#[test]
fn prores_with_alpha_is_written_as_4444() {
    let base = crate::codecs::NativeVideoCodec::ProRes.base_layout();
    let with_alpha = PixelLayout {
        alpha: true,
        ..base
    };
    let entry = |layout| match container_video_codec(alpha2_media::VideoCodec::ProRes, layout) {
        VideoCodec::ProRes(fourcc) => fourcc.0,
        other => panic!("{other:?}"),
    };
    assert_eq!(&entry(base), b"apch");
    assert_eq!(&entry(with_alpha), b"ap4h");
}

/// 30 s of audio and 900 frames at 30 fps through a sink into `writer`, the audio first when
/// `audio_first`, as a mobile export feeds them.
fn feed_thirty_seconds(
    writer: Box<dyn ContainerWriter>,
    audio_first: bool,
    video_codec: alpha2_media::VideoCodec,
    audio: (AudioEncodeConfig, Option<Bytes>),
) {
    let (config, private) = audio;
    let encoder = Audio {
        private,
        ..Audio::default()
    };
    let mut sink = EncodeSink::new(
        writer,
        Box::new(Video::default()),
        VideoEncodeConfig {
            codec: video_codec,
            ..video_config()
        },
        Some((Box::new(encoder), config)),
        CancelToken::default(),
    );
    let cancel = CancelToken::default();
    let write_audio = |sink: &mut EncodeSink| {
        for block in 0..(30 * 48_000 / 4_800) {
            sink.write_audio(
                &AudioBlock {
                    start_sample: block * 4_800,
                    sample_rate: 48_000,
                    speakers: SpeakerLayout::STEREO,
                    channels: vec![vec![0.0; 4_800]; 2],
                },
                &cancel,
            )
            .expect("audio");
        }
    };
    if audio_first {
        write_audio(&mut sink);
        write_frames(&mut sink, 900);
    } else {
        write_frames(&mut sink, 900);
        write_audio(&mut sink);
    }
    sink.finish().expect("finish");
}

fn logged_feed(audio_first: bool) -> Vec<Event> {
    let log = Log::default();
    let writer = Box::new(Writer {
        log: log.clone(),
        tracks: 0,
        written: false,
        fails: false,
    });
    feed_thirty_seconds(
        writer,
        audio_first,
        alpha2_media::VideoCodec::H264,
        (audio_config(), None),
    );
    log.events()
}

#[test]
fn audio_encoded_before_the_video_reaches_the_writer_at_most_a_second_ahead_of_it() {
    let events = logged_feed(true);
    let mut video = None;
    let mut audio_packets = 0;
    for event in &events {
        match *event {
            Event::Packet(TrackId(1), pts, _, _) => video = Some(pts),
            Event::Packet(TrackId(2), pts, _, _) => {
                audio_packets += 1;
                let video = video.expect("no audio before the first video packet");
                // At most one second plus one audio frame past the video, compared exactly.
                assert!(
                    i128::from(pts) * 90_000
                        <= (i128::from(video) + 90_000) * 48_000 + 1_024 * 90_000,
                    "audio at {pts} / 48000 after video at {video} / 90000"
                );
            }
            _ => {}
        }
    }
    assert!(audio_packets > 1_000, "{audio_packets}");
    let by_track = |events: &[Event], track: u32| {
        events
            .iter()
            .filter(|event| matches!(event, Event::Packet(TrackId(id), ..) if *id == track))
            .cloned()
            .collect::<Vec<_>>()
    };
    let unpaced = logged_feed(false);
    for track in [1, 2] {
        assert_eq!(
            by_track(&events, track),
            by_track(&unpaced, track),
            "track {track}"
        );
    }
}

fn opus_head() -> Bytes {
    let mut head = b"OpusHead".to_vec();
    head.extend([1, 2]);
    head.extend(2_112_u16.to_le_bytes());
    head.extend(48_000_u32.to_le_bytes());
    head.extend([0, 0, 0]);
    Bytes::from(head)
}

fn file_feed(
    name: &str,
    writer: impl FnOnce(std::fs::File) -> Box<dyn ContainerWriter>,
    audio_first: bool,
    video_codec: alpha2_media::VideoCodec,
    audio: (AudioEncodeConfig, Option<Bytes>),
) -> Vec<u8> {
    let path = std::env::temp_dir().join(format!(
        "alpha2-paced-{}-{name}-{audio_first}",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("file");
    feed_thirty_seconds(writer(file), audio_first, video_codec, audio);
    let bytes = std::fs::read(&path).expect("read");
    let _ = std::fs::remove_file(path);
    bytes
}

#[test]
fn a_matroska_export_with_its_audio_encoded_first_keeps_the_tracks_together_in_the_file() {
    use alpha2_container::{MatroskaFlavor, MatroskaWriter};
    let opus = AudioEncodeConfig {
        codec: alpha2_media::AudioCodec::Opus,
        ..audio_config()
    };
    for flavor in [MatroskaFlavor::Matroska, MatroskaFlavor::WebM] {
        let bytes = file_feed(
            &format!("{flavor:?}"),
            |file| Box::new(MatroskaWriter::create(file, flavor)),
            true,
            alpha2_media::VideoCodec::Vp9,
            (opus, Some(opus_head())),
        );
        let (blocks, clusters) = alpha2_test_support::media::matroska_blocks(&bytes);
        assert!(blocks.len() > 2_000, "{flavor:?}: {} blocks", blocks.len());
        let widest = alpha2_test_support::media::widest_track_distance(&blocks);
        assert!(
            widest < 2_000,
            "{flavor:?}: the tracks are {widest} ms apart"
        );
        let back = clusters
            .windows(2)
            .map(|pair| pair[0] - pair[1])
            .max()
            .unwrap_or(0);
        assert!(
            back <= 2_000,
            "{flavor:?}: a cluster steps back by {back} ms"
        );
    }
}

#[test]
fn an_mp4_export_is_the_same_bytes_whichever_track_is_encoded_first() {
    use alpha2_container::{Mp4Flavor, Mp4Writer};
    let feed = |audio_first| {
        file_feed(
            "mp4",
            |file| Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4)),
            audio_first,
            alpha2_media::VideoCodec::H264,
            (audio_config(), None),
        )
    };
    let audio_first = feed(true);
    assert!(audio_first.len() > 4_000, "{}", audio_first.len());
    assert_eq!(audio_first, feed(false));
}

/// Runs `body` on its own thread and fails the test when it takes longer than 10 s.
fn within_ten_seconds<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(body());
    });
    wait.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the sink returns")
}

fn busy() -> Video {
    Video {
        busy: true,
        ..Video::default()
    }
}

#[test]
fn a_video_encoder_that_accepts_no_frame_fails_the_frame_after_the_stall_bound() {
    let result = within_ten_seconds(|| {
        let mut sink = sink_of(&Log::default(), busy(), None, CancelToken::default())
            .with_stall_limit(std::time::Duration::from_millis(300));
        sink.write_frame(0, &planes())
    });
    assert!(
        matches!(&result, Err(EncodeError::Adapter(message)) if message.contains("the video encoder accepted no frame")),
        "{result:?}"
    );
}

#[test]
fn a_cancel_stops_the_wait_on_a_busy_encoder() {
    let cancel = CancelToken::default();
    cancel.cancel();
    let result = within_ten_seconds(move || {
        let mut sink = sink_of(&Log::default(), busy(), None, cancel);
        sink.write_frame(0, &planes())
    });
    assert!(matches!(result, Err(EncodeError::Cancelled)), "{result:?}");
}

#[test]
fn a_drain_that_blocks_once_past_the_stall_bound_still_finishes() {
    let log = Log::default();
    let video = Video {
        blocks_once: Some(std::time::Duration::from_millis(500)),
        ..Video::default()
    };
    let mut sink = sink_of(&log, video, None, CancelToken::default())
        .with_stall_limit(std::time::Duration::from_millis(300));
    write_frames(&mut sink, 1);
    sink.finish().expect("finish");
    assert_eq!(log.events().last(), Some(&Event::Finished));
}

#[test]
fn finish_names_the_encoder_that_published_no_configuration() {
    let audio = Audio {
        no_record: true,
        ..Audio::default()
    };
    let mut sink = sink_of(
        &Log::default(),
        Video::default(),
        Some(audio),
        CancelToken::default(),
    );
    write_frames(&mut sink, 2);
    let result = sink.finish();
    assert!(
        matches!(&result, Err(EncodeError::Adapter(message)) if message.contains("the audio encoder published no configuration")),
        "{result:?}"
    );
}

#[test]
fn a_failing_video_encoder_is_rejected_and_other_failures_are_not() {
    let video = Video {
        fails_at: Some(3),
        ..Video::default()
    };
    let mut sink = sink_of(&Log::default(), video, None, CancelToken::default());
    write_frames(&mut sink, 3);
    let result = sink.write_frame(3, &planes());
    assert!(
        matches!(&result, Err(EncodeError::EncoderRejected { encoder, frame: 3, message })
            if encoder == "fake" && message.contains("the session broke")),
        "{result:?}"
    );

    let mut sink = EncodeSink::new(
        Box::new(Writer {
            log: Log::default(),
            tracks: 0,
            written: false,
            fails: true,
        }),
        Box::new(Video::default()),
        video_config(),
        None,
        CancelToken::default(),
    );
    let mut result = Ok(());
    for index in 0..4 {
        result = sink.write_frame(index, &planes());
        if result.is_err() {
            break;
        }
    }
    assert!(
        matches!(&result, Err(error) if !matches!(error, EncodeError::EncoderRejected { .. })),
        "{result:?}"
    );

    let audio = Audio {
        fails: true,
        ..Audio::default()
    };
    let mut sink = sink_of(
        &Log::default(),
        Video::default(),
        Some(audio),
        CancelToken::default(),
    );
    let block = AudioBlock {
        start_sample: 0,
        sample_rate: 48_000,
        speakers: SpeakerLayout::STEREO,
        channels: vec![vec![0.0; 4_800]; 2],
    };
    let result = sink.write_audio(&block, &CancelToken::default());
    assert!(
        matches!(&result, Err(EncodeError::Adapter(message)) if message.contains("audio session")),
        "{result:?}"
    );
}
