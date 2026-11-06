use alpha2_container::{AudioTrack, DemuxError, TrackId, TrackKind};
use bytes::Bytes;

use super::*;

/// Stereo s16 PCM packets at 48 kHz: `(pts, samples, discard)`; sample `n` of the file is
/// `n % 1000` in both channels.
struct Pcm {
    tracks: Vec<TrackInfo>,
    packets: Vec<(i64, usize, (u32, u32))>,
    next: usize,
}

fn pcm(packets: &[(i64, usize, (u32, u32))]) -> Box<dyn ContainerReader> {
    Box::new(Pcm {
        tracks: vec![TrackInfo {
            id: TrackId(1),
            kind: TrackKind::Audio,
            timescale: 48_000,
            duration: None,
            language: Some("eng".into()),
            title: None,
            default: true,
            enabled: true,
            video: None,
            audio: Some(AudioTrack {
                codec: AudioCodec::Pcm(PcmFormat {
                    bits: 16,
                    float: false,
                    big_endian: false,
                    signed: true,
                }),
                codec_private: Bytes::new(),
                sample_rate: 48_000,
                channels: 2,
                speakers: None,
                bits_per_sample: Some(16),
                priming: 0,
            }),
        }],
        packets: packets.to_vec(),
        next: 0,
    })
}

impl ContainerReader for Pcm {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(
        &mut self,
        _track: alpha2_container::TrackId,
        pts: i64,
    ) -> Result<Option<i64>, DemuxError> {
        self.next = self
            .packets
            .iter()
            .rposition(|packet| packet.0 <= pts)
            .unwrap_or(0);
        Ok(self.packets.get(self.next).map(|packet| packet.0))
    }

    fn next_packet(
        &mut self,
        track: alpha2_container::TrackId,
    ) -> Result<Option<Packet>, DemuxError> {
        let Some(&(pts, samples, discard)) = self.packets.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        let mut data = Vec::with_capacity(samples * 4);
        for index in 0..samples {
            let value =
                i16::try_from((pts + i64::try_from(index).expect("small")).rem_euclid(1000))
                    .expect("small");
            data.extend_from_slice(&value.to_le_bytes());
            data.extend_from_slice(&value.to_le_bytes());
        }
        Ok(Some(Packet {
            track,
            pts,
            dts: pts,
            duration: i64::try_from(samples).expect("small"),
            keyframe: true,
            data: Bytes::from(data),
            discard,
        }))
    }
}

fn open(packets: &[(i64, usize, (u32, u32))], rate: u32) -> AudioStream {
    AudioStream::open(
        pcm(packets),
        0,
        &WithPcm(PcmOnly),
        rate,
        DecodePolicy::Strict,
    )
    .expect("open")
}

fn linear(stream: &mut AudioStream) -> (i64, Vec<f32>) {
    let cancel = CancelToken::default();
    let mut start = None;
    let mut samples = Vec::new();
    while let Some(block) = stream.next_block(&cancel).expect("block") {
        let first = *start.get_or_insert(block.start_sample);
        assert_eq!(
            block.start_sample,
            first + i64::try_from(samples.len()).expect("small"),
            "contiguous"
        );
        samples.extend_from_slice(&block.channels[0]);
    }
    (start.expect("audio"), samples)
}

fn value(sample: i64) -> f32 {
    f32::from(i16::try_from(sample.rem_euclid(1000)).expect("small")) / 32_768.0
}

fn packets(from: i64, count: i64) -> Vec<(i64, usize, (u32, u32))> {
    (0..count)
        .map(|index| (from + index * 1024, 1024, (0, 0)))
        .collect()
}

#[test]
fn samples_sit_at_their_packets_time_and_skip_samples_are_dropped() {
    let mut stream = open(&packets(4_944, 10), 48_000);
    let (start, samples) = linear(&mut stream);
    assert_eq!(start, 4_944);
    assert_eq!(samples.len(), 10_240);
    assert_eq!(samples[0], value(4_944));
    assert_eq!(stream.speakers(), SpeakerLayout::STEREO);
    // A priming packet before zero whose samples are all skipped, then the rest.
    let mut primed = packets(0, 4);
    primed.insert(0, (-1_024, 1_024, (1_024, 0)));
    primed[4].2 = (0, 24);
    let (start, samples) = linear(&mut open(&primed, 48_000));
    assert_eq!(start, 0);
    assert_eq!(samples.len(), 4_096 - 24);
}

#[test]
fn a_timestamp_gap_is_silence_and_an_overlap_is_dropped() {
    let mut gapped = packets(0, 2);
    gapped.extend(packets(4_096, 2));
    gapped.push((6_000, 1_024, (0, 0)));
    let mut stream = open(&gapped, 48_000);
    let (start, samples) = linear(&mut stream);
    assert_eq!(start, 0);
    assert!(samples[2_048..4_096].iter().all(|sample| *sample == 0.0));
    assert_eq!(samples[4_096], value(4_096));
    assert_eq!(samples.len(), 6_000 + 1_024);
    assert_eq!(samples[6_500], value(6_500));
    let diagnostics = &stream.recovery().diagnostics;
    assert_eq!(diagnostics.audio_gaps.len(), 1);
    assert_eq!(diagnostics.audio_overlaps.len(), 1);
}

