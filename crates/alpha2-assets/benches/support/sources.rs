//! A project that reads `count` host-supplied sources, one clip each, and a
//! catalog that knows every one of them: the shape an editor's catalog has.

use alpha2_assets::{AssetCatalog, AssetEntry, OriginalState};
use alpha2_media::SourceId;
use alpha2_model::{
    Clip, ClipId, MasterAudio, MediaKind, MediaSource, Project, ProjectId, ProjectSettings,
    Timeline, Track, TrackId, TrackKind,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};

/// IDs of one length, as an editor's UUIDs are, so no comparison stops at
/// the length.
fn source_id(index: usize) -> SourceId {
    SourceId::new(format!("source-{index:08}")).expect("ID")
}

/// The project and its catalog.
pub fn many_sources(count: usize) -> (Project, AssetCatalog) {
    let mut track = Track::new(TrackId::new("v1").expect("ID"), "V1", TrackKind::Video);
    track.clips = (0..count)
        .map(|index| {
            let start = i64::try_from(index).expect("small") * 4_004;
            let timeline = TimeRange::new(
                MediaTime::from_ticks(start),
                MediaTime::from_ticks(start + 4_004),
            )
            .expect("range");
            let source =
                TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(4_004)).expect("range");
            Clip::new(
                ClipId::new(format!("clip-{index:08}")).expect("ID"),
                source_id(index),
                timeline,
                source,
            )
        })
        .collect();
    let project = Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("many-sources").expect("ID"),
        name: "many sources".into(),
        settings: ProjectSettings::new(
            1_920,
            1_080,
            FrameRate::new(30_000, 1_001).expect("rate"),
            48_000,
        )
        .expect("settings"),
        sources: (0..count)
            .map(|index| {
                MediaSource::new(
                    source_id(index),
                    format!("asset://{index}"),
                    MediaKind::Video,
                )
            })
            .collect(),
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline {
            tracks: vec![track],
        },
        audio: MasterAudio::default(),
    };
    let catalog = AssetCatalog::new(
        (0..count)
            .map(|index| AssetEntry::new(source_id(index), 0, OriginalState::Offline))
            .collect(),
    );
    (project, catalog)
}
