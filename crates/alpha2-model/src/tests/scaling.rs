//! Validation and lint that take time linear in the document, and count limits at their maximum.

use super::*;

#[test]
fn lint_looks_for_hidden_clips_in_time_linear_in_the_clips() {
    // Up to as many clips as a project holds, each with its own source, under a full-length
    // overlay that hides nothing.
    let with_clips = |count: usize| {
        let mut project = project();
        let frame = 4_004;
        let mut tracks: Vec<Track> = Vec::new();
        let mut sources = Vec::new();
        for index in 0..count {
            let at = index % crate::MAX_CLIPS_PER_TRACK;
            if at == 0 {
                tracks.push(Track::new(
                    id(TrackId::new, &format!("t{}", tracks.len())),
                    "V",
                    TrackKind::Video,
                ));
            }
            let source = id(SourceId::new, &format!("s{index}"));
            sources.push(MediaSource::new(
                source.clone(),
                format!("{index}.mp4"),
                MediaKind::Video,
            ));
            let start = MediaTime(i64::try_from(at).expect("index") * frame);
            let range = TimeRange::new(start, MediaTime(start.0 + frame)).expect("range");
            tracks.last_mut().expect("a track").clips.push(Clip::new(
                id(ClipId::new, &format!("c{index}")),
                source,
                range,
                range,
            ));
        }
        let overlay = id(SourceId::new, "overlay");
        sources.push(MediaSource::new(
            overlay.clone(),
            "overlay.png".to_owned(),
            MediaKind::Image,
        ));
        let mut top = Track::new(id(TrackId::new, "top"), "Overlay", TrackKind::Video);
        let whole = TimeRange::new(MediaTime(0), MediaTime(4_096 * frame)).expect("range");
        top.clips.push(Clip::new(
            id(ClipId::new, "watermark"),
            overlay,
            whole,
            whole,
        ));
        tracks.push(top);
        project.sources = sources;
        project.timeline = Timeline { tracks };
        project
    };
    let issues = with_clips(crate::MAX_CLIPS - 1).lint();
    assert!(
        !issues
            .iter()
            .any(|issue| matches!(issue, crate::LintIssue::HiddenClip { .. })),
        "{issues:?}"
    );
    alpha2_scaling_support::assert_linear(
        "lint",
        (crate::MAX_CLIPS - 1) / alpha2_scaling_support::GROWTH,
        with_clips,
        |project| {
            std::hint::black_box(project.lint());
        },
    );
}

#[test]
fn an_invalid_matte_is_reported_on_the_first_clip_that_has_one() {
    let mut project = project();
    let template = project.timeline.tracks[0].clips[0].clone();
    project.timeline.tracks[0].clips = (0..6_i64)
        .map(|index| {
            let mut clip = template.clone();
            clip.id = id(ClipId::new, &format!("x{index}"));
            let start = MediaTime(index * 120_000);
            clip.timeline_range =
                TimeRange::new(start, MediaTime(start.0 + 120_000)).expect("range");
            // Every clip its own matte: all six are invalid.
            clip.matte = Some(crate::Matte {
                source: clip.id.clone(),
                mode: crate::MatteMode::Alpha,
            });
            clip
        })
        .collect();
    // Each validation hashes with a fresh seed, so an order a map chose would differ between these.
    for _ in 0..64 {
        assert_eq!(
            project.validate(),
            Err(crate::ModelError::InvalidMatte {
                clip: id(ClipId::new, "x0")
            })
        );
    }
}

#[test]
fn validation_finds_sources_and_compositions_in_time_linear_in_the_document() {
    // Up to as many compositions as a project holds: a clip for each, with its own source and its
    // own composition, as the editor makes them, and one in 156 (64 of the most) of media nodes.
    let with_compositions = |total: usize| {
        let mut project = project();
        let frame = 4_004;
        let mut tracks = Vec::new();
        let mut sources = Vec::new();
        let mut compositions = Vec::new();
        let mut index = 0_i64;
        let media_compositions = total / 156;
        let clip_count = total - media_compositions;
        let per_track = crate::MAX_CLIPS_PER_TRACK;
        for (track, first) in (0..clip_count).step_by(per_track).enumerate() {
            let count = per_track.min(clip_count - first);
            let mut lane = Track::new(
                id(TrackId::new, &format!("t{track}")),
                "V",
                TrackKind::Video,
            );
            for at in 0..count {
                let source = id(SourceId::new, &format!("s{index}"));
                sources.push(MediaSource::new(
                    source.clone(),
                    format!("{index}.mp4"),
                    MediaKind::Video,
                ));
                let composition = id(crate::CompositionId::new, &format!("c{index}-fx"));
                compositions.push(crate::Composition {
                    id: composition.clone(),
                    nodes: vec![crate::CompositionNode::new(
                        id(crate::NodeId::new, "clip"),
                        crate::NodeOp::Clip {},
                    )],
                    output: Some(id(crate::NodeId::new, "clip")),
                    inputs: Vec::new(),
                    controls: Vec::new(),
                });
                let start = MediaTime(i64::try_from(at).expect("index") * frame);
                let range = TimeRange::new(start, MediaTime(start.0 + frame)).expect("range");
                let mut clip =
                    Clip::new(id(ClipId::new, &format!("c{index}")), source, range, range);
                clip.composition = Some(crate::ClipComposition::new(composition));
                lane.clips.push(clip);
                index += 1;
            }
            tracks.push(lane);
        }
        // And compositions of media nodes that all read the last source.
        let last = sources.last().expect("a source").id.clone();
        for group in 0..media_compositions {
            compositions.push(crate::Composition {
                id: id(crate::CompositionId::new, &format!("media-{group}")),
                nodes: (0..crate::MAX_COMPOSITION_NODES)
                    .map(|node| {
                        crate::CompositionNode::new(
                            id(crate::NodeId::new, &format!("m{node}")),
                            crate::NodeOp::Media {
                                source_id: last.clone(),
                                start: MediaTime(0),
                            },
                        )
                    })
                    .collect(),
                output: None,
                inputs: Vec::new(),
                controls: Vec::new(),
            });
        }
        project.sources = sources;
        project.compositions = compositions;
        project.timeline = Timeline { tracks };
        project
    };
    with_compositions(crate::MAX_COMPOSITIONS)
        .validate()
        .expect("a large valid project");
    alpha2_scaling_support::assert_linear(
        "validation",
        crate::MAX_COMPOSITIONS / alpha2_scaling_support::GROWTH,
        with_compositions,
        |project| project.validate().expect("a large valid project"),
    );
}

