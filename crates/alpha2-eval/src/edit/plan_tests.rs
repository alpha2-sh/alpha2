use super::tests::{FRAME, clip, corpus, kept_moments_hold, load, timeline_list};
use super::*;
use alpha2_model::Project;
use serde_json::{Value, json};

fn request(value: &Value) -> EditRequest {
    serde_json::from_value(value.clone()).expect("request")
}

fn split(frames: &[i64], target: &Value) -> Value {
    json!({ "op": "split_at", "target": target,
            "times": { "list": frames.iter().map(|frame| frame * FRAME).collect::<Vec<_>>(),
                       "anchor": { "type": "timeline" } } })
}

fn skipped(report: &EditReport) -> Vec<(&str, i64, SkipReason)> {
    report
        .skipped
        .iter()
        .map(|skip| (skip.clip.as_str(), skip.time.ticks() / FRAME, skip.reason))
        .collect()
}

fn one_track(clips: &[Value], transitions: &Value) -> Project {
    load(&json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [
            { "id": "v", "kind": "video", "locator": "v.mp4" },
            { "id": "w", "kind": "video", "locator": "w.mp4" }
        ],
        "timeline": { "tracks": [{ "id": "t", "name": "T", "kind": "video", "clips": clips,
                                   "transitions": transitions }] }
    }))
}

#[test]
fn a_split_inside_a_transition_skips_that_cut_and_keeps_the_rest() {
    let project = load(&corpus());
    let (edited, report) = apply(
        &project,
        &request(&split(&[20, 35, 60], &json!({ "track": "t-mix" }))),
    )
    .expect("split");
    assert_eq!(
        skipped(&report),
        [
            ("ta", 35, SkipReason::InTransition),
            ("tb", 35, SkipReason::InTransition)
        ]
    );
    assert_eq!(report.created.len(), 2);
    kept_moments_hold(&project, &edited, &report, false);
    // A cut on the transition's edge is no cut inside it.
    let (_, report) = apply(
        &project,
        &request(&split(&[30], &json!({ "track": "t-mix" }))),
    )
    .expect("split");
    assert_eq!(skipped(&report), []);
}

#[test]
fn removing_without_ripple_on_one_side_of_a_transition_trims_both_sides() {
    let project = load(&corpus());
    for target in [["ta"].as_slice(), &["tb"], &["ta", "tb"]] {
        let (edited, report) = apply(
            &project,
            &EditRequest::RemoveRanges {
                ranges: timeline_list(&[(35, 38)]),
                target: Target::Clips(
                    target
                        .iter()
                        .map(|id| alpha2_model::ClipId::new(*id).expect("id"))
                        .collect(),
                ),
                ripple: Ripple::None,
                invert: false,
                padding: MediaTime::ZERO,
                min_duration: MediaTime::ZERO,
            },
        )
        .expect("edit");
        assert!(
            edited.timeline.tracks[6].transitions.is_empty(),
            "{target:?}"
        );
        kept_moments_hold(&project, &edited, &report, false);
    }
}

/// A loop of 20 source frames over frames 0..70 dissolving into `y` over frames 50..70.
fn loop_into_a_transition() -> Project {
    let looped = clip(
        "lp",
        "v",
        (0, 70),
        (0, 20),
        &json!({ "type": "loop", "mode": "repeat" }),
    );
    let y = clip("y", "w", (50, 100), (0, 50), &json!({ "type": "forward" }));
    one_track(
        &[looped, y],
        &json!([{ "from_clip": "lp", "to_clip": "y",
                  "timeline_range": { "start": 50 * FRAME, "end": 70 * FRAME },
                  "easing": { "type": "linear" }, "effect": { "id": "alpha2.transition.fade", "version": 1 } }]),
    )
}

#[test]
fn a_loop_cut_whose_next_pass_starts_inside_a_transition_is_skipped_or_refused() {
    let project = loop_into_a_transition();
    // Cutting at 45 starts a part mid-pass, so a piece starts at the wrap at 60.
    let (edited, report) = apply(
        &project,
        &request(&split(&[45], &json!({ "clips": ["lp"] }))),
    )
    .expect("split");
    assert_eq!(skipped(&report), [("lp", 45, SkipReason::InTransition)]);
    assert_eq!(edited, project);
    let (_, report) = apply(
        &project,
        &request(&split(&[40], &json!({ "clips": ["lp"] }))),
    )
    .expect("split");
    assert_eq!(skipped(&report), []);
    let removal = json!({ "op": "remove_ranges", "ranges": { "list": [{ "start": 44 * FRAME, "end": 46 * FRAME }],
                          "anchor": { "type": "timeline" } }, "target": { "clips": ["lp"] }, "ripple": "none" });
    assert_eq!(
        apply(&project, &request(&removal)).expect_err("refused"),
        EditError::InTransition {
            clip: alpha2_model::ClipId::new("lp").expect("id"),
            time: MediaTime(60 * FRAME),
        }
    );
}

