use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use alpha2_engine::extract::{
    ExtractRequest, ExtractSession, Need, SuppliedAudio, SuppliedFrame, Supplier, drive,
};
use alpha2_eval::edit::{DataRef, EditRequest, Target, Times, apply};
use alpha2_model::{DataSet, DataValues, Project};
use serde_json::json;

use super::tests::MANIFEST_SHA256;
use super::*;

const RATE: i64 = 22_050;
const BPM: f64 = 120.0;
const FIRST_CLICK_SECONDS: f64 = 0.5;
const LENGTH_SECONDS: f64 = 20.0;
const TOLERANCE_TICKS: i64 = 3_000;
const CLICK_SAMPLES: usize = 882;
const HOP_TICKS: i64 = 2_787;
const HALF_PROJECT_FRAME_TICKS: i64 = 2_000;

fn ticks(sample: i64) -> i64 {
    (sample * 120_000).div_euclid(RATE)
}

fn packed() -> &'static Path {
    static PACKED: OnceLock<PathBuf> = OnceLock::new();
    PACKED.get_or_init(|| {
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let out = plugins.join("target/tmp/beats-packed");
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["run", "--manifest-path"])
            .arg(plugins.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(plugins.join("target"))
            .args(["-p", "alpha2-plugin-pack", "--"])
            .arg(plugins.join("first-party/beats"))
            .arg("--out")
            .arg(&out)
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("cargo runs");
        assert!(status.success(), "plugins/first-party/beats packs");
        out
    })
}

struct Packed;

impl Fetch for Packed {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<Fetched, PluginError> {
        Ok(Fetched {
            path: packed().join(url),
            local: true,
        })
    }
}

fn click_track() -> (Vec<i64>, Vec<f32>) {
    let mut samples = vec![0.0_f32; sample_at(LENGTH_SECONDS)];
    let mut clicks = Vec::new();
    let mut at = FIRST_CLICK_SECONDS;
    while at < LENGTH_SECONDS - 0.1 {
        let first = sample_at(at);
        clicks.push(ticks(i64::try_from(first).expect("small")));
        for (offset, sample) in samples[first..first + CLICK_SAMPLES].iter_mut().enumerate() {
            let t = f64::from(u32::try_from(offset).expect("small")) / 22_050.0;
            let value = 0.8 * (std::f64::consts::TAU * 1_500.0 * t).sin() * (-t / 0.006).exp();
            #[expect(clippy::cast_possible_truncation, reason = "a sample in -1..1")]
            let value = value as f32;
            *sample += value;
        }
        at += 60.0 / BPM;
    }
    (clicks, samples)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "seconds of a short track"
)]
fn sample_at(seconds: f64) -> usize {
    (seconds * 22_050.0).round() as usize
}

struct Audio<'a>(&'a [f32]);

impl Supplier for Audio<'_> {
    fn frame(&mut self, _need: &Need) -> Result<Option<SuppliedFrame>, String> {
        Err("no video".to_owned())
    }

    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String> {
        let Need::SourceAudio {
            start_sample,
            frames,
            ..
        } = need
        else {
            return Err("not source audio".to_owned());
        };
        let samples = (0..i64::from(*frames))
            .map(|offset| {
                usize::try_from(start_sample + offset)
                    .ok()
                    .and_then(|at| self.0.get(at))
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect();
        Ok(SuppliedAudio {
            channels: 1,
            samples,
        })
    }
}

fn project(duration: i64, data: &[DataSet]) -> Project {
    Project::from_json(
        &json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [{ "id": "a", "kind": "audio", "locator": "a.wav" }],
            "timeline": { "tracks": [{ "id": "t", "name": "T", "kind": "audio", "clips": [{
                "id": "au", "source_id": "a",
                "timeline_range": { "start": 0, "end": duration }, "source_range": { "start": 0, "end": duration } }] }] },
            "data": data
        })
        .to_string(),
    )
    .expect("project")
}

fn run(samples: &[f32], policy: &Policy) -> [DataSet; 3] {
    let text = std::fs::read_to_string(packed().join("manifest.json")).expect("manifest");
    let manifest = parse_manifest(&text).expect("manifest");
    let mut plugin = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::new(),
        },
        policy,
        &mut Packed,
        &CancelToken::default(),
    )
    .expect("plugin loads");
    let duration = ticks(i64::try_from(samples.len()).expect("small"));
    let request: ExtractRequest = serde_json::from_value(json!({
        "extractor": "alpha2.beats", "version": 1, "params": {},
        "input": { "from": "source", "source_id": "a", "range": { "start": 0, "end": duration } },
        "outputs": { "beats": "beats", "onsets": "onsets", "envelope": "envelope" }
    }))
    .expect("request");
    let session = ExtractSession::new(&project(duration, &[]), &manifest.descriptor, request)
        .expect("session");
    let sets = drive(session, &mut plugin, &mut Audio(samples)).expect("run");
    ["beats", "onsets", "envelope"].map(|id| {
        sets.iter()
            .find(|set| set.id.as_str() == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} data set"))
    })
}

