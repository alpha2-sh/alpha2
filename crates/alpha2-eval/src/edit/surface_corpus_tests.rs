//! `tests/cases/data-edit-corpus.json`: requests over the kept-moments corpus with the SHA-256 of
//! the edited project text and the report, or the refusal, that every binding must answer.
//! `ALPHA2_UPDATE_GOLDENS=1` rewrites it.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::apply_json;
use super::tests::corpus;

const FRAME: i64 = 4_000;

fn range(start: i64, end: i64) -> Value {
    json!({ "start": start * FRAME, "end": end * FRAME })
}

fn timeline(ranges: &[(i64, i64)]) -> Value {
    json!({ "list": ranges.iter().map(|&(start, end)| range(start, end)).collect::<Vec<_>>(),
            "anchor": { "type": "timeline" } })
}

fn remove(ranges: &Value, target: &Value, extra: &Value) -> Value {
    let mut request = json!({ "op": "remove_ranges", "ranges": ranges, "target": target });
    if let (Some(request), Some(extra)) = (request.as_object_mut(), extra.as_object()) {
        request.extend(extra.clone());
    }
    request
}

fn words() -> Value {
    let mut document = corpus();
    document["data"].as_array_mut().expect("data").push(json!({
        "id": "speech", "anchor": { "type": "timeline" }, "kind": "words",
        "words": [{ "start": 0, "end": 4_000, "text": "Hi" },
                  { "start": 8_000, "end": 20_000, "text": "there", "speaker": "A" }] }));
    document["timeline"]["tracks"][8]["captions"] =
        json!({ "from_words": { "data": "speech", "max_chars": 40 } });
    document
}

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<(&'static str, &'static str, Value)> {
    let track = |id: &str| json!({ "track": id });
    let clips = |ids: &[&str]| json!({ "clips": ids });
    let none = json!({});
    vec![
        (
            "remove-timeline-ripple",
            "kept",
            remove(&timeline(&[(5, 10), (20, 25)]), &track("t-fwd"), &none),
        ),
        (
            "remove-track-ripple",
            "kept",
            remove(
                &timeline(&[(5, 10)]),
                &track("t-speed"),
                &json!({ "ripple": "track" }),
            ),
        ),
        (
            "remove-no-ripple",
            "kept",
            remove(
                &timeline(&[(5, 10)]),
                &track("t-fwd"),
                &json!({ "ripple": "none" }),
            ),
        ),
        (
            "remove-source-data",
            "kept",
            remove(&json!({ "data": "quiet" }), &clips(&["au"]), &none),
        ),
        (
            "remove-source-list",
            "kept",
            remove(
                &json!({ "list": [range(15, 25)], "anchor": { "type": "source", "source_id": "v" } }),
                &clips(&["rev", "rep", "pp", "frz"]),
                &none,
            ),
        ),
        (
            "remove-invert",
            "kept",
            remove(
                &timeline(&[(10, 20), (40, 50)]),
                &track("t-speed"),
                &json!({ "invert": true }),
            ),
        ),
        (
            "remove-padding-min-duration",
            "kept",
            remove(
                &timeline(&[(5, 8), (20, 40)]),
                &track("t-fwd"),
                &json!({ "padding": 2 * FRAME, "min_duration": 3 * FRAME }),
            ),
        ),
        (
            "remove-over-a-transition",
            "kept",
            remove(&timeline(&[(32, 38)]), &track("t-mix"), &none),
        ),
        (
            "remove-through-captions",
            "kept",
            remove(&timeline(&[(4, 14), (41, 50)]), &track("t-fwd"), &none),
        ),
        (
            "remove-loops-and-freeze",
            "kept",
            remove(&timeline(&[(12, 33)]), &clips(&["rep", "pp", "frz"]), &none),
        ),
        (
            "remove-nothing",
            "kept",
            remove(&timeline(&[]), &track("t-fwd"), &none),
        ),
        (
            "split-timeline-list",
            "kept",
            json!({ "op": "split_at",
            "times": { "list": [7 * FRAME, 33 * FRAME, 61 * FRAME], "anchor": { "type": "timeline" } },
            "target": { "track": "t-rep" } }),
        ),
        (
            "split-reverse-and-ping-pong",
            "kept",
            json!({ "op": "split_at",
            "times": { "list": [5 * FRAME, 31 * FRAME], "anchor": { "type": "timeline" } },
            "target": { "clips": ["rev", "pp"] } }),
        ),
        (
            "split-source-data",
            "kept",
            json!({ "op": "split_at", "times": { "data": "quiet" },
            "target": { "clips": ["au"] } }),
        ),
        (
            "bake-data-binding",
            "kept",
            json!({ "op": "bake",
            "targets": [{ "clip": "ta", "property": "visual.opacity" }] }),
        ),
        (
            "bake-words",
            "words",
            json!({ "op": "bake",
            "targets": [{ "track": "t-subs", "property": "captions" }] }),
        ),
        (
            "refuse-unknown-data",
            "kept",
            remove(&json!({ "data": "nope" }), &clips(&["fwd"]), &none),
        ),
        (
            "refuse-wrong-kind",
            "kept",
            remove(&json!({ "data": "beat" }), &clips(&["fwd"]), &none),
        ),
        (
            "refuse-unknown-target",
            "kept",
            remove(&timeline(&[(1, 2)]), &clips(&["ghost"]), &none),
        ),
        (
            "refuse-empty-target",
            "kept",
            remove(&timeline(&[(1, 2)]), &clips(&[]), &none),
        ),
        (
            "refuse-missing-anchor",
            "kept",
            remove(&json!({ "list": [] }), &clips(&["fwd"]), &none),
        ),
        (
            "refuse-invalid-range",
            "kept",
            remove(
                &json!({ "list": [{ "start": 9, "end": 5 }],
            "anchor": { "type": "timeline" } }),
                &clips(&["fwd"]),
                &none,
            ),
        ),
        (
            "refuse-nothing-to-bake",
            "kept",
            json!({ "op": "bake",
            "targets": [{ "clip": "fwd", "property": "visual.opacity" }] }),
        ),
        (
            "refuse-correction",
            "kept",
            json!({ "op": "bake",
            "targets": [{ "clip": "fwd", "property": "visual.correction" }] }),
        ),
        (
            "refuse-unknown-operation",
            "kept",
            json!({ "op": "remove_silence" }),
        ),
    ]
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The corpus as written to the file.
fn generated() -> String {
    let projects = json!({ "kept": corpus(), "words": words() });
    let entries = cases()
        .into_iter()
        .map(|(name, project, request)| {
            let outcome = match apply_json(&projects[project].to_string(), &request.to_string()) {
                Ok(edited) => json!({
                    "project_sha256": hex(&Sha256::digest(edited.project.as_bytes())),
                    "report": serde_json::from_str::<Value>(&edited.report).expect("report"),
                }),
                Err(failure) => json!({ "code": failure.code, "refusal": failure.refusal }),
            };
            json!({ "name": name, "project": project, "request": request, "expected": outcome })
        })
        .collect::<Vec<_>>();
    let mut text = serde_json::to_string_pretty(&json!({ "projects": projects, "cases": entries }))
        .expect("json");
    text.push('\n');
    text
}

#[test]
fn the_surface_corpus_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/data-edit-corpus.json");
    let text = generated();
    if std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, &text).expect("write the corpus");
        return;
    }
    let pinned = std::fs::read_to_string(&path).expect("tests/cases/data-edit-corpus.json");
    assert!(
        pinned == text,
        "the corpus is stale; rerun with ALPHA2_UPDATE_GOLDENS=1"
    );
}

#[test]
fn the_corpus_covers_every_refusal_and_succeeds_otherwise() {
    let corpus: Value = serde_json::from_str(&generated()).expect("json");
    let cases = corpus["cases"].as_array().expect("cases");
    let refusals = cases
        .iter()
        .filter_map(|case| case["expected"]["refusal"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(refusals.len(), 9, "{refusals:?}");
    for case in cases {
        let name = case["name"].as_str().expect("name");
        assert_eq!(
            name.starts_with("refuse-"),
            case["expected"]["refusal"].is_string(),
            "{name}: {}",
            case["expected"]
        );
    }
}
