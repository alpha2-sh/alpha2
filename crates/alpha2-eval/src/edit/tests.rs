use super::*;
use crate::{ActiveIndex, evaluate};
use alpha2_model::Project;
use alpha2_time::MediaTime;
use serde_json::{Value, json};

const FRAME: i64 = 4_000;

fn clip(
    id: &str,
    source: &str,
    timeline: (i64, i64),
    source_range: (i64, i64),
    map: &Value,
) -> Value {
    json!({ "id": id, "source_id": source,
            "timeline_range": { "start": timeline.0 * FRAME, "end": timeline.1 * FRAME },
            "source_range": { "start": source_range.0 * FRAME, "end": source_range.1 * FRAME },
            "time_map": map })
}

fn keyed(times: &[(i64, f64)], curve: &Value) -> Value {
    json!({ "mode": "keyframes", "keyframes": times.iter().map(|(time, value)| json!({ "time": time * FRAME, "value": value, "interpolation": curve })).collect::<Vec<_>>() })
}

/// One clip of each time map on its own track, with keyframes, a mask, a transition, captions,
/// an audio clip and a timeline-anchored binding.
fn corpus() -> Value {
    let linear = json!({ "type": "linear" });
    let eased = json!({ "type": "cubic_bezier", "x1": 0.42, "y1": 0.0, "x2": 0.58, "y2": 1.0 });
    let mut forward = clip("fwd", "v", (0, 60), (10, 70), &json!({ "type": "forward" }));
    forward["visual"] = json!({ "opacity": keyed(&[(0, 0.2), (30, 1.0), (60, 0.5)], &linear),
                                "rotation_degrees": keyed(&[(5, 0.0), (50, 90.0)], &eased) });
    forward["masks"] = json!([{ "shape": { "type": "ellipse",
        "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
        "size": keyed(&[(0, 0.2), (60, 0.6)], &linear).as_object().map(|object| {
            let mut object = object.clone();
            object["keyframes"].as_array_mut().expect("keys").iter_mut().for_each(|key| {
                let value = key["value"].as_f64().expect("value");
                key["value"] = json!({ "x": value, "y": value });
            });
            Value::Object(object)
        }).expect("size") },
        "feather": keyed(&[(10, 0.0), (40, 0.1)], &linear) }]);
    let fast = clip("fast", "v", (0, 30), (0, 60), &json!({ "type": "forward" }));
    let slow = clip(
        "slow",
        "v",
        (30, 90),
        (0, 30),
        &json!({ "type": "forward" }),
    );
    let reverse = clip("rev", "v", (0, 50), (20, 70), &json!({ "type": "reverse" }));
    let repeat = clip(
        "rep",
        "v",
        (0, 70),
        (0, 20),
        &json!({ "type": "loop", "mode": "repeat" }),
    );
    let pingpong = clip(
        "pp",
        "v",
        (0, 70),
        (0, 20),
        &json!({ "type": "loop", "mode": "ping_pong" }),
    );
    let freeze = clip(
        "frz",
        "v",
        (0, 40),
        (0, 40),
        &json!({ "type": "freeze", "source_time": 50_000 }),
    );
    let mut a = clip("ta", "v", (0, 40), (0, 40), &json!({ "type": "forward" }));
    a["visual"] = json!({ "opacity": { "mode": "data", "data": "beat", "channel": "level" } });
    let b = clip("tb", "w", (30, 80), (0, 50), &json!({ "type": "forward" }));
    let mut audio = clip("au", "a", (0, 90), (0, 90), &json!({ "type": "forward" }));
    audio["audio"] = json!({ "gain": keyed(&[(0, 0.5), (90, 1.0)], &linear) });
    json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [
            { "id": "v", "kind": "video", "locator": "v.mp4" },
            { "id": "w", "kind": "video", "locator": "w.mp4" },
            { "id": "a", "kind": "audio", "locator": "a.m4a" }
        ],
        "timeline": { "tracks": [
            { "id": "t-fwd", "name": "F", "kind": "video", "clips": [forward] },
            { "id": "t-speed", "name": "S", "kind": "video", "clips": [fast, slow] },
            { "id": "t-rev", "name": "R", "kind": "video", "clips": [reverse] },
            { "id": "t-rep", "name": "L", "kind": "video", "clips": [repeat] },
            { "id": "t-pp", "name": "P", "kind": "video", "clips": [pingpong] },
            { "id": "t-frz", "name": "Z", "kind": "video", "clips": [freeze] },
            { "id": "t-mix", "name": "M", "kind": "video", "clips": [a, b], "transitions": [
                { "from_clip": "ta", "to_clip": "tb", "timeline_range": { "start": 30 * FRAME, "end": 40 * FRAME },
                  "easing": { "type": "linear" }, "effect": { "id": "alpha2.transition.fade", "version": 1 } } ] },
            { "id": "t-audio", "name": "A", "kind": "audio", "clips": [audio] },
            { "id": "t-subs", "name": "C", "kind": "caption", "captions": { "cues": [
                { "timeline_range": { "start": 2 * FRAME, "end": 22 * FRAME }, "text": [
                    { "text": "one ", "time": { "start": 0, "end": 5 * FRAME } },
                    { "text": "two", "time": { "start": 12 * FRAME, "end": 20 * FRAME } } ] },
                { "timeline_range": { "start": 40 * FRAME, "end": 44 * FRAME }, "text": "three" } ] } }
        ] },
        "data": [
            { "id": "beat", "anchor": { "type": "timeline" }, "kind": "scalars", "interpolation": "hold",
              "channels": ["level"], "times": [0, 8 * FRAME, 16 * FRAME, 24 * FRAME, 32 * FRAME], "values": [0.1, 0.9, 0.3, 0.7, 0.5] },
            { "id": "quiet", "anchor": { "type": "source", "source_id": "a" }, "kind": "intervals",
              "intervals": [{ "start": 12 * FRAME, "end": 18 * FRAME }, { "start": 25 * FRAME, "end": 25 * FRAME }] }
        ]
    })
}

