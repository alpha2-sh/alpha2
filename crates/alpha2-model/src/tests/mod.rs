use alpha2_time::{FrameRate, MediaTime, TimeRange};
use std::collections::BTreeMap;

use crate::{
    Clip, ClipId, MediaKind, MediaSource, Project, ProjectId, ProjectSettings, Timeline, Track,
    TrackId, TrackKind,
};
use alpha2_media::SourceId;

mod adjustments;
mod audio_projects;
mod bounds;
mod captions;
mod curves;
mod documents;
mod layouts;
mod provider_sources;
mod scaling;
mod shapes;
mod shared_corpora;
mod text_fit;
mod text_runs;
mod visuals;

fn id<T>(make: impl FnOnce(String) -> Result<T, alpha2_media::IdError>, value: &str) -> T {
    make(value.to_owned()).expect("test ID is non-empty")
}

pub(crate) fn project() -> Project {
    let source_id = id(SourceId::new, "source-a");
    let clip = Clip {
        id: id(ClipId::new, "clip-a"),
        source_id: source_id.clone(),
        timeline_range: TimeRange::new(MediaTime(0), MediaTime(120_000)).expect("ordered range"),
        source_range: TimeRange::new(MediaTime(0), MediaTime(120_000)).expect("ordered range"),
        visual: crate::VisualProperties::default(),
        composition: None,
        blend_mode: crate::ClipBlendMode::Normal,
        masks: Vec::new(),
        matte: None,
        time_map: crate::TimeMap::Forward,
        audio: crate::AudioProperties::default(),
        cache: crate::CacheIntent::Auto,
    };
    Project {
        data: Vec::new(),
        schema_version: crate::CURRENT_SCHEMA_VERSION,
        id: id(ProjectId::new, "project-a"),
        name: "A project".into(),
        settings: ProjectSettings {
            width: 1_920,
            height: 1_080,
            frame_rate: FrameRate::new(30_000, 1_001).expect("valid frame rate"),
            sample_rate: 48_000,
            background: crate::Rgb::BLACK,
            blend_space: crate::BlendSpace::LinearLight,
            color: crate::ColorPipeline::default(),
        },
        sources: vec![MediaSource {
            id: source_id,
            locator: "asset://video-a".into(),
            kind: MediaKind::Video,
            color_metadata: None,
            has_audio: false,
            generator: None,
            composition: None,
            fingerprint: None,
            provider: None,
        }],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        audio: crate::MasterAudio::default(),
        timeline: Timeline {
            tracks: vec![Track {
                id: id(TrackId::new, "track-a"),
                name: "Video".into(),
                kind: TrackKind::Video,
                clips: vec![clip],
                transitions: Vec::new(),
                audio: crate::TrackAudio::default(),
                captions: None,
            }],
        },
    }
}

fn audio_project() -> Project {
    let mut project = project();
    let source = id(SourceId::new, "music");
    project.sources.push(MediaSource {
        id: source.clone(),
        locator: "asset://music".into(),
        kind: MediaKind::Audio,
        color_metadata: None,
        has_audio: false,
        generator: None,
        composition: None,
        fingerprint: None,
        provider: None,
    });
    project.timeline.tracks.push(Track {
        id: id(TrackId::new, "track-audio"),
        name: "Music".into(),
        kind: TrackKind::Audio,
        clips: vec![Clip {
            id: id(ClipId::new, "clip-music"),
            source_id: source,
            timeline_range: TimeRange::new(MediaTime(0), MediaTime(240_000)).expect("range"),
            source_range: TimeRange::new(MediaTime(0), MediaTime(240_000)).expect("range"),
            visual: crate::VisualProperties::default(),
            composition: None,
            blend_mode: crate::ClipBlendMode::Normal,
            masks: Vec::new(),
            matte: None,
            time_map: crate::TimeMap::Forward,
            audio: crate::AudioProperties {
                fade_in: MediaTime(12_000),
                fade_out: MediaTime(24_000),
                effects: vec![crate::AudioEffect::Reverb {
                    room_size: 0.6,
                    damping: 0.4,
                    mix: 0.2,
                }],
                ..crate::AudioProperties::default()
            },
            cache: crate::CacheIntent::Auto,
        }],
        transitions: Vec::new(),
        audio: crate::TrackAudio::default(),
        captions: None,
    });
    project
}

fn with_source(source: MediaSource) -> crate::Project {
    let mut project = project();
    project.timeline.tracks[0].clips[0].source_id = source.id.clone();
    project.sources.push(source);
    project
}

#[test]
fn a_whole_project_export_runs_from_zero_to_the_end_of_what_shows() {
    let mut late = project();
    let two_seconds = MediaTime(2 * alpha2_time::TICKS_PER_SECOND);
    let clip = &mut late.timeline.tracks[0].clips[0];
    clip.timeline_range =
        TimeRange::new(two_seconds, MediaTime(two_seconds.0 + 120_000)).expect("ordered range");
    assert_eq!(late.extent().map(TimeRange::start), Some(two_seconds));
    assert_eq!(
        late.default_export_range(),
        Some(TimeRange::new(MediaTime(0), MediaTime(two_seconds.0 + 120_000)).expect("range"))
    );
    let mut empty = project();
    empty.timeline.tracks[0].clips.clear();
    assert_eq!(empty.default_export_range(), None);
}
