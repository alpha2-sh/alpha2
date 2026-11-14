//! Replays `tests/cases/edit-corpus.json`, the cases of the TypeScript operations, against the
//! engine: a case's `timeline` says what it did to time, so its expected document must evaluate
//! as the project did at every kept moment, and its `engine` request must have the same effect.

use super::tests::{same_state, state};
use super::*;
use alpha2_model::Project;
use serde_json::Value;

fn corpus() -> Vec<Value> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases/edit-corpus.json");
    serde_json::from_slice(&std::fs::read(path).expect("the corpus")).expect("json")
}

fn project(value: &Value) -> Project {
    Project::from_json(&value.to_string()).expect("a valid project")
}

/// Where a kept tick of the old timeline is in the new one, or `None` for removed time.
fn mapped(tick: i64, timeline: &Value) -> Option<i64> {
    let ranges = |key: &str| timeline[key].as_array().cloned().unwrap_or_default();
    let removed = ranges("removed")
        .iter()
        .map(|range| {
            (
                range[0].as_i64().expect("start"),
                range[1].as_i64().expect("end"),
            )
        })
        .collect::<Vec<_>>();
    let mut tick = new_time(tick, &removed)?;
    for opening in ranges("opened") {
        let (at, length) = (
            opening["at"].as_i64().expect("at"),
            opening["length"].as_i64().expect("length"),
        );
        if tick >= at {
            tick += length;
        }
    }
    Some(tick)
}

/// Every tick worth comparing over `project`'s extent: frame starts and a point between them.
fn ticks(project: &Project) -> Vec<i64> {
    let end = project.extent().map_or(0, |extent| extent.end().ticks());
    (0..end)
        .step_by(4_000)
        .flat_map(|frame| [frame, frame + 1_333])
        .collect()
}

/// Whether a reversed clip shows `tick` off its frame starts, where parts mirror by frame.
fn reversed_between_frames(project: &Project, tick: i64) -> bool {
    tick % 4_000 != 0
        && project
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .any(|clip| {
                clip.time_map == alpha2_model::TimeMap::Reverse
                    && clip.timeline_range.contains(MediaTime(tick))
            })
}

fn assert_kept(name: &str, before: &Project, after: &Project, timeline: &Value) {
    for tick in ticks(before) {
        let Some(at) = mapped(tick, timeline) else {
            continue;
        };
        if reversed_between_frames(before, tick) || reversed_between_frames(after, at) {
            continue;
        }
        let (old, new) = (state(before, tick), state(after, at));
        assert!(
            same_state(&old, &new),
            "{name}: kept time {tick} now at {at}\n{old:#?}\n{new:#?}"
        );
    }
}

#[test]
fn the_typescript_results_keep_every_kept_moment() {
    let mut checked = 0;
    for case in corpus() {
        let (Some(timeline), Some(expected)) = (case.get("timeline"), case.get("expected")) else {
            continue;
        };
        assert_eq!(timeline["tracks"], "all", "{}", case["name"]);
        let name = case["name"].as_str().expect("name");
        assert_kept(
            name,
            &project(&case["project"]),
            &project(expected),
            timeline,
        );
        checked += 1;
    }
    assert!(checked >= 5, "{checked}");
}

#[test]
fn the_engine_request_of_a_case_has_its_effect() {
    let mut checked = 0;
    for case in corpus() {
        let Some(engine) = case.get("engine") else {
            continue;
        };
        let name = case["name"].as_str().expect("name");
        let before = project(&case["project"]);
        let request: EditRequest =
            serde_json::from_value(engine["request"].clone()).expect("request");
        let result = apply(&before, &request);
        if let Some(refusal) = engine.get("refusal") {
            let error = result.expect_err(name);
            assert_eq!(Some(error.kind()), refusal.as_str(), "{name}");
            checked += 1;
            continue;
        }
        let (after, report) = result.unwrap_or_else(|error| panic!("{name}: {error}"));
        let reasons = report
            .skipped
            .iter()
            .map(|skip| serde_json::to_value(skip.reason).expect("reason"))
            .collect::<Vec<_>>();
        let expected_skips = engine["skipped"].as_array().cloned().unwrap_or_default();
        assert_eq!(reasons, expected_skips, "{name}");
        match (case.get("expected"), case.get("timeline")) {
            // The engine and the TypeScript result agree wherever the edit kept time.
            (Some(expected), Some(timeline)) => {
                let expected = project(expected);
                for tick in ticks(&before) {
                    let Some(at) = mapped(tick, timeline) else {
                        continue;
                    };
                    if reversed_between_frames(&after, at) {
                        continue;
                    }
                    let (engine, typescript) = (state(&after, at), state(&expected, at));
                    assert!(
                        same_state(&engine, &typescript),
                        "{name} at {at}\n{engine:#?}\n{typescript:#?}"
                    );
                }
            }
            // Where TypeScript refuses, the engine's split or skip keeps every moment.
            _ => assert_kept(name, &before, &after, &Value::Null),
        }
        checked += 1;
    }
    assert!(checked >= 5, "{checked}");
}
