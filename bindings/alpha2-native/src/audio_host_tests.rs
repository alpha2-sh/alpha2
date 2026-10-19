use alpha2_audio::Gain;

use super::*;

#[derive(Default)]
struct CollectSink {
    samples: Vec<f32>,
    starts: Vec<i64>,
    finished: bool,
}

impl AudioBlockSink for CollectSink {
    fn write_audio(
        &mut self,
        block: &AudioBlock,
        _cancel: &CancelToken,
    ) -> Result<(), EncodeError> {
        self.starts.push(block.start_sample);
        self.samples.extend_from_slice(&block.channels[0]);
        Ok(())
    }
    fn finish_audio(&mut self, _cancel: &CancelToken) -> Result<(), EncodeError> {
        self.finished = true;
        Ok(())
    }
    fn abort_audio(&mut self) -> Result<(), EncodeError> {
        Ok(())
    }
}

#[test]
fn offline_loop_uses_irregular_blocks_and_public_sink() {
    let mut graph = AudioGraph::new(ChannelLayout::Mono, 1);
    graph
        .clip_chain_mut(0)
        .expect("clip chain")
        .push(Gain::new(0.5))
        .expect("one input");
    let input = vec![vec![1.0; 1_001]];
    let mut sink = CollectSink::default();
    render_offline(
        &mut graph,
        48_000,
        &input,
        &[32, 127, 511],
        &mut sink,
        &CancelToken::default(),
    )
    .expect("offline render");
    assert!(sink.finished);
    assert_eq!(sink.samples, vec![0.5; 1_001]);
    assert_eq!(sink.starts[0], 0);
}

/// A mono input whose every sample differs, so a sample rendered at the
/// wrong position or out of order shows.
fn ramp(frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|index| {
            let index = f32::from(u16::try_from(index).expect("short ramp"));
            (index * 0.013).sin() * 0.9
        })
        .collect()
}

fn offline_with_a_filter(block_sizes: &[usize]) -> CollectSink {
    let mut graph = AudioGraph::new(ChannelLayout::Mono, 1);
    let chain = graph.clip_chain_mut(0).expect("clip chain");
    chain.push(Gain::new(0.5)).expect("one input");
    chain
        .push(alpha2_audio::Biquad::new(
            alpha2_audio::BiquadKind::LowPass,
            2_000.0,
            0.7,
            0.0,
        ))
        .expect("one input");
    let mut sink = CollectSink::default();
    render_offline(
        &mut graph,
        48_000,
        &[ramp(1_001)],
        block_sizes,
        &mut sink,
        &CancelToken::default(),
    )
    .expect("offline render");
    assert!(sink.finished);
    sink
}

