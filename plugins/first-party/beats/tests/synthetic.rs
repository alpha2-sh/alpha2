//! The plugin over click tracks, notes and silence built in the test, with known answers.

#![expect(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "sample counts and times of a few seconds"
)]

use std::collections::HashMap;

use alpha2_plugin_abi::records::Record;
use alpha2_plugin_abi::status;
use alpha2_plugin_sdk::testing::Instance;
use serde_json::{Value, json};

const RATE: f64 = 22_050.0;
const BLOCK: usize = 512;
const TOLERANCE_SECONDS: f64 = 0.025;
const RELEASE_SECONDS: f64 = 0.1;

fn ticks(sample: i64) -> i64 {
    (sample * 120_000).div_euclid(22_050)
}

fn seconds(ticks: i64) -> f64 {
    ticks as f64 / 120_000.0
}

struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (f64::from(self.0) / f64::from(u32::MAX) * 2.0 - 1.0) as f32
    }
}

fn silence(seconds: f64) -> Vec<f32> {
    vec![0.0; (seconds * RATE) as usize]
}

fn click(samples: &mut [f32], at: f64, amplitude: f32) {
    let start = (at * RATE).round() as usize;
    for offset in 0..(0.04 * RATE) as usize {
        let Some(sample) = samples.get_mut(start + offset) else {
            break;
        };
        let t = offset as f64 / RATE;
        *sample +=
            amplitude * ((std::f64::consts::TAU * 1500.0 * t).sin() * (-t / 0.006).exp()) as f32;
    }
}

fn note(samples: &mut [f32], at: f64, frequency: f64, length: f64, amplitude: f32) {
    let start = (at * RATE).round() as usize;
    for offset in 0..((length + RELEASE_SECONDS) * RATE) as usize {
        let Some(sample) = samples.get_mut(start + offset) else {
            break;
        };
        let t = offset as f64 / RATE;
        let release = ((t - length) / RELEASE_SECONDS).clamp(0.0, 1.0);
        let gain = (t / 0.005).min(1.0) * 0.5 * (1.0 + (std::f64::consts::PI * release).cos());
        *sample += amplitude * (gain * (std::f64::consts::TAU * frequency * t).sin()) as f32;
    }
}

fn beat_times(bpm: f64, first: f64, length: f64) -> Vec<f64> {
    let count = ((length - 0.1 - first) * bpm / 60.0).ceil() as u32;
    (0..count)
        .map(|beat| first + f64::from(beat) * 60.0 / bpm)
        .collect()
}

fn click_track(times: &[f64], length: f64, amplitude: f32) -> Vec<f32> {
    let mut samples = silence(length);
    for &time in times {
        click(&mut samples, time, amplitude);
    }
    samples
}

fn instance() -> Instance {
    // SAFETY: the plugin's own entry returns its static table.
    unsafe { Instance::new(alpha2_plugin_beats::alpha2_plugin_v1(), HashMap::new()) }
}

fn context(end: i64, params: &Value) -> String {
    json!({
        "range": { "start": 0, "end": end }, "passes": [{ "start": 0, "direction": "forward" }],
        "from": "source", "video": null,
        "audio": { "rate": 22_050, "channels": "mono", "block_frames": 512 },
        "frame_rate": null, "params": params
    })
    .to_string()
}

#[derive(Debug, PartialEq)]
struct Found {
    beats: Vec<(f64, f32)>,
    onsets: Vec<(f64, f32)>,
    envelope: Vec<(i64, f32, f32)>,
}

