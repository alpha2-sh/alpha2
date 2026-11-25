use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use alpha2_engine::extract::{
    ExtractRequest, ExtractSession, Need, SuppliedAudio, SuppliedFrame, Supplier, drive,
};
use alpha2_model::{DataSet, DataValues, Project};
use serde_json::{Value, json};

use super::process_tests::isolated;
use super::tests::{MANIFEST_SHA256, native_allowed};
use super::*;

const RATE: i64 = 16_000;
/// Silences put before, between and after the fixture's phrases, in milliseconds.
const GAPS: [i64; 7] = [500, 300, 450, 600, 900, 1500, 700];
/// What the reader says in the fixture's phrases.
const SPOKEN: &str = "so too it is better to recapture an army and hence to fight and conquer in \
    all your battles is to prevent the junction of the enemy's forces policy of all is to besiege \
    walled the preparation of mantlets movable shelters will launch his men to the assault like \
    swarming ants";
const NEAR_TICKS: i64 = 18_000;

fn ticks(sample: i64) -> i64 {
    sample * 120_000 / RATE
}

/// The model `ALPHA2_TEST_WHISPER_MODEL` names (`ggml-tiny.en.bin`), or a printed note.
fn model() -> Option<PathBuf> {
    let path = std::env::var_os("ALPHA2_TEST_WHISPER_MODEL").map(PathBuf::from);
    if path.is_none() {
        eprintln!("ALPHA2_TEST_WHISPER_MODEL is not set; skipped");
    }
    path
}

/// The packed plugin's directory, packed once per test process and shared by its tests.
fn packed() -> &'static Path {
    static PACKED: OnceLock<PathBuf> = OnceLock::new();
    PACKED.get_or_init(|| {
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let out = plugins.join("target/tmp/transcribe-packed");
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["run", "--manifest-path"])
            .arg(plugins.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(plugins.join("target"))
            .args(["-p", "alpha2-plugin-pack", "--"])
            .arg(plugins.join("first-party/transcribe"))
            .arg("--out")
            .arg(&out)
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("cargo runs");
        assert!(status.success(), "plugins/first-party/transcribe packs");
        out
    })
}

/// Fetches files relative to the packed directory.
struct Packed;

impl Fetch for Packed {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<Fetched, PluginError> {
        Ok(Fetched {
            path: packed().join(url),
            local: true,
        })
    }
}

fn packed_manifest() -> PluginManifest {
    parse_manifest(&std::fs::read_to_string(packed().join("manifest.json")).expect("manifest"))
        .expect("manifest")
}

/// The fixture's samples and, in ticks, where each phrase lies.
fn fixture() -> (Vec<f32>, Vec<(i64, i64)>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/first-party/silence/tests/fixtures");
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
    let length = |samples: &Vec<f32>| i64::try_from(samples.len()).expect("small");
    let mut samples = Vec::new();
    let mut spans = Vec::new();
    for (index, gap) in GAPS.iter().enumerate() {
        samples.resize(
            samples.len() + usize::try_from(gap * RATE / 1000).expect("gap"),
            0.0,
        );
        if let Some(phrase) = phrases.get(index) {
            let at =
                |key: &str| usize::try_from(phrase[key].as_u64().expect("bound")).expect("small");
            let start = ticks(length(&samples));
            samples.extend_from_slice(&speech[at("start")..at("end")]);
            spans.push((start, ticks(length(&samples))));
        }
    }
    (samples, spans)
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

fn project(duration: i64) -> Project {
    Project::from_json(
        &json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [{ "id": "a", "kind": "audio", "locator": "a.wav" }],
            "timeline": { "tracks": [{ "id": "t", "name": "T", "kind": "audio", "clips": [{
                "id": "au", "source_id": "a",
                "timeline_range": { "start": 0, "end": duration }, "source_range": { "start": 0, "end": duration } }] }] }
        })
        .to_string(),
    )
    .expect("project")
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// One less the word error rate of `heard` against `spoken`.
fn accuracy(heard: &[&str], spoken: &[&str]) -> f64 {
    let heard: Vec<String> = heard.iter().map(|word| normalized(word)).collect();
    let spoken: Vec<String> = spoken.iter().map(|word| normalized(word)).collect();
    let mut row: Vec<usize> = (0..=spoken.len()).collect();
    for word in &heard {
        let mut diagonal = row[0];
        row[0] += 1;
        for (at, expected) in spoken.iter().enumerate() {
            let substituted = diagonal + usize::from(word != expected);
            diagonal = row[at + 1];
            row[at + 1] = substituted.min(row[at] + 1).min(row[at + 1] + 1);
        }
    }
    let errors = u32::try_from(row[spoken.len()]).expect("small");
    let words = u32::try_from(spoken.len()).expect("small");
    1.0 - f64::from(errors) / f64::from(words)
}