#[test]
fn offline_audio_is_the_same_bits_in_any_block_schedule() {
    let irregular = offline_with_a_filter(&[32, 127, 511]);
    let whole = offline_with_a_filter(&[1_001]);
    assert_eq!(irregular.starts, [0, 32, 159, 670, 702, 829]);
    assert_eq!(whole.starts, [0]);
    assert_eq!(
        irregular
            .samples
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        whole
            .samples
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
    assert!(whole.samples.iter().any(|value| value.abs() > 0.1));
}

#[test]
fn offline_inputs_must_be_exactly_the_graphs_buses() {
    let plane = || vec![0.25_f32; 64];
    for (name, layout, buses, planes) in [
        (
            "one stereo bus for a two-bus graph",
            ChannelLayout::Stereo,
            2,
            2,
        ),
        (
            "two stereo buses for a one-bus graph",
            ChannelLayout::Stereo,
            1,
            4,
        ),
        ("a stereo bus for a mono graph", ChannelLayout::Mono, 1, 2),
    ] {
        let mut graph = AudioGraph::new(layout, buses);
        let inputs = (0..planes).map(|_| plane()).collect::<Vec<_>>();
        let mut sink = CollectSink::default();
        assert_eq!(
            render_offline(
                &mut graph,
                48_000,
                &inputs,
                &[64],
                &mut sink,
                &CancelToken::default(),
            ),
            Err(OfflineAudioError::InvalidInput),
            "{name}"
        );
        assert!(!sink.finished, "{name}: nothing is committed");
    }
    // Two stereo buses for a two-bus stereo graph mix into the graph's stereo.
    let mut graph = AudioGraph::new(ChannelLayout::Stereo, 2);
    let mut sink = CollectSink::default();
    render_offline(
        &mut graph,
        48_000,
        &(0..4).map(|_| plane()).collect::<Vec<_>>(),
        &[64],
        &mut sink,
        &CancelToken::default(),
    )
    .expect("matching buses render");
    assert!(sink.finished);
    assert!(
        sink.samples
            .iter()
            .all(|value| (value - 0.5).abs() < 1.0e-6)
    );
}

#[cfg(feature = "timeline")]
fn audio_project(gain: f32) -> EngineProject {
    audio_project_with(gain, |_| {})
}

#[cfg(feature = "timeline")]
fn audio_project_with(gain: f32, edit: impl FnOnce(&mut alpha2_model::Project)) -> EngineProject {
    use alpha2_model::{
        Animated, AudioProperties, Clip, ClipId, MediaKind, MediaSource, Project, ProjectId,
        ProjectSettings, Timeline, Track, TrackId, TrackKind,
    };
    use alpha2_time::{FrameRate, TimeRange};

    let second = MediaTime::from_seconds(1);
    let source_id = SourceId::new(String::from("tone")).expect("id");
    let mut clip = Clip::new(
        ClipId::new(String::from("c1")).expect("id"),
        source_id.clone(),
        TimeRange::new(MediaTime::ZERO, second).expect("range"),
        TimeRange::new(MediaTime::ZERO, second).expect("range"),
    );
    clip.audio = AudioProperties {
        gain: Animated::Constant { value: gain },
        ..AudioProperties::default()
    };
    let mut track = Track::new(
        TrackId::new(String::from("a1")).expect("id"),
        "audio",
        TrackKind::Audio,
    );
    track.clips.push(clip);
    let project = Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new(String::from("native-audio")).expect("id"),
        name: "native".into(),
        settings: ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), 48_000)
            .expect("settings"),
        sources: vec![MediaSource::new(
            source_id,
            "asset://tone",
            MediaKind::Audio,
        )],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline {
            tracks: vec![track],
        },
        audio: alpha2_model::MasterAudio {
            limiter: None,
            ..alpha2_model::MasterAudio::default()
        },
    };
    let mut project = project;
    edit(&mut project);
    EngineProject::new(project).expect("valid project")
}

#[cfg(feature = "timeline")]
fn decoded(value: f32, frames: usize) -> DecodedAudioSources {
    let mut sources = DecodedAudioSources::new();
    sources.insert(
        SourceId::new(String::from("tone")).expect("id"),
        AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![vec![value; frames], vec![value; frames]],
        },
    );
    sources
}

#[test]
#[cfg(feature = "timeline")]
fn project_audio_renders_through_irregular_blocks_into_a_public_sink() {
    let snapshot = audio_project(0.5);
    let sources = decoded(1.0, 48_000);
    let mut sink = CollectSink::default();
    render_project_audio(
        &snapshot,
        &ProjectAudioRequest {
            channels: 2,
            start_sample: 0,
            total_frames: 1_001,
            block_sizes: &[32, 127, 511],
        },
        &sources,
        &mut sink,
        &CancelToken::default(),
    )
    .expect("project audio render");

    assert!(sink.finished);
    assert_eq!(sink.samples.len(), 1_001);
    assert_eq!(sink.starts[0], 0);
    assert!(
        sink.samples
            .iter()
            .all(|value| (value - 0.5).abs() < 1.0e-5)
    );
}

#[cfg(feature = "timeline")]
fn decoded_ramp(frames: usize) -> DecodedAudioSources {
    let mut sources = DecodedAudioSources::new();
    let channel = (0..frames)
        .map(|index| {
            let index = f32::from(u16::try_from(index % 60_000).expect("short ramp"));
            (index * 0.011).sin() * 0.8
        })
        .collect::<Vec<_>>();
    sources.insert(
        SourceId::new(String::from("tone")).expect("id"),
        AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![channel.clone(), channel],
        },
    );
    sources
}

