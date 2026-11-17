//! The plugin over the speech fixture with silences inserted at known places.

// Sample counts here stay in the hundreds of thousands, far inside every integer type used.
#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use std::collections::HashMap;

use alpha2_plugin_abi::records::Record;
use alpha2_plugin_abi::status;
use alpha2_plugin_sdk::testing::Instance;
use serde_json::{Value, json};

const RATE: i64 = 16_000;
/// Silences put before, between and after the phrases, in milliseconds.
const GAPS: [i64; 7] = [500, 300, 450, 600, 900, 1500, 700];

/// The fixture's samples and, in samples, where each inserted silence lies.
fn fixture() -> (Vec<f32>, Vec<(i64, i64)>) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let wav = std::fs::read(format!("{dir}/speech.wav")).expect("speech.wav");
    let meta: Value =
        serde_json::from_slice(&std::fs::read(format!("{dir}/speech.json")).expect("json"))
            .expect("json");
    // A canonical 44-byte header, then 16-bit mono samples.
    let speech = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect::<Vec<_>>();
    let phrases = meta["phrases"].as_array().expect("phrases");
    let mut samples = Vec::new();
    let mut silences = Vec::new();
    for (index, gap) in GAPS.iter().enumerate() {
        let start = samples.len() as i64;
        samples.resize(
            samples.len() + usize::try_from(gap * RATE / 1000).expect("gap"),
            0.0,
        );
        silences.push((start, samples.len() as i64));
        if let Some(phrase) = phrases.get(index) {
            let from = phrase["start"].as_u64().expect("start") as usize;
            let to = phrase["end"].as_u64().expect("end") as usize;
            samples.extend_from_slice(&speech[from..to]);
        }
    }
    (samples, silences)
}

fn resources() -> HashMap<String, Vec<u8>> {
    let model =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/vad.onnx")).expect("model");
    HashMap::from([("vad.onnx".to_owned(), model)])
}

fn instance() -> Instance {
    // SAFETY: the plugin's own entry returns its static table.
    unsafe { Instance::new(alpha2_plugin_silence::alpha2_plugin_v1(), resources()) }
}

fn context(end: i64, params: &Value) -> String {
    json!({
        "range": { "start": 0, "end": end }, "passes": [{ "start": 0, "direction": "forward" }],
        "from": "source", "video": null,
        "audio": { "rate": 16_000, "channels": "mono", "block_frames": 512 },
        "frame_rate": null, "params": params
    })
    .to_string()
}

/// Runs the native build over `samples` and returns the silences in samples, with their scores.
fn silences(samples: &[f32], params: &Value) -> Vec<(i64, i64, f32)> {
    let mut plugin = instance();
    let end = samples.len() as i64 * 120_000 / RATE;
    assert_eq!(
        plugin.begin(&context(end, params)),
        status::OK,
        "{:?}",
        plugin.take().failure
    );
    assert_eq!(plugin.pass(0, 0), status::OK);
    for (index, block) in samples.chunks(512).enumerate() {
        let start_sample = index as i64 * 512;
        let status = plugin.audio(
            start_sample * 120_000 / RATE,
            start_sample,
            block.len() as u32,
            16_000,
            1,
            block,
        );
        assert_eq!(status, status::OK, "{:?}", plugin.take().failure);
    }
    assert_eq!(plugin.end(), status::OK, "{:?}", plugin.take().failure);
    plugin
        .take()
        .records
        .into_iter()
        .map(|record| match record {
            Record::Interval {
                start, end, score, ..
            } => (
                start * RATE / 120_000,
                end * RATE / 120_000,
                score.expect("score"),
            ),
            other => panic!("an interval, not {other:?}"),
        })
        .collect()
}

#[test]
fn every_inserted_silence_is_found_within_50_ms_and_none_inside_speech() {
    let (samples, inserted) = fixture();
    let found = silences(&samples, &json!({}));
    let tolerance = RATE * 50 / 1000;
    eprintln!("inserted {inserted:?}\nfound {found:?}");
    assert_eq!(
        found.len(),
        inserted.len(),
        "one interval per inserted silence"
    );
    for ((start, end), (found_start, found_end, score)) in inserted.iter().zip(&found) {
        assert!(
            (found_start - start).abs() <= tolerance && (found_end - end).abs() <= tolerance,
            "{start}..{end} found as {found_start}..{found_end}"
        );
        assert!((0.5..=1.0).contains(score), "{score}");
    }
}

#[test]
fn a_higher_minimum_drops_the_short_silences() {
    let (samples, inserted) = fixture();
    let found = silences(&samples, &json!({ "min_silence_ms": 800 }));
    let long = inserted
        .iter()
        .filter(|(start, end)| end - start >= RATE * 800 / 1000)
        .count();
    assert_eq!(found.len(), long);
}

#[test]
fn a_last_block_shorter_than_the_context_is_taken() {
    let (samples, _) = fixture();
    for tail in [1, 10, 63, 64, 65, 300] {
        let found = silences(&samples[..512 * 200 + tail], &json!({}));
        assert!(!found.is_empty(), "last block of {tail}");
    }
}

#[test]
fn params_outside_the_schema_fail_begin_naming_the_key() {
    for (params, key) in [
        (json!({ "threshold": "0.7" }), "threshold"),
        (json!({ "threshold": 2 }), "threshold"),
        (json!({ "min_silence_ms": -1 }), "min_silence_ms"),
        (json!({ "x": 1 }), "x"),
    ] {
        let mut plugin = instance();
        assert_eq!(plugin.begin(&context(120_000, &params)), status::FAILED);
        let failure = plugin.take().failure.expect("a reason");
        assert!(failure.contains(&format!("params.{key}")), "{failure}");
    }
    let mut plugin = instance();
    assert_eq!(plugin.begin(&context(120_000, &json!({}))), status::OK);
}