/// A document at a count limit, made rather than stored: `count` sources,
/// clips, clips on one track, transitions on one track, or compositions.
/// `packages/alpha2/test/project.test.mjs` makes the same documents.
fn document_with(limit: &str, count: usize) -> String {
    use serde_json::json;
    let clip = |index: usize, source: &str| {
        json!({
            "id": format!("c{index}"),
            "source_id": source,
            "timeline_range": {"start": index * 10, "end": index * 10 + 10},
            "source_range": {"start": 0, "end": 10}
        })
    };
    let track = |id: &str, clips: Vec<serde_json::Value>, transitions: Vec<serde_json::Value>| json!({"id": id, "name": id, "kind": "video", "clips": clips, "transitions": transitions});
    let mut sources = vec![json!({"id": "v", "kind": "video", "locator": "v.mp4"})];
    let mut tracks = Vec::new();
    let mut compositions = Vec::new();
    match limit {
        "sources" => sources.extend(
            (1..count).map(|index| json!({"id": format!("s{index}"), "kind": "audio", "locator": format!("{index}.m4a")})),
        ),
        "clips_on_a_track" => tracks.push(track("t", (0..count).map(|index| clip(index, "v")).collect(), Vec::new())),
        "clips" => {
            let mut index = 0;
            while index < count {
                let end = (index + crate::MAX_CLIPS_PER_TRACK).min(count);
                tracks.push(track(&format!("t{index}"), (index..end).map(|at| clip(at, "v")).collect(), Vec::new()));
                index = end;
            }
        }
        "transitions_on_a_track" => {
            let transition = json!({
                "from_clip": "c0", "to_clip": "c1",
                "timeline_range": {"start": 5, "end": 10},
                "easing": {"type": "linear"},
                "effect": {"id": "cross_dissolve", "version": 1, "params": {}}
            });
            tracks.push(track("t", vec![clip(0, "v"), clip(1, "v")], vec![transition; count]));
        }
        "compositions" => compositions.extend((0..count).map(|index| {
            json!({"id": format!("k{index}"), "nodes": [{"id": "clip", "op": {"type": "clip"}}], "output": "clip"})
        })),
        other => panic!("no limit {other}"),
    }
    json!({
        "schema_version": crate::CURRENT_SCHEMA_VERSION,
        "id": "limits",
        "name": "Limits",
        "settings": {"width": 64, "height": 36, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": sources,
        "compositions": compositions,
        "timeline": {"tracks": tracks}
    })
    .to_string()
}

#[test]
fn count_limits_admit_their_maximum_and_refuse_one_more() {
    use crate::{ModelError, ProjectLoadError};
    let refused = |result: Result<Project, ProjectLoadError>| match result {
        Err(ProjectLoadError::InvalidProject(error)) => Some(error),
        _ => None,
    };
    for (limit, maximum) in [
        ("sources", crate::MAX_CLIPS),
        ("clips_on_a_track", crate::MAX_CLIPS_PER_TRACK),
        ("clips", crate::MAX_CLIPS),
        ("transitions_on_a_track", crate::MAX_TRANSITIONS_PER_TRACK),
        ("compositions", crate::MAX_COMPOSITIONS),
    ] {
        let at = Project::from_json(&document_with(limit, maximum));
        let over = refused(Project::from_json(&document_with(limit, maximum + 1)));
        let over_the_limit = |error: &Option<ModelError>| {
            matches!(
                (limit, error),
                ("sources", Some(ModelError::TooManySources { .. }))
                    | (
                        "clips_on_a_track",
                        Some(ModelError::TooManyClipsOnTrack { .. })
                    )
                    | ("clips", Some(ModelError::TooManyClips { .. }))
                    | (
                        "transitions_on_a_track",
                        Some(ModelError::TooManyTransitions { .. })
                    )
                    | ("compositions", Some(ModelError::TooManyCompositions { .. }))
            )
        };
        assert!(
            over_the_limit(&over),
            "{limit} at {}: {over:?}",
            maximum + 1
        );
        // Two clips hold one transition, so a full track of them is refused, but not for its count.
        if limit == "transitions_on_a_track" {
            assert!(!over_the_limit(&refused(at)), "{limit} at {maximum}");
        } else {
            assert!(at.is_ok(), "{limit} at {maximum}: {:?}", at.err());
        }
    }
}