#[cfg(feature = "timeline")]
fn project_range(start_sample: i64, block_sizes: &[usize]) -> CollectSink {
    let mut sink = CollectSink::default();
    render_project_audio(
        &audio_project(0.5),
        &ProjectAudioRequest {
            channels: 2,
            start_sample,
            total_frames: 1_001,
            block_sizes,
        },
        &decoded_ramp(48_000),
        &mut sink,
        &CancelToken::default(),
    )
    .expect("project audio render");
    assert!(sink.finished);
    sink
}

#[test]
#[cfg(feature = "timeline")]
fn project_audio_is_the_same_bits_in_any_block_schedule() {
    for start in [0, 12_345] {
        let irregular = project_range(start, &[32, 127, 511]);
        let whole = project_range(start, &[1_001]);
        assert_eq!(
            irregular.starts,
            [0, 32, 159, 670, 702, 829],
            "from {start}"
        );
        assert_eq!(
            irregular
                .samples
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            whole
                .samples
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            "from {start}"
        );
        assert!(whole.samples.iter().any(|value| value.abs() > 0.1));
    }
    // The range is the project's own samples from where it starts.
    let from_zero = project_range(0, &[1_001]);
    let later = project_range(500, &[1_001]);
    assert_eq!(later.samples[..501], from_zero.samples[500..]);
}

/// A 62 s clip through an EQ band, on a track through a compressor, into
/// a master reverb and limiter: recursive stages with different leads.
#[cfg(feature = "timeline")]
fn effects_project() -> EngineProject {
    use alpha2_model::{AudioEffect, EqBand, EqBandKind, MasterAudio};
    use alpha2_time::TimeRange;
    audio_project_with(1.0, |project| {
        let range = TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(62)).expect("range");
        let track = &mut project.timeline.tracks[0];
        let clip = &mut track.clips[0];
        clip.timeline_range = range;
        clip.source_range = range;
        clip.audio.effects = vec![AudioEffect::Eq {
            bands: vec![EqBand {
                kind: EqBandKind::Peaking,
                frequency_hz: 1_000.0,
                q: 0.7,
                gain_db: 3.0,
            }],
        }];
        track.audio.effects = vec![AudioEffect::Compressor {
            threshold_db: -30.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 80.0,
            lookahead_ms: 0.0,
        }];
        project.audio = MasterAudio::default();
        project.audio.effects = vec![AudioEffect::Reverb {
            room_size: 0.6,
            damping: 0.4,
            mix: 0.2,
        }];
    })
}

#[test]
#[cfg(feature = "timeline")]
fn project_audio_with_effects_is_the_same_bits_in_any_block_schedule() {
    // After every stage's lead, so each stage starts its replay at its own sample rather than
    // at timeline zero.
    let start = 60 * 48_000 + 123;
    // A quiet 440 Hz sine, under the compressor's threshold after the clip gain.
    let plane = (0..62 * 48_000)
        .map(|index| {
            let phase = std::f64::consts::TAU * 440.0 * f64::from(index) / 48_000.0;
            #[allow(clippy::cast_possible_truncation)]
            let sample = (phase.sin() * 0.1) as f32;
            sample
        })
        .collect::<Vec<_>>();
    let mut sources = DecodedAudioSources::new();
    sources.insert(
        SourceId::new(String::from("tone")).expect("id"),
        AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![plane.clone(), plane],
        },
    );
    let render = |block_sizes: &[usize]| {
        let mut sink = CollectSink::default();
        render_project_audio(
            &effects_project(),
            &ProjectAudioRequest {
                channels: 2,
                start_sample: start,
                total_frames: 4_096,
                block_sizes,
            },
            &sources,
            &mut sink,
            &CancelToken::default(),
        )
        .expect("project audio render");
        sink.samples
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    };
    let whole = render(&[4_096]);
    assert!(whole.iter().any(|&bits| f32::from_bits(bits).abs() > 0.01));
    for schedule in [&[32, 127, 511][..], &[1_001], &[512]] {
        let other = render(schedule);
        let differ = whole.iter().zip(&other).filter(|(a, b)| a != b).count();
        assert_eq!(
            differ,
            0,
            "{schedule:?}: {differ} of {} samples differ",
            whole.len()
        );
    }
}

