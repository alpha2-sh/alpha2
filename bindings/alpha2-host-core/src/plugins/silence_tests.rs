use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use alpha2_engine::extract::{
    ExtractRequest, ExtractSession, Need, SuppliedAudio, SuppliedFrame, Supplier, drive,
};
use alpha2_eval::edit::{DataRef, EditRequest, Ranges, Ripple, Target, apply};
use alpha2_model::{DataSet, DataValues, Project};
use serde_json::{Value, json};

use super::*;

const RATE: i64 = 16_000;
/// Silences put before, between and after the fixture's phrases, in milliseconds.
const GAPS: [i64; 7] = [500, 300, 450, 600, 900, 1500, 700];
/// One 512-sample block, in ticks.
const BLOCK_TICKS: i64 = 3_840;
/// 50 ms in ticks.
const TOLERANCE: i64 = 6_000;

fn ticks(sample: i64) -> i64 {
    sample * 120_000 / RATE
}

/// The packed plugin's directory, packed once per test process.
fn packed() -> &'static Path {
    static PACKED: OnceLock<PathBuf> = OnceLock::new();
    PACKED.get_or_init(|| {
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let out = std::env::temp_dir().join(format!("alpha2-silence-{}", std::process::id()));
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["run", "--manifest-path"])
            .arg(plugins.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(plugins.join("target"))
            .args(["-p", "alpha2-plugin-pack", "--"])
            .arg(plugins.join("silence"))
            .arg("--out")
            .arg(&out)
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("cargo runs");
        assert!(status.success(), "plugins/silence packs");
        out
    })
}

/// Fetches files relative to the packed directory.
struct Packed;

impl Fetch for Packed {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<PathBuf, PluginError> {
        Ok(packed().join(url))
    }
}

/// The fixture's samples and, in ticks, where each inserted silence lies.
fn fixture() -> (Vec<f32>, Vec<(i64, i64)>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/silence/tests/fixtures");
    let wav = std::fs::read(dir.join("speech.wav")).expect("speech.wav");
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("speech.json")).expect("json"))
            .expect("json");
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
        let start = i64::try_from(samples.len()).expect("small");
        samples.resize(
            samples.len() + usize::try_from(gap * RATE / 1000).expect("gap"),
            0.0,
        );
        silences.push((
            ticks(start),
            ticks(i64::try_from(samples.len()).expect("small")),
        ));
        if let Some(phrase) = phrases.get(index) {
            let at =
                |key: &str| usize::try_from(phrase[key].as_u64().expect("bound")).expect("small");
            samples.extend_from_slice(&speech[at("start")..at("end")]);
        }
    }
    (samples, silences)
}

/// Source audio from the fixture, silence past its end.
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

/// The silences that the artifact `policy` picks finds in the fixture, in ticks.
fn run(samples: &[f32], policy: &Policy) -> (DataSet, Vec<(i64, i64)>) {
    let manifest =
        parse_manifest(&std::fs::read_to_string(packed().join("manifest.json")).expect("manifest"))
            .expect("manifest");
    let mut plugin =
        load(&manifest, serde_json::Map::new(), policy, &mut Packed).expect("plugin loads");
    let duration = ticks(i64::try_from(samples.len()).expect("small"));
    let request: ExtractRequest = serde_json::from_value(json!({
        "extractor": "alpha2.silence", "version": 1,
        "input": { "from": "source", "source_id": "a", "range": { "start": 0, "end": duration } },
        "outputs": { "silence": "quiet" }
    }))
    .expect("request");
    let session = ExtractSession::new(&project(duration, &[]), &manifest.descriptor, request)
        .expect("session");
    let mut sets = drive(session, &mut plugin, &mut Audio(samples)).expect("run");
    let set = sets.remove(0);
    let DataValues::Intervals(intervals) = &set.values else {
        panic!("intervals");
    };
    let found = intervals
        .iter()
        .map(|interval| (interval.start.ticks(), interval.end.ticks()))
        .collect();
    (set, found)
}

#[test]
fn the_silence_plugin_finds_the_inserted_silences_and_its_cuts_keep_the_speech() {
    let (samples, inserted) = fixture();
    let (set, wasm) = run(&samples, &Policy::default());
    let (_, native) = run(
        &samples,
        &Policy {
            allow_native: true,
            ..Policy::default()
        },
    );
    assert_eq!(wasm.len(), native.len(), "{wasm:?} against {native:?}");
    for ((start, end), (native_start, native_end)) in wasm.iter().zip(&native) {
        assert!(
            (start - native_start).abs() <= BLOCK_TICKS && (end - native_end).abs() <= BLOCK_TICKS,
            "wasm {start}..{end}, native {native_start}..{native_end}"
        );
    }
    assert_eq!(wasm.len(), inserted.len(), "{wasm:?} against {inserted:?}");
    for ((start, end), (found_start, found_end)) in inserted.iter().zip(&wasm) {
        assert!(
            (found_start - start).abs() <= TOLERANCE && (found_end - end).abs() <= TOLERANCE,
            "{start}..{end} found as {found_start}..{found_end}"
        );
    }

    let duration = ticks(i64::try_from(samples.len()).expect("small"));
    let original = project(duration, std::slice::from_ref(&set));
    let (edited, report) = apply(
        &original,
        &EditRequest::RemoveRanges {
            ranges: Ranges::Data(DataRef {
                data: set.id.clone(),
            }),
            target: Target::Clips(vec![alpha2_model::ClipId::new("au").expect("id")]),
            ripple: Ripple::Timeline,
            invert: false,
            padding: MediaTime::ZERO,
            min_duration: MediaTime::ZERO,
        },
    )
    .expect("the edit applies");
    edited.validate().expect("the edited project is valid");
    let removed = report
        .removed
        .iter()
        .map(|range| (range.start().ticks(), range.end().ticks()))
        .collect::<Vec<_>>();
    assert_eq!(removed.len(), inserted.len());
    // Every kept moment plays the same source sample at its rippled time.
    let source_at = |project: &Project, time: i64| {
        let index = alpha2_eval::ActiveIndex::build(project).expect("index");
        alpha2_eval::evaluate(project, &index, MediaTime(time))
            .expect("frame")
            .audio
            .first()
            .map(|clip| clip.source_time.ticks())
    };
    for tick in (0..duration).step_by(1_000) {
        if removed
            .iter()
            .any(|(start, end)| (*start..*end).contains(&tick))
        {
            continue;
        }
        let shift = removed
            .iter()
            .filter(|(_, end)| *end <= tick)
            .map(|(start, end)| end - start)
            .sum::<i64>();
        assert_eq!(
            source_at(&original, tick),
            source_at(&edited, tick - shift),
            "kept time {tick}"
        );
    }
    let _ = std::fs::remove_dir_all(packed());
}
