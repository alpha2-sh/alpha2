//! Project audio through the native WAV sink.
#![cfg(feature = "timeline")]

use alpha2_engine::EngineProject;
use alpha2_media::{AudioBlock, CancelToken, SourceId};
use alpha2_time::MediaTime;

use crate::{DecodedAudioSources, ProjectAudioRequest, render_project_audio};

fn audio_project() -> EngineProject {
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
        gain: Animated::Constant { value: 0.5 },
        ..AudioProperties::default()
    };
    let mut track = Track::new(
        TrackId::new(String::from("a1")).expect("id"),
        "audio",
        TrackKind::Audio,
    );
    track.clips.push(clip);
    EngineProject::new(Project {
        data: Vec::new(),
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
    })
    .expect("valid project")
}

fn decoded_ramp(frames: usize) -> DecodedAudioSources {
    let channel = (0..frames)
        .map(|index| {
            let index = f32::from(u16::try_from(index % 60_000).expect("short ramp"));
            (index * 0.011).sin() * 0.8
        })
        .collect::<Vec<_>>();
    let mut sources = DecodedAudioSources::new();
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

#[test]

fn a_project_range_after_sample_zero_is_written_from_the_files_first_sample() {
    let path = std::env::temp_dir().join(format!(
        "alpha2-native-project-range-{}.wav",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut sink = crate::NativeWavEncodeSink::create(&path, 48_000, 2).expect("wav sink");
    let rendered = render_project_audio(
        &audio_project(),
        &ProjectAudioRequest {
            channels: 2,
            start_sample: 24_000,
            total_frames: 4_800,
            block_sizes: &[1_024],
        },
        &mut decoded_ramp(48_000),
        &mut sink,
        &CancelToken::default(),
    );
    let written = std::fs::metadata(&path).map(|metadata| metadata.len());
    let _ = std::fs::remove_file(&path);
    assert_eq!(rendered, Ok(()));
    assert!(written.expect("the WAV file") >= 4_800 * 2 * 4);
}