#[test]
#[cfg(feature = "timeline")]
fn a_project_range_after_sample_zero_is_written_from_the_files_first_sample() {
    let path = std::env::temp_dir().join(format!(
        "alpha2-native-project-range-{}.wav",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut sink = crate::NativeWavEncodeSink::create(&path, 48_000, 2).expect("wav sink");
    let rendered = render_project_audio(
        &audio_project(0.5),
        &ProjectAudioRequest {
            channels: 2,
            start_sample: 24_000,
            total_frames: 4_800,
            block_sizes: &[1_024],
        },
        &decoded_ramp(48_000),
        &mut sink,
        &CancelToken::default(),
    );
    let written = std::fs::metadata(&path).map(|metadata| metadata.len());
    let _ = std::fs::remove_file(&path);
    assert_eq!(rendered, Ok(()));
    // The header, then 4 800 stereo frames of 32-bit samples.
    assert!(written.expect("the WAV file") >= 4_800 * 2 * 4);
}

#[test]
#[cfg(feature = "timeline")]
fn project_audio_reports_missing_media_instead_of_committing_silence() {
    let snapshot = audio_project(1.0);
    let empty = DecodedAudioSources::new();
    let mut sink = CollectSink::default();
    let error = render_project_audio(
        &snapshot,
        &ProjectAudioRequest {
            channels: 2,
            start_sample: 0,
            total_frames: 256,
            block_sizes: &[128],
        },
        &empty,
        &mut sink,
        &CancelToken::default(),
    )
    .expect_err("missing media is reported");
    match error {
        OfflineAudioError::MissingSources(sources) => {
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].as_str(), "tone");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(!sink.finished, "an incomplete mix must not be committed");
}

#[test]
#[cfg(feature = "timeline")]
fn project_audio_rejects_empty_schedules_and_cancels_cleanly() {
    let snapshot = audio_project(1.0);
    let sources = decoded(1.0, 48_000);
    let mut sink = CollectSink::default();
    for (frames, blocks) in [(0_usize, &[128_usize][..]), (256, &[][..]), (256, &[0][..])] {
        assert_eq!(
            render_project_audio(
                &snapshot,
                &ProjectAudioRequest {
                    channels: 2,
                    start_sample: 0,
                    total_frames: frames,
                    block_sizes: blocks,
                },
                &sources,
                &mut sink,
                &CancelToken::default(),
            ),
            Err(OfflineAudioError::InvalidInput)
        );
    }

    let cancel = CancelToken::default();
    cancel.cancel();
    assert_eq!(
        render_project_audio(
            &snapshot,
            &ProjectAudioRequest {
                channels: 2,
                start_sample: 0,
                total_frames: 256,
                block_sizes: &[128],
            },
            &sources,
            &mut sink,
            &cancel,
        ),
        Err(OfflineAudioError::Sink(EncodeError::Cancelled))
    );
    assert!(!sink.finished);
}

#[test]
fn a_device_that_lists_only_its_own_channel_count_is_opened_in_stereo() {
    let f32_at = |channels: u16| HostAudioConfig {
        channels,
        min_sample_rate: 44_100,
        max_sample_rate: 96_000,
        format: HostSampleFormat::F32,
    };
    // WASAPI lists only the mix format's channel count, CoreAudio the sum of the device's
    // stream channels.
    assert_eq!(
        negotiate_output_config(&[f32_at(6)], 48_000),
        Some(f32_at(6))
    );
    assert_eq!(
        negotiate_output_config(&[f32_at(8)], 48_000),
        Some(f32_at(8))
    );
    // ALSA lists every count between its least and most: stereo, then the fewest above it, then
    // mono.
    let every = (1..=8).map(f32_at).collect::<Vec<_>>();
    assert_eq!(negotiate_output_config(&every, 48_000), Some(f32_at(2)));
    assert_eq!(
        negotiate_output_config(&[f32_at(1), f32_at(8), f32_at(6)], 48_000),
        Some(f32_at(6))
    );
    assert_eq!(
        negotiate_output_config(&[f32_at(1)], 48_000),
        Some(f32_at(1))
    );
    assert_eq!(negotiate_output_config(&[f32_at(0)], 48_000), None);
}

/// A realtime source whose sample n is n, recording each render.
#[cfg(feature = "cpal-host")]
#[derive(Default)]
struct Counting {
    renders: Vec<(i64, usize, usize)>,
}

#[cfg(feature = "cpal-host")]
impl RealtimeAudioSource for Counting {
    fn render(&mut self, ctx: &ProcessCtx, output: &mut [&mut [f32]]) {
        self.renders
            .push((ctx.sample_position, output.len(), output[0].len()));
        for (channel, plane) in output.iter_mut().enumerate() {
            for (offset, sample) in plane.iter_mut().enumerate() {
                let position = ctx.sample_position + i64::try_from(offset).expect("offset");
                *sample = f32::from(u16::try_from(position).expect("position"))
                    + f32::from(u8::try_from(channel).expect("channel")) * 0.5;
            }
        }
    }
}

#[test]
#[cfg(feature = "cpal-host")]
fn a_device_buffer_larger_than_the_callback_limit_is_rendered_in_pieces() {
    let mut source = Counting::default();
    let mut planar = [vec![0.0; 512], vec![0.0; 512]];
    let mut position = 0;
    // An ALSA or PipeWire period of 1 024 frames, then a WASAPI buffer's free space.
    for frames in [1_024, 700] {
        let mut data = vec![-1.0; frames * 2];
        assert!(fill_device_buffer(
            &mut source,
            &mut planar,
            &mut data,
            2,
            48_000,
            &mut position,
        ));
        let start = usize::try_from(position).expect("position") - frames;
        for (frame, pair) in data.as_chunks::<2>().0.iter().enumerate() {
            let expected = f32::from(u16::try_from(start + frame).expect("frame"));
            assert_eq!(pair.as_slice(), [expected, expected + 0.5]);
        }
    }
    assert_eq!(position, 1_724);
    assert_eq!(
        source.renders,
        [(0, 2, 512), (512, 2, 512), (1_024, 2, 512), (1_536, 2, 188)]
    );
}

#[test]
#[cfg(feature = "cpal-host")]
fn stereo_fills_a_wider_devices_first_two_channels_and_mono_a_mono_device() {
    let mut source = Counting::default();
    let mut planar = [vec![0.0; 4], vec![0.0; 4]];
    let mut position = 0;
    let mut data = vec![-1.0; 6 * 6];
    assert!(fill_device_buffer(
        &mut source,
        &mut planar,
        &mut data,
        6,
        48_000,
        &mut position
    ));
    for (frame, speakers) in data.as_chunks::<6>().0.iter().enumerate() {
        let expected = f32::from(u8::try_from(frame).expect("frame"));
        assert_eq!(
            speakers.as_slice(),
            [expected, expected + 0.5, 0.0, 0.0, 0.0, 0.0]
        );
    }
    let mut mono = vec![-1.0; 3];
    assert!(fill_device_buffer(
        &mut source,
        &mut planar,
        &mut mono,
        1,
        48_000,
        &mut position
    ));
    assert_eq!(mono, [6.0, 7.0, 8.0]);
    assert_eq!(source.renders, [(0, 2, 4), (4, 2, 2), (6, 1, 3)]);
    // A buffer that is not whole frames is played as silence and does not move the clock.
    let mut torn = vec![-1.0; 5];
    assert!(!fill_device_buffer(
        &mut source,
        &mut planar,
        &mut torn,
        2,
        48_000,
        &mut position
    ));
    assert_eq!(torn, [0.0; 5]);
    assert_eq!(position, 9);
}

#[test]
fn output_negotiation_is_deterministic_without_an_audio_device() {
    let configs = [
        HostAudioConfig {
            channels: 6,
            min_sample_rate: 48_000,
            max_sample_rate: 48_000,
            format: HostSampleFormat::F32,
        },
        HostAudioConfig {
            channels: 1,
            min_sample_rate: 44_100,
            max_sample_rate: 96_000,
            format: HostSampleFormat::F32,
        },
        HostAudioConfig {
            channels: 2,
            min_sample_rate: 48_000,
            max_sample_rate: 48_000,
            format: HostSampleFormat::F32,
        },
        HostAudioConfig {
            channels: 2,
            min_sample_rate: 44_100,
            max_sample_rate: 96_000,
            format: HostSampleFormat::I16,
        },
    ];
    assert_eq!(negotiate_output_config(&configs, 48_000), Some(configs[2]));
    assert_eq!(negotiate_output_config(&configs, 192_000), None);
}