fn run(samples: &[f32], params: &Value) -> Found {
    let mut plugin = instance();
    let end = ticks(samples.len() as i64 + 1);
    assert_eq!(
        plugin.begin(&context(end, params)),
        status::OK,
        "{:?}",
        plugin.take().failure
    );
    assert_eq!(plugin.pass(0, 0), status::OK);
    for (index, block) in samples.chunks(BLOCK).enumerate() {
        let start_sample = (index * BLOCK) as i64;
        let status = plugin.audio(
            ticks(start_sample),
            start_sample,
            block.len() as u32,
            22_050,
            1,
            block,
        );
        assert_eq!(status, status::OK, "{:?}", plugin.take().failure);
    }
    assert_eq!(plugin.end(), status::OK, "{:?}", plugin.take().failure);
    let mut found = Found {
        beats: Vec::new(),
        onsets: Vec::new(),
        envelope: Vec::new(),
    };
    for record in plugin.take().records {
        match record {
            Record::Interval {
                output,
                start,
                end,
                score,
                label: None,
            } => {
                assert_eq!(start, end, "an instant");
                assert!((0..ticks(samples.len() as i64)).contains(&start));
                let score = score.expect("a score");
                assert!((0.0..=1.0).contains(&score), "{score}");
                let list = if output == 0 {
                    &mut found.beats
                } else {
                    &mut found.onsets
                };
                list.push((seconds(start), score));
            }
            Record::Sample {
                output: 2,
                time,
                values,
            } => {
                let [Some(onset), Some(rms)] = values[..] else {
                    panic!("two values, not {values:?}");
                };
                found.envelope.push((time, onset, rms));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    found
}

fn tempo(beats: &[(f64, f32)]) -> f64 {
    let count = beats.len() as f64;
    let mean_index = (count - 1.0) / 2.0;
    let mean_time = beats.iter().map(|(time, _)| time).sum::<f64>() / count;
    let (mut covariance, mut variance) = (0.0, 0.0);
    for (index, (time, _)) in beats.iter().enumerate() {
        covariance += (index as f64 - mean_index) * (time - mean_time);
        variance += (index as f64 - mean_index).powi(2);
    }
    60.0 / (covariance / variance)
}

fn worst_distance(found: &[(f64, f32)], expected: &[f64]) -> f64 {
    found
        .iter()
        .map(|(time, _)| {
            expected
                .iter()
                .map(|other| (time - other).abs())
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0, f64::max)
}

fn assert_beats(found: &Found, expected: &[f64], bpm: f64, missing: usize) {
    let worst = worst_distance(&found.beats, expected);
    assert!(worst <= TOLERANCE_SECONDS, "a beat {worst} s from a click");
    assert!(
        found.beats.len() + missing >= expected.len(),
        "{} beats for {} clicks",
        found.beats.len(),
        expected.len()
    );
    let kept = tempo(&found.beats);
    assert!((kept / bpm - 1.0).abs() <= 0.01, "{kept} BPM, not {bpm}");
}

#[test]
fn click_tracks_give_beats_within_25_ms_and_the_tempo_within_1_percent() {
    for bpm in [90.0, 120.0, 128.0, 174.0] {
        let times = beat_times(bpm, 0.5, 30.0);
        let found = run(&click_track(&times, 30.0, 0.8), &json!({}));
        assert_beats(&found, &times, bpm, 2);
        assert!(
            worst_distance(&found.onsets, &times) <= TOLERANCE_SECONDS,
            "an onset away from the clicks at {bpm} BPM"
        );
        assert_eq!(
            found.onsets.len(),
            times.len(),
            "one onset per click at {bpm}"
        );
    }
}

#[test]
fn a_click_track_under_seeded_noise_keeps_its_beats() {
    let times = beat_times(120.0, 0.5, 30.0);
    let mut samples = click_track(&times, 30.0, 0.8);
    let mut noise = Noise(0x2545_f491);
    for sample in &mut samples {
        *sample += 0.05 * noise.next();
    }
    let found = run(&samples, &json!({}));
    assert_beats(&found, &times, 120.0, 2);
}

#[test]
fn swung_eighths_leave_the_beats_on_the_quarter_notes() {
    let quarters = beat_times(120.0, 0.5, 30.0);
    let mut samples = click_track(&quarters, 30.0, 0.8);
    for &time in &quarters {
        click(&mut samples, time + 2.0 / 3.0 * 0.5, 0.3);
    }
    let found = run(&samples, &json!({}));
    assert_beats(&found, &quarters, 120.0, 2);
}

#[test]
fn accented_bars_keep_the_quarter_note_tempo_and_score_the_downbeats_higher() {
    let quarters = beat_times(100.0, 0.5, 30.0);
    let mut samples = silence(30.0);
    for (index, &time) in quarters.iter().enumerate() {
        click(
            &mut samples,
            time,
            if index.is_multiple_of(4) { 0.9 } else { 0.25 },
        );
    }
    let found = run(&samples, &json!({ "min_bpm": 30 }));
    assert_beats(&found, &quarters, 100.0, 2);
    let (mut downbeats, mut others) = (Vec::new(), Vec::new());
    for (time, score) in &found.beats {
        let index = ((time - 0.5) / 0.6).round() as usize;
        if index.is_multiple_of(4) {
            downbeats.push(*score);
        } else {
            others.push(*score);
        }
    }
    let mean = |scores: &[f32]| scores.iter().sum::<f32>() / scores.len() as f32;
    assert!(
        mean(&downbeats) > mean(&others),
        "{downbeats:?} against {others:?}"
    );
}

#[test]
fn silence_gives_no_beats_no_onsets_and_a_zero_envelope() {
    let samples = silence(10.0);
    let found = run(&samples, &json!({}));
    assert!(found.beats.is_empty() && found.onsets.is_empty());
    assert_eq!(found.envelope.len(), samples.len().div_ceil(BLOCK));
    assert!(
        found
            .envelope
            .iter()
            .all(|&(_, onset, rms)| onset == 0.0 && rms == 0.0)
    );
}

#[test]
fn a_sparse_note_sequence_gives_one_onset_per_note() {
    let notes = [
        (0.40, 220.0, 0.30),
        (1.15, 330.0, 0.20),
        (1.70, 440.0, 0.60),
        (2.90, 262.0, 0.15),
        (3.30, 523.0, 0.40),
        (4.65, 392.0, 0.80),
        (6.10, 196.0, 0.25),
    ];
    let mut samples = silence(7.5);
    for (at, frequency, length) in notes {
        note(&mut samples, at, frequency, length, 0.5);
    }
    let found = run(&samples, &json!({}));
    let times = notes.map(|(at, ..)| at);
    assert_eq!(found.onsets.len(), times.len(), "{:?}", found.onsets);
    assert!(
        worst_distance(&found.onsets, &times) <= TOLERANCE_SECONDS,
        "{:?}",
        found.onsets
    );
}

#[test]
fn a_lower_sensitivity_drops_the_soft_notes() {
    let loud = [0.5, 1.5, 2.5, 3.5];
    let soft = [1.0, 2.0, 3.0];
    let mut samples = silence(4.5);
    for at in loud {
        note(&mut samples, at, 440.0, 0.3, 0.8);
    }
    for at in soft {
        note(&mut samples, at, 660.0, 0.3, 0.004);
    }
    let all = run(&samples, &json!({ "sensitivity": 1 }));
    let strict = run(&samples, &json!({ "sensitivity": 0 }));
    assert_eq!(
        all.onsets.len(),
        loud.len() + soft.len(),
        "{:?}",
        all.onsets
    );
    assert_eq!(strict.onsets.len(), loud.len(), "{:?}", strict.onsets);
}

#[test]
fn the_envelope_has_one_sample_per_hop_normalized_to_0_through_1() {
    let times = beat_times(120.0, 0.5, 12.0);
    let mut samples = click_track(&times, 12.0, 0.8);
    samples.truncate(samples.len() - 77);
    let found = run(&samples, &json!({}));
    assert_eq!(found.envelope.len(), samples.len().div_ceil(BLOCK));
    for (index, &(time, onset, rms)) in found.envelope.iter().enumerate() {
        assert_eq!(time, ticks((index * BLOCK) as i64));
        assert!((0.0..=1.0).contains(&onset) && (0.0..=1.0).contains(&rms));
    }
    let peak = |channel: fn(&(i64, f32, f32)) -> f32| {
        found.envelope.iter().map(channel).fold(0.0, f32::max)
    };
    assert_eq!(peak(|sample| sample.1), 1.0);
    assert_eq!(peak(|sample| sample.2), 1.0);
    assert_eq!(
        found,
        run(&samples, &json!({})),
        "the same input, the same output"
    );
}

#[test]
fn params_outside_the_schema_fail_begin_naming_the_key() {
    for (params, key) in [
        (json!({ "min_bpm": "90" }), "min_bpm"),
        (json!({ "min_bpm": 20 }), "min_bpm"),
        (json!({ "max_bpm": 301 }), "max_bpm"),
        (json!({ "sensitivity": 1.5 }), "sensitivity"),
        (json!({ "sensitivity": null }), "sensitivity"),
        (json!({ "min_bpm": 150, "max_bpm": 150 }), "min_bpm"),
        (json!({ "min_bpm": 160, "max_bpm": 120 }), "max_bpm"),
        (json!({ "tempo": 120 }), "tempo"),
    ] {
        let mut plugin = instance();
        assert_eq!(plugin.begin(&context(120_000, &params)), status::FAILED);
        let failure = plugin.take().failure.expect("a reason");
        assert!(failure.contains(&format!("params.{key}")), "{failure}");
    }
    let mut plugin = instance();
    let params = json!({ "min_bpm": 30, "max_bpm": 300, "sensitivity": 0 });
    assert_eq!(plugin.begin(&context(120_000, &params)), status::OK);
}

#[test]
fn a_run_that_is_not_one_forward_pass_from_the_start_fails_begin() {
    for passes in [
        json!([{ "start": 0, "direction": "backward" }]),
        json!([{ "start": 60_000, "direction": "forward" }]),
        json!([{ "start": 0, "direction": "forward" }, { "start": 0, "direction": "forward" }]),
    ] {
        let mut context: Value = serde_json::from_str(&context(120_000, &json!({}))).expect("json");
        context["passes"] = passes;
        let mut plugin = instance();
        assert_eq!(plugin.begin(&context.to_string()), status::FAILED);
        let failure = plugin.take().failure.expect("a reason");
        assert!(failure.contains("one forward pass"), "{failure}");
    }
}
