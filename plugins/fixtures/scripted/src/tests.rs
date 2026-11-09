use std::collections::HashMap;

use alpha2_plugin_abi::records::Record;
use alpha2_plugin_abi::{direction, status, video_format};
use alpha2_plugin_sdk::testing::Instance;
use serde_json::json;

use super::*;

fn fresh() -> Instance {
    // SAFETY: the plugin's own entry returns its static table.
    unsafe { Instance::new(alpha2_plugin_v1(), HashMap::new()) }
}

fn context(params: &Value) -> String {
    json!({
        "range": { "start": 0, "end": 40_000 },
        "passes": [{ "start": 0, "direction": "forward" }],
        "from": "source",
        "video": { "size": 4, "format": "luma8" },
        "audio": null,
        "frame_rate": null,
        "params": params
    })
    .to_string()
}

fn started(params: &Value) -> Instance {
    let mut plugin = fresh();
    assert_eq!(plugin.begin(&context(params)), status::OK);
    assert_eq!(plugin.pass(0, direction::FORWARD), status::OK);
    plugin
}

fn frame(plugin: &mut Instance, time: i64) -> i32 {
    plugin.video(time, 2, 2, video_format::LUMA8, &[0; 4])
}

#[test]
fn every_record_kind_reaches_the_host_as_emitted() {
    let mut plugin = started(&json!({
        "per_delivery": [
            [
                { "output": "track", "time": 0, "values": [0.25, null] },
                { "output": "levels", "time": 0, "values": [1.5, -2] },
                { "output": "correction", "time": 0, "values": [0.01, -0.02, 0.5, 1.25] }
            ],
            [],
            [{ "output": "quiet", "start": 0, "end": 8000, "label": "silence", "score": 0.75 }]
        ],
        "at_end": [
            { "output": "words", "start": 100, "end": 200, "text": "caf\u{e9}", "speaker": "a" },
            { "output": "words", "start": 200, "end": 300, "text": "b" },
            { "output": "quiet", "start": 12_000, "end": 16_000 }
        ]
    }));
    for time in [0, 4000, 8000, 12_000] {
        assert_eq!(frame(&mut plugin, time), status::OK);
    }
    assert_eq!(plugin.end(), status::OK);
    let received = plugin.take();
    assert_eq!(received.failure, None);
    assert_eq!(
        received.records,
        vec![
            Record::Sample {
                output: 0,
                time: 0,
                values: vec![Some(0.25), None]
            },
            Record::Sample {
                output: 1,
                time: 0,
                values: vec![Some(1.5), Some(-2.0)]
            },
            Record::Sample {
                output: 2,
                time: 0,
                values: vec![Some(0.01), Some(-0.02), Some(0.5), Some(1.25)],
            },
            Record::Interval {
                output: 4,
                start: 0,
                end: 8000,
                label: Some("silence".to_owned()),
                score: Some(0.75),
            },
            Record::Word {
                output: 3,
                start: 100,
                end: 200,
                text: "caf\u{e9}".to_owned(),
                speaker: Some("a".to_owned()),
            },
            Record::Word {
                output: 3,
                start: 200,
                end: 300,
                text: "b".to_owned(),
                speaker: None
            },
            Record::Interval {
                output: 4,
                start: 12_000,
                end: 16_000,
                label: None,
                score: None
            },
        ]
    );
}

#[test]
fn an_unknown_output_fails_the_step_and_sends_nothing() {
    let mut plugin = started(&json!({
        "per_delivery": [[
            { "output": "levels", "time": 0, "values": [1, 2] },
            { "output": "nope", "time": 0, "values": [1] }
        ]]
    }));
    assert_eq!(frame(&mut plugin, 0), status::FAILED);
    let received = plugin.take();
    assert_eq!(received.records, Vec::new());
    assert_eq!(
        received.failure.as_deref(),
        Some("the descriptor has no output named nope")
    );
}

#[test]
fn a_failure_carries_its_message_and_a_panic_is_caught() {
    let mut plugin = started(&json!({ "fail_at": 1, "panic_at": 2 }));
    assert_eq!(frame(&mut plugin, 0), status::OK);
    assert_eq!(frame(&mut plugin, 4000), status::FAILED);
    assert_eq!(
        plugin.take().failure.as_deref(),
        Some("scripted failure at delivery 1")
    );
    assert_eq!(frame(&mut plugin, 8000), status::TRAPPED);
}

#[test]
fn steps_before_begin_bad_contexts_and_unknown_codes_fail() {
    let mut plugin = fresh();
    assert_eq!(frame(&mut plugin, 0), status::FAILED);
    assert_eq!(
        plugin.take().failure.as_deref(),
        Some("the run has not begun")
    );
    assert_eq!(plugin.begin("{"), status::FAILED);
    assert!(
        plugin
            .take()
            .failure
            .is_some_and(|why| why.starts_with("the context does not parse"))
    );
    assert_eq!(
        plugin.begin(&context(&json!({ "per_delivery": 3 }))),
        status::FAILED
    );
    assert_eq!(
        plugin.take().failure.as_deref(),
        Some("params.per_delivery is not a list")
    );
    assert_eq!(plugin.begin(&context(&json!({}))), status::OK);
    assert_eq!(plugin.begin(&context(&json!({}))), status::FAILED);
    assert_eq!(plugin.pass(0, 7), status::FAILED);
    assert_eq!(plugin.video(0, 1, 1, 9, &[0]), status::FAILED);
    assert_eq!(
        plugin.take().failure.as_deref(),
        Some("unknown video format")
    );
}

#[test]
fn the_descriptor_is_the_file() {
    let plugin = fresh();
    let described: Value = serde_json::from_str(&plugin.describe()).expect("json");
    assert_eq!(described["id"], "alpha2.test.scripted");
    assert_eq!(described["outputs"].as_array().map(Vec::len), Some(5));
}
