//! Standalone benchmark for indexed timeline evaluation. It prints the time
//! an evaluation takes; it never fails on it.

use std::{hint::black_box, time::Instant};

use alpha2_eval::{ActiveIndex, evaluate};
use alpha2_media::SourceId;
use alpha2_model::{
    Clip, ClipId, MasterAudio, MediaKind, MediaSource, Project, ProjectId, ProjectSettings,
    Timeline, Track, TrackId, TrackKind,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};

const TRACKS: usize = 32;
const ITERATIONS: usize = 20_000;

fn main() {
    let project = fixture();
    let index = ActiveIndex::build(&project).expect("benchmark project is valid");
    let started = Instant::now();
    for iteration in 0..ITERATIONS {
        let time = MediaTime::from_ticks(i64::try_from(iteration % 120_000).expect("small"));
        black_box(evaluate(black_box(&project), black_box(&index), time).expect("evaluation"));
    }
    let elapsed = started.elapsed();
    let nanos = elapsed.as_nanos() / u128::try_from(ITERATIONS).expect("non-zero");
    // The time is recorded, never gated; tests/alloc_budget gates the allocations an evaluation
    // makes.
    println!(
        "timeline_evaluation tracks={TRACKS} iterations={ITERATIONS} ns_per_evaluation={nanos}"
    );
}

fn fixture() -> Project {
    let source_id = SourceId::new("video").expect("ID");
    let range = TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(120_000)).expect("range");
    let tracks = (0..TRACKS)
        .map(|index| {
            let mut track = Track::new(
                TrackId::new(format!("track-{index}")).expect("ID"),
                format!("Track {index}"),
                TrackKind::Video,
            );
            track.clips.push(Clip::new(
                ClipId::new(format!("clip-{index}")).expect("ID"),
                source_id.clone(),
                range,
                range,
            ));
            track
        })
        .collect();
    Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("benchmark").expect("ID"),
        name: "timeline evaluation benchmark".into(),
        settings: ProjectSettings::new(
            1_920,
            1_080,
            FrameRate::new(30_000, 1_001).expect("rate"),
            48_000,
        )
        .expect("settings"),
        sources: vec![MediaSource::new(
            source_id,
            "benchmark://video",
            MediaKind::Video,
        )],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline { tracks },
        audio: MasterAudio::default(),
    }
}