/// The words the plugin writes for the fixture under `policy`, and the slot files it read.
fn transcribe(
    model: &Path,
    samples: &[f32],
    policy: &Policy,
) -> (Vec<DataWord>, Vec<SuppliedFile>) {
    let manifest = packed_manifest();
    let supplied = BTreeMap::from([("model".to_owned(), model.to_owned())]);
    let mut plugin = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &supplied,
        },
        policy,
        &mut Packed,
        &CancelToken::default(),
    )
    .expect("plugin loads");
    let duration = ticks(i64::try_from(samples.len()).expect("small"));
    let request: ExtractRequest = serde_json::from_value(json!({
        "extractor": "alpha2.transcribe", "version": 1, "params": {},
        "input": { "from": "source", "source_id": "a", "range": { "start": 0, "end": duration } },
        "outputs": { "words": "transcript" }
    }))
    .expect("request");
    let session =
        ExtractSession::new(&project(duration), &manifest.descriptor, request).expect("session");
    let set: DataSet = drive(session, &mut plugin, &mut Audio(samples))
        .expect("run")
        .remove(0);
    let DataValues::Words(words) = set.values else {
        panic!("words");
    };
    (words, plugin.made_by().supplied)
}

#[test]
fn the_transcribe_plugin_writes_the_fixture_s_words_near_their_phrases() {
    let Some(model) = model() else { return };
    let (samples, spans) = fixture();
    let model_bytes = std::fs::read(&model).expect("model");
    let in_process = Policy {
        resource_root: ResourceRoot::Any,
        ..native_allowed()
    };
    let in_a_worker = Policy {
        resource_root: ResourceRoot::Any,
        ..isolated()
    };
    let (words, read) = transcribe(&model, &samples, &in_process);
    let heard: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
    let accuracy = accuracy(&heard, &SPOKEN.split(' ').collect::<Vec<_>>());
    eprintln!("word accuracy {accuracy:.3}: {heard:?}");
    assert!(accuracy >= 0.9, "{accuracy}");
    for word in &words {
        let (start, end) = (word.start.ticks(), word.end.ticks());
        assert!(
            spans
                .iter()
                .any(|(from, to)| from - NEAR_TICKS <= start && end <= to + NEAR_TICKS),
            "{word:?} lies in no phrase of {spans:?}"
        );
        assert_eq!(word.speaker, None);
    }
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].name, "model");
    assert_eq!(
        read[0].bytes,
        u64::try_from(model_bytes.len()).expect("small")
    );
    assert_eq!(read[0].sha256, sha256_hex(&model_bytes));
    let (isolated_words, isolated_read) = transcribe(&model, &samples, &in_a_worker);
    assert_eq!(isolated_words, words);
    assert_eq!(isolated_read, read);
}

#[test]
fn the_packed_manifest_offers_native_artifacts_only_which_need_native_code_allowed() {
    let manifest = packed_manifest();
    assert!(
        manifest
            .artifacts
            .iter()
            .all(|artifact| artifact.target != WASM_TARGET),
        "{:?}",
        manifest.artifacts
    );
    assert!(
        manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.target == TARGET),
        "{:?}",
        manifest.artifacts
    );
    let refused = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::new(),
        },
        &Policy::default(),
        &mut Packed,
        &CancelToken::default(),
    )
    .expect_err("refused");
    assert_eq!(refused.code, codes::NATIVE_REFUSED, "{refused}");
    let about = manifest.about.as_ref().expect("about");
    assert_eq!(about.name, "Transcribe");
    assert_eq!(
        manifest.descriptor.outputs[0].label.as_deref(),
        Some("Words")
    );
}