#[test]
fn a_seek_starts_at_its_sample_with_the_samples_a_linear_decode_gives() {
    let cancel = CancelToken::default();
    let mut stream = open(&packets(0, 40), 48_000);
    let (_, samples) = linear(&mut stream);
    for sample in [0_i64, 9_600, 30_002] {
        let time = MediaTime::from_ticks(sample * TICKS_PER_SECOND / 48_000);
        let block = stream.seek(time, &cancel).expect("seek");
        assert_eq!(block.start_sample, sample);
        let from = usize::try_from(sample).expect("small");
        assert_eq!(block.channels[0], samples[from..from + block.frames()]);
    }
    let late = open(&packets(4_800, 4), 48_000)
        .seek(MediaTime::ZERO, &cancel)
        .expect("seek");
    assert_eq!(late.start_sample, 4_800);
}

#[test]
fn audio_is_resampled_to_the_project_rate() {
    let (start, samples) = linear(&mut open(&packets(48_000, 47), 44_100));
    assert_eq!(start, 44_100);
    let expected = 47 * 1024 * 44_100 / 48_000;
    assert!(
        samples.len().abs_diff(expected) <= 1,
        "{} of {expected}",
        samples.len()
    );
}

#[test]
fn codecs_without_a_session_are_refused() {
    let mut reader = pcm(&packets(0, 1));
    let tracks = reader.tracks().to_vec();
    let _ = reader.seek(tracks[0].id, 0);
    let error = PcmOnly.open(&TrackInfo {
        audio: Some(AudioTrack {
            codec: AudioCodec::Aac,
            ..tracks[0].audio.clone().expect("audio")
        }),
        ..tracks[0].clone()
    });
    assert!(matches!(error, Err(BackendError::Unsupported(_))));
}

#[test]
fn aac_layouts_come_from_the_channel_configuration_or_the_program_config_element() {
    let names = |config: &[u8], channels| {
        aac_layout(config, channels).map(|layout| {
            layout
                .speakers()
                .iter()
                .map(|speaker| speaker.name())
                .collect::<Vec<_>>()
        })
    };
    // The corpus's AAC 3.0, 5.1 and 7.1 configurations, and its quad's program config element.
    assert_eq!(
        names(&[0x11, 0x98, 0x56, 0xe5, 0x00], 3).expect("3.0"),
        ["FL", "FR", "FC"]
    );
    assert_eq!(
        names(&[0x11, 0xb0, 0x56, 0xe5, 0x00], 6).expect("5.1"),
        ["FL", "FR", "FC", "LFE", "BL", "BR"]
    );
    assert_eq!(
        names(&[0x11, 0xb8, 0x56, 0xe5, 0x00], 8).expect("7.1"),
        ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]
    );
    let quad = [
        0x11, 0x80, 0x04, 0xc4, 0x04, 0x00, 0x21, 0x10, 0x04, 0x4c, 0x61, 0x76, 0x63, 0x56, 0xe5,
        0x00,
    ];
    assert_eq!(names(&quad, 4).expect("quad"), ["FL", "FR", "BL", "BR"]);
    // A count other than the configuration's is left to the default.
    assert_eq!(names(&[0x11, 0x98], 2), None);
}

#[test]
fn the_stream_takes_its_speakers_from_planes_in_another_layout() {
    let vorbis = alpha2_container::vorbis_order(6).expect("5.1");
    let native = SpeakerLayout::default_for(6).expect("5.1");
    let values = |planes: &[Vec<f32>]| planes.iter().map(|plane| plane[0]).collect::<Vec<_>>();
    // Vorbis 5.1 is FL FC FR BL BR LFE; native is FL FR FC LFE BL BR.
    let planes = (0_u8..6)
        .map(|plane| vec![f32::from(plane)])
        .collect::<Vec<_>>();
    assert_eq!(
        values(&select(planes.clone(), vorbis, native)),
        [0.0, 2.0, 1.0, 5.0, 3.0, 4.0]
    );
    // A 5.0 with side speakers from a decoder's 5.1 with back ones: the LFE goes.
    let side = SpeakerLayout::new(&[
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::SideLeft,
        Speaker::SideRight,
    ])
    .expect("5.0");
    assert_eq!(
        values(&select(planes.clone(), native, side)),
        [0.0, 1.0, 2.0, 4.0, 5.0]
    );
    // Layouts without the stream's speakers leave the planes alone.
    let quad = SpeakerLayout::default_for(4).expect("4.0");
    assert_eq!(select(planes[..4].to_vec(), quad, native), planes[..4]);
}