#[test]
fn negative_padding_widens_a_range_down_to_time_zero() {
    let project = load(&corpus());
    let widened = json!({ "op": "remove_ranges", "padding": -4_000,
                          "ranges": { "list": [{ "start": 0, "end": 1_000 }], "anchor": { "type": "timeline" } },
                          "target": { "track": "t-fwd" } });
    let (edited, report) = apply(&project, &request(&widened)).expect("edit");
    assert_eq!(
        report.removed,
        [TimeRange::new(MediaTime::ZERO, MediaTime(FRAME)).expect("range")]
    );
    kept_moments_hold(&project, &edited, &report, true);
}

fn faded() -> Project {
    let mut sound = clip("s", "v", (0, 60), (0, 60), &json!({ "type": "forward" }));
    sound["audio"] = json!({ "fade_in": 10 * FRAME, "fade_out": 10 * FRAME });
    one_track(&[sound], &json!([]))
}

#[test]
fn a_cut_inside_a_fade_is_skipped_or_refused_and_one_outside_it_is_exact() {
    let project = faded();
    let target = json!({ "clips": ["s"] });
    let (edited, report) = apply(&project, &request(&split(&[5, 30, 55], &target))).expect("split");
    assert_eq!(
        skipped(&report),
        [("s", 5, SkipReason::InFade), ("s", 55, SkipReason::InFade)]
    );
    kept_moments_hold(&project, &edited, &report, false);
    let removal = |from: i64, to: i64| json!({ "op": "remove_ranges", "ranges": timeline_ranges(from, to), "target": target });
    assert_eq!(
        apply(&project, &request(&removal(5, 20))).expect_err("refused"),
        EditError::InFade {
            clip: alpha2_model::ClipId::new("s").expect("id"),
            time: MediaTime(5 * FRAME),
        }
    );
    // A removal that takes the whole fade in, or none of either fade, is exact.
    for (from, to) in [(0, 12), (20, 30)] {
        let (edited, report) = apply(&project, &request(&removal(from, to))).expect("edit");
        kept_moments_hold(&project, &edited, &report, true);
    }
}

fn timeline_ranges(from: i64, to: i64) -> Value {
    json!({ "list": [{ "start": from * FRAME, "end": to * FRAME }], "anchor": { "type": "timeline" } })
}

#[test]
fn more_parts_than_a_track_holds_is_refused_by_count() {
    let maximum = alpha2_model::MAX_CLIPS_PER_TRACK;
    let frames = i64::try_from(2 * maximum + 10).expect("frames");
    let project = one_track(
        &[clip(
            "c",
            "v",
            (0, frames),
            (0, frames),
            &json!({ "type": "forward" }),
        )],
        &json!([]),
    );
    let every_other = |count: usize| {
        let ranges = (0..i64::try_from(count).expect("count"))
            .map(|k| json!({ "start": (2 * k + 1) * FRAME, "end": (2 * k + 2) * FRAME }))
            .collect::<Vec<_>>();
        json!({ "op": "remove_ranges", "ranges": { "list": ranges, "anchor": { "type": "timeline" } },
                "target": { "track": "t" }, "ripple": "none" })
    };
    assert!(apply(&project, &request(&every_other(maximum - 1))).is_ok());
    assert_eq!(
        apply(&project, &request(&every_other(maximum))).expect_err("refused"),
        EditError::TooManyClips {
            track: alpha2_model::TrackId::new("t").expect("id"),
            found: maximum + 1,
            maximum,
        }
    );
}

#[test]
fn baking_more_cues_than_a_track_holds_is_refused_by_count() {
    let mut document = corpus();
    let words = (0..5_000_i64)
        .map(|k| json!({ "start": k * 10 * FRAME, "end": k * 10 * FRAME + 5 * FRAME, "text": "Yes." }))
        .collect::<Vec<_>>();
    document["data"].as_array_mut().expect("data").push(
        json!({ "id": "speech", "anchor": { "type": "timeline" }, "kind": "words", "words": words }),
    );
    document["timeline"]["tracks"][8]["captions"] =
        json!({ "from_words": { "data": "speech", "max_chars": 40 } });
    let project = load(&document);
    let bake = json!({ "op": "bake", "targets": [{ "track": "t-subs", "property": "captions" }] });
    assert_eq!(
        apply(&project, &request(&bake)).expect_err("refused"),
        EditError::TooManyCues {
            track: alpha2_model::TrackId::new("t-subs").expect("id"),
            found: Some(5_000),
            maximum: alpha2_model::MAX_CLIPS_PER_TRACK,
        }
    );
}