fn load(document: &Value) -> Project {
    Project::from_json(&document.to_string()).expect("valid project")
}

/// What a frame shows at `time`, without clip ids (split parts have new ones).
fn state(project: &Project, time: i64) -> Vec<String> {
    let index = ActiveIndex::build(project).expect("index");
    let frame = evaluate(project, &index, MediaTime(time)).expect("frame");
    let context = index.sample_context(project);
    let aspect = 16.0 / 9.0;
    let mut out = Vec::new();
    for clip in &frame.visual {
        let masks = clip
            .masks
            .iter()
            .map(|mask| {
                format!(
                    "{:?} {}",
                    crate::evaluate_geometry(&mask.shape, MediaTime(time), aspect, &context),
                    crate::sample_scalar(&mask.feather, MediaTime(time), &context)
                )
            })
            .collect::<Vec<_>>();
        out.push(format!(
            "{} {} {:?} {:?}",
            clip.source_id,
            clip.source_time.ticks(),
            clip.visual,
            masks
        ));
    }
    for clip in &frame.audio {
        let original = project
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .find(|candidate| &candidate.id == clip.clip_id)
            .expect("clip");
        out.push(format!(
            "audio {} {} {}",
            clip.source_id,
            clip.source_time.ticks(),
            crate::sample_scalar(&original.audio.gain, MediaTime(time), &context)
        ));
    }
    for transition in &frame.transitions {
        out.push(format!("transition {}", transition.progress));
    }
    for caption in &frame.captions {
        let spoken = match caption.text {
            alpha2_model::TextContent::Runs(runs) => runs
                .iter()
                .map(|run| {
                    run.time
                        .is_some_and(|span| {
                            span.start <= caption.local_time && caption.local_time < span.end
                        })
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(","),
            alpha2_model::TextContent::Plain(_) => String::new(),
        };
        out.push(format!("caption {} {spoken}", caption.text.text()));
    }
    out
}

fn kept_moments_hold(original: &Project, edited: &Project, report: &EditReport, ripple: bool) {
    let removed = report
        .removed
        .iter()
        .map(|range| (range.start().ticks(), range.end().ticks()))
        .collect::<Vec<_>>();
    // Reversed parts mirror by frame, so inside a part's last frame they clamp to its start;
    // between frame starts only clips of other time maps are compared.
    let reversed = |project: &Project, time: i64| {
        project
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .any(|clip| {
                clip.time_map == alpha2_model::TimeMap::Reverse
                    && clip.timeline_range.contains(MediaTime(time))
            })
    };
    for frame in 0..90 {
        for tick in [frame * FRAME, frame * FRAME + 1_333] {
            let Some(moved) = new_time(tick, &removed) else {
                continue;
            };
            let at = if ripple { moved } else { tick };
            if tick % FRAME != 0 && (reversed(original, tick) || reversed(edited, at)) {
                continue;
            }
            // A transition a removed range crosses is removed, so its mix is no kept moment.
            let mixed = original
                .timeline
                .tracks
                .iter()
                .flat_map(|track| &track.transitions)
                .any(|transition| {
                    let (start, end) = (
                        transition.timeline_range.start().ticks(),
                        transition.timeline_range.end().ticks(),
                    );
                    (start..end).contains(&tick)
                        && removed.iter().any(|(from, to)| *from < end && start < *to)
                });
            if mixed {
                continue;
            }
            assert_eq!(
                state(original, tick),
                state(edited, at),
                "kept time {tick} now at {at}"
            );
        }
    }
}

fn remove(
    project: &Project,
    ranges: Ranges,
    target: Target,
    ripple: Ripple,
) -> (Project, EditReport) {
    apply(
        project,
        &EditRequest::RemoveRanges {
            ranges,
            target,
            ripple,
            invert: false,
            padding: MediaTime::ZERO,
            min_duration: MediaTime::ZERO,
        },
    )
    .expect("edit")
}

fn timeline_list(ranges: &[(i64, i64)]) -> Ranges {
    Ranges::List(RangeList {
        list: ranges
            .iter()
            .map(|(start, end)| RawRange {
                start: MediaTime(start * FRAME),
                end: MediaTime(end * FRAME),
            })
            .collect(),
        anchor: Some(alpha2_model::DataAnchor::Timeline),
    })
}

#[test]
fn every_kept_moment_renders_as_before_with_timeline_ripple() {
    let project = load(&corpus());
    for ranges in [
        vec![(5, 9)],
        vec![(13, 17), (31, 36), (61, 64)],
        vec![(0, 3), (45, 52)],
    ] {
        let (edited, report) = remove(
            &project,
            timeline_list(&ranges),
            Target::Track(alpha2_model::TrackId::new("t-fwd").expect("id")),
            Ripple::Timeline,
        );
        kept_moments_hold(&project, &edited, &report, true);
        assert_eq!(report.removed.len(), ranges.len());
    }
}

#[test]
fn source_intervals_remove_where_the_target_clip_shows_them() {
    let project = load(&corpus());
    let ranges = Ranges::Data(DataRef {
        data: alpha2_model::DataSetId::new("quiet").expect("id"),
    });
    let (edited, report) = remove(
        &project,
        ranges,
        Target::Clips(vec![alpha2_model::ClipId::new("au").expect("id")]),
        Ripple::Timeline,
    );
    // The audio clip shows source 12..18 frames at timeline 12..18; the instant removes nothing.
    assert_eq!(
        report.removed,
        vec![TimeRange::new(MediaTime(12 * FRAME), MediaTime(18 * FRAME)).expect("range")]
    );
    kept_moments_hold(&project, &edited, &report, true);
    assert_ne!(report.created, Vec::new());
}

#[test]
fn track_ripple_closes_only_the_target_track() {
    let project = load(&corpus());
    let target = Target::Track(alpha2_model::TrackId::new("t-rev").expect("id"));
    let (edited, report) = remove(&project, timeline_list(&[(10, 20)]), target, Ripple::Track);
    let rev = &edited.timeline.tracks[2];
    assert_eq!(
        rev.clips
            .iter()
            .map(|clip| clip.timeline_range.end().ticks())
            .max(),
        Some(40 * FRAME)
    );
    // Other tracks keep their times.
    assert_eq!(edited.timeline.tracks[0], project.timeline.tracks[0]);
    assert_eq!(report.removed.len(), 1);
}

#[test]
fn no_ripple_leaves_a_gap_in_the_target_clips_only() {
    let project = load(&corpus());
    let target = Target::Clips(vec![alpha2_model::ClipId::new("rep").expect("id")]);
    let (edited, report) = remove(&project, timeline_list(&[(10, 33)]), target, Ripple::None);
    kept_moments_hold(&project, &edited, &report, false);
    let loop_track = &edited.timeline.tracks[3];
    assert!(
        loop_track
            .clips
            .iter()
            .all(|clip| !(clip.timeline_range.start().ticks() < 33 * FRAME
                && 10 * FRAME < clip.timeline_range.end().ticks()))
    );
    assert_eq!(edited.timeline.tracks[4], project.timeline.tracks[4]);
}

#[test]
fn invert_keeps_exactly_the_ranges() {
    let project = load(&corpus());
    let target = Target::Clips(vec![alpha2_model::ClipId::new("fwd").expect("id")]);
    let request = EditRequest::RemoveRanges {
        ranges: timeline_list(&[(10, 20), (30, 40)]),
        target,
        ripple: Ripple::None,
        invert: true,
        padding: MediaTime::ZERO,
        min_duration: MediaTime::ZERO,
    };
    let (edited, _) = apply(&project, &request).expect("edit");
    let kept = edited.timeline.tracks[0]
        .clips
        .iter()
        .map(|clip| {
            (
                clip.timeline_range.start().ticks() / FRAME,
                clip.timeline_range.end().ticks() / FRAME,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(kept, vec![(10, 20), (30, 40)]);
}

#[test]
fn padding_and_minimum_duration_shape_the_ranges() {
    let project = load(&corpus());
    let target = Target::Clips(vec![alpha2_model::ClipId::new("fwd").expect("id")]);
    let request = EditRequest::RemoveRanges {
        ranges: timeline_list(&[(10, 20), (30, 32)]),
        target,
        ripple: Ripple::None,
        invert: false,
        padding: MediaTime(FRAME),
        min_duration: MediaTime(3 * FRAME),
    };
    let (_, report) = apply(&project, &request).expect("edit");
    assert_eq!(
        report.removed,
        vec![TimeRange::new(MediaTime(11 * FRAME), MediaTime(19 * FRAME)).expect("range")]
    );
}

#[test]
fn split_parts_continue_their_source_and_phase() {
    let project = load(&corpus());
    for track in [
        "t-fwd", "t-speed", "t-rev", "t-rep", "t-pp", "t-frz", "t-audio",
    ] {
        let times = Times::List(TimeList {
            list: vec![
                MediaTime(7 * FRAME),
                MediaTime(25 * FRAME),
                MediaTime(47 * FRAME),
            ],
            anchor: Some(alpha2_model::DataAnchor::Timeline),
        });
        let (edited, report) = apply(
            &project,
            &EditRequest::SplitAt {
                times,
                target: Target::Track(alpha2_model::TrackId::new(track).expect("id")),
            },
        )
        .expect("split");
        kept_moments_hold(&project, &edited, &report, false);
        assert!(!report.created.is_empty(), "{track}");
    }
}

#[test]
fn baking_a_binding_evaluates_alike_at_every_frame() {
    let project = load(&corpus());
    let target = BakeTarget::Clip {
        clip: alpha2_model::ClipId::new("ta").expect("id"),
        property: "visual.opacity".into(),
    };
    let (edited, _) = apply(
        &project,
        &EditRequest::Bake {
            targets: vec![target],
        },
    )
    .expect("bake");
    let clip = &edited.timeline.tracks[6].clips[0];
    assert!(matches!(
        clip.visual.opacity,
        alpha2_model::Animated::Keyframes { .. }
    ));
    for frame in 0..40 {
        assert_eq!(
            state(&project, frame * FRAME),
            state(&edited, frame * FRAME),
            "frame {frame}"
        );
    }
}

#[test]
fn baking_words_writes_their_cues() {
    let mut document = corpus();
    document["data"].as_array_mut().expect("data").push(
        json!({ "id": "speech", "anchor": { "type": "timeline" }, "kind": "words",
        "words": [{ "start": 0, "end": 4_000, "text": "Hi" }] }),
    );
    document["timeline"]["tracks"][8]["captions"] =
        json!({ "from_words": { "data": "speech", "max_chars": 40 } });
    let project = load(&document);
    let target = BakeTarget::Track {
        track: alpha2_model::TrackId::new("t-subs").expect("id"),
        property: "captions".into(),
    };
    let (edited, _) = apply(
        &project,
        &EditRequest::Bake {
            targets: vec![target],
        },
    )
    .expect("bake");
    let captions = edited.timeline.tracks[8]
        .captions
        .as_ref()
        .expect("captions");
    assert!(captions.from_words.is_none());
    assert_eq!(captions.cues.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn refusals_name_their_reason() {
    let project = load(&corpus());
    let clip = |id: &str| Target::Clips(vec![alpha2_model::ClipId::new(id).expect("id")]);
    let cases = [
        (
            EditRequest::RemoveRanges {
                ranges: Ranges::Data(DataRef {
                    data: alpha2_model::DataSetId::new("nope").expect("id"),
                }),
                target: clip("fwd"),
                ripple: Ripple::Timeline,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
            "unknown data",
        ),
        (
            EditRequest::RemoveRanges {
                ranges: Ranges::Data(DataRef {
                    data: alpha2_model::DataSetId::new("beat").expect("id"),
                }),
                target: clip("fwd"),
                ripple: Ripple::Timeline,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
            "wrong kind",
        ),
        (
            EditRequest::RemoveRanges {
                ranges: timeline_list(&[(1, 2)]),
                target: clip("ghost"),
                ripple: Ripple::Timeline,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
            "unknown target",
        ),
        (
            EditRequest::RemoveRanges {
                ranges: timeline_list(&[(1, 2)]),
                target: Target::Clips(vec![]),
                ripple: Ripple::Timeline,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
            "empty target",
        ),
        (
            EditRequest::RemoveRanges {
                ranges: Ranges::List(RangeList {
                    list: vec![],
                    anchor: None,
                }),
                target: clip("fwd"),
                ripple: Ripple::Timeline,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
            "missing anchor",
        ),
        (
            EditRequest::RemoveRanges {
                ranges: Ranges::List(RangeList {
                    list: vec![RawRange {
                        start: MediaTime(9),
                        end: MediaTime(5),
                    }],
                    anchor: Some(alpha2_model::DataAnchor::Timeline),
                }),
                target: clip("fwd"),
                ripple: Ripple::Timeline,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
            "invalid range",
        ),
        (
            EditRequest::Bake {
                targets: vec![BakeTarget::Clip {
                    clip: alpha2_model::ClipId::new("fwd").expect("id"),
                    property: "visual.opacity".into(),
                }],
            },
            "nothing to bake",
        ),
        (
            EditRequest::Bake {
                targets: vec![BakeTarget::Clip {
                    clip: alpha2_model::ClipId::new("fwd").expect("id"),
                    property: "visual.correction".into(),
                }],
            },
            "correction",
        ),
    ];
    for (request, what) in cases {
        let error = apply(&project, &request).expect_err(what);
        let matches = match what {
            "unknown data" => matches!(error, EditError::UnknownData(_)),
            "wrong kind" => matches!(error, EditError::WrongKind { .. }),
            "unknown target" => matches!(error, EditError::UnknownTarget(_)),
            "empty target" => matches!(error, EditError::EmptyTarget),
            "missing anchor" => matches!(error, EditError::MissingAnchor),
            "invalid range" => matches!(error, EditError::InvalidRange(0)),
            "nothing to bake" => matches!(error, EditError::NothingToBake(_)),
            _ => matches!(error, EditError::CorrectionNotBakeable(_)),
        };
        assert!(matches, "{what}: {error:?}");
    }
}

#[test]
fn results_are_canonical() {
    let project = load(&corpus());
    let request: EditRequest = serde_json::from_value(json!({ "op": "remove_ranges",
        "ranges": { "list": [{ "start": 20_000, "end": 60_000 }], "anchor": { "type": "timeline" } },
        "target": { "track": "t-fwd" } }))
    .expect("request");
    let first = serde_json::to_string(&apply(&project, &request).expect("edit").0).expect("json");
    let second = serde_json::to_string(&apply(&project, &request).expect("edit").0).expect("json");
    assert_eq!(first, second);
}

#[test]
fn a_no_op_edit_returns_the_same_document() {
    let project = load(&corpus());
    let (edited, report) = remove(
        &project,
        timeline_list(&[]),
        Target::Track(alpha2_model::TrackId::new("t-fwd").expect("id")),
        Ripple::Timeline,
    );
    assert_eq!(edited, project);
    assert_eq!(report, EditReport::default());
    assert_eq!(
        serde_json::to_string(&edited).expect("json"),
        serde_json::to_string(&project).expect("json")
    );
}