fn instants(set: &DataSet) -> Vec<(i64, f32)> {
    let DataValues::Intervals(intervals) = &set.values else {
        panic!("intervals");
    };
    intervals
        .iter()
        .map(|interval| {
            assert_eq!(interval.start, interval.end, "an instant");
            let score = interval.score.expect("a score");
            assert!((0.0..=1.0).contains(&score), "{score}");
            (interval.start.ticks(), score)
        })
        .collect()
}

fn assert_on_clicks(found: &[(i64, f32)], clicks: &[i64]) {
    for (time, _) in found {
        let nearest = clicks
            .iter()
            .map(|click| (click - time).abs())
            .min()
            .unwrap_or(i64::MAX);
        assert!(
            nearest <= TOLERANCE_TICKS,
            "{time} is {nearest} ticks from a click"
        );
    }
}

fn assert_close(a: &[(i64, f32)], b: &[(i64, f32)]) {
    assert_eq!(a.len(), b.len(), "{a:?} against {b:?}");
    for ((time, score), (other, other_score)) in a.iter().zip(b) {
        assert!((time - other).abs() <= HOP_TICKS, "{time} against {other}");
        assert!(
            (score - other_score).abs() <= 1e-3,
            "{score} against {other_score}"
        );
    }
}

#[test]
fn the_beats_plugin_finds_a_click_tracks_beats_as_wasm_and_native_and_split_at_cuts_on_them() {
    let (clicks, samples) = click_track();
    let native_policy = Policy {
        allow_native: true,
        ..Policy::default()
    };
    let [beats, onsets, envelope] = run(&samples, &Policy::default());
    let [native_beats, native_onsets, native_envelope] = run(&samples, &native_policy);

    let found = instants(&beats);
    assert_on_clicks(&found, &clicks);
    assert!(found.len() + 2 >= clicks.len(), "{} beats", found.len());
    let first = found.first().expect("beats").0;
    let last = found.last().expect("beats").0;
    #[expect(clippy::cast_precision_loss, reason = "a few hundred beats and ticks")]
    let tempo = 60.0 * 120_000.0 * (found.len() - 1) as f64 / (last - first) as f64;
    assert!((tempo / BPM - 1.0).abs() <= 0.01, "{tempo} BPM");
    assert_on_clicks(&instants(&onsets), &clicks);
    assert_eq!(instants(&onsets).len(), clicks.len());
    assert_close(&found, &instants(&native_beats));
    assert_close(&instants(&onsets), &instants(&native_onsets));

    let DataValues::Scalars(wasm) = &envelope.values else {
        panic!("scalars");
    };
    let DataValues::Scalars(native) = &native_envelope.values else {
        panic!("scalars");
    };
    assert_eq!(wasm.channels, ["onset", "rms"]);
    assert_eq!(wasm.times.len(), samples.len().div_ceil(512));
    assert_eq!(wasm.times, native.times);
    for (value, other) in wasm.values.iter().zip(&native.values) {
        let (value, other) = (value.expect("a value"), other.expect("a value"));
        assert!((0.0..=1.0).contains(&value));
        assert!((value - other).abs() <= 1e-4, "{value} against {other}");
    }

    let duration = ticks(i64::try_from(samples.len()).expect("small"));
    let original = project(duration, std::slice::from_ref(&beats));
    let (edited, report) = apply(
        &original,
        &EditRequest::SplitAt {
            times: Times::Data(DataRef {
                data: beats.id.clone(),
            }),
            target: Target::Clips(vec![alpha2_model::ClipId::new("au").expect("id")]),
        },
    )
    .expect("the edit applies");
    assert_eq!(report.created.len(), found.len());
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let starts = edited.timeline.tracks[0]
        .clips
        .iter()
        .skip(1)
        .map(|clip| clip.timeline_range.start().ticks());
    for (start, (beat, _)) in starts.zip(&found) {
        assert!(
            (start - beat).abs() <= HALF_PROJECT_FRAME_TICKS,
            "a cut at {start} for the beat at {beat}"
        );
    }
}
