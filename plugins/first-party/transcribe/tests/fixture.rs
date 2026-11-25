//! The plugin over the speech fixture of `alpha2.silence`, with silences inserted at known places,
//! and over files that are not whisper models. Tests that transcribe read the model from
//! `ALPHA2_TEST_WHISPER_MODEL` (`ggml-tiny.en.bin`) and print a note and pass when it is unset.

#![expect(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "sample counts of minutes of audio fit every integer and float type used"
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alpha2_plugin_abi::records::Record;
use alpha2_plugin_abi::{direction, status};
use alpha2_plugin_sdk::testing::{Instance, Offered};
use serde_json::{Value, json};

const RATE: i64 = 16_000;
const BLOCK: usize = 16_000;
const GAPS_MS: [i64; 7] = [500, 300, 450, 600, 900, 1500, 700];
const SPOKEN: [&str; 6] = [
    "so too it is better to recapture an army",
    "and hence to fight and conquer in all your battles",
    "is to prevent the junction of the enemy's forces",
    "policy of all is to besiege walled",
    "the preparation of mantlets movable shelters",
    "will launch his men to the assault like swarming ants",
];
const NEAR_SECONDS: f64 = 0.15;

fn model_from(variable: &str) -> Option<PathBuf> {
    let path = std::env::var_os(variable).map(PathBuf::from);
    if path.is_none() {
        eprintln!("{variable} is not set; skipped");
    }
    path
}

fn model() -> Option<PathBuf> {
    model_from("ALPHA2_TEST_WHISPER_MODEL")
}

fn fixture() -> (Vec<f32>, Vec<(f64, f64)>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../silence/tests/fixtures");
    let wav = std::fs::read(dir.join("speech.wav")).expect("speech.wav");
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("speech.json")).expect("json"))
            .expect("json");
    let speech: Vec<f32> = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect();
    let phrases = meta["phrases"].as_array().expect("phrases");
    let mut samples = Vec::new();
    let mut spans = Vec::new();
    for (index, gap) in GAPS_MS.iter().enumerate() {
        samples.resize(samples.len() + (gap * RATE / 1000) as usize, 0.0);
        if let Some(phrase) = phrases.get(index) {
            let from = phrase["start"].as_u64().expect("start") as usize;
            let to = phrase["end"].as_u64().expect("end") as usize;
            let start = seconds(samples.len());
            samples.extend_from_slice(&speech[from..to]);
            spans.push((start, seconds(samples.len())));
        }
    }
    (samples, spans)
}

fn repeated(copies: usize, pause_seconds: usize) -> (Vec<f32>, f64) {
    let (one, _) = fixture();
    let mut samples = Vec::new();
    for _ in 0..copies {
        samples.extend_from_slice(&one);
        samples.resize(samples.len() + pause_seconds * RATE as usize, 0.0);
    }
    let period = seconds(one.len() + pause_seconds * RATE as usize);
    (samples, period)
}

fn seconds(samples: usize) -> f64 {
    samples as f64 / RATE as f64
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

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
    1.0 - row[spoken.len()] as f64 / spoken.len() as f64
}

fn spoken_words(copies: usize) -> Vec<&'static str> {
    let once: Vec<&str> = SPOKEN.iter().flat_map(|phrase| phrase.split(' ')).collect();
    once.iter()
        .copied()
        .cycle()
        .take(copies * once.len())
        .collect()
}

fn instance(model: &Path) -> Instance {
    // SAFETY: the plugin's own entry returns its static table.
    unsafe {
        Instance::offering(
            alpha2_plugin_transcribe::alpha2_plugin_v1(),
            Offered {
                paths: [("model".to_owned(), model.to_owned())].into(),
                ..Offered::default()
            },
        )
    }
}

fn context(end: i64, params: &Value, model: Option<&Path>, passes: &Value) -> String {
    let supplied = model.map_or(json!({}), |model| {
        let bytes = std::fs::metadata(model).map_or(0, |file| file.len());
        json!({ "model": { "shape": "file", "files": [{ "name": "model", "bytes": bytes }] } })
    });
    json!({
        "range": { "start": 0, "end": end }, "passes": passes,
        "from": "source", "video": null,
        "audio": { "rate": 16_000, "channels": "mono", "block_frames": BLOCK },
        "frame_rate": null, "params": params, "supplied": supplied
    })
    .to_string()
}

fn refusal(model: Option<&Path>, params: &Value) -> String {
    let mut plugin = instance(model.unwrap_or(Path::new("/nonexistent")));
    let passes = json!([{ "start": 0, "direction": "forward" }]);
    assert_eq!(
        plugin.begin(&context(120_000, params, model, &passes)),
        status::FAILED
    );
    plugin.take().failure.expect("a reason")
}

#[derive(Clone, Debug, PartialEq)]
struct Heard {
    start: f64,
    end: f64,
    text: String,
}

struct Run {
    words: Vec<Heard>,
    /// How many words each call emitted, `end` last.
    per_call: Vec<usize>,
    slowest: Duration,
}

impl Run {
    fn texts(&self) -> Vec<&str> {
        self.words.iter().map(|word| word.text.as_str()).collect()
    }
}

type Pass = (u32, Vec<usize>);

fn forward(samples: &[f32]) -> Vec<Pass> {
    vec![(
        direction::FORWARD,
        (0..samples.len().div_ceil(BLOCK)).collect(),
    )]
}

fn pass_json((way, blocks): &Pass) -> Value {
    let (first, way) = if *way == direction::FORWARD {
        (blocks[0] * BLOCK, "forward")
    } else {
        ((blocks[0] + 1) * BLOCK, "backward")
    };
    json!({ "start": first as i64 * 120_000 / RATE, "direction": way })
}

fn run(model: &Path, samples: &[f32], params: &Value, passes: &[Pass]) -> Run {
    let mut plugin = instance(model);
    let end = samples.len() as i64 * 120_000 / RATE;
    let described: Vec<Value> = passes.iter().map(pass_json).collect();
    assert_eq!(
        plugin.begin(&context(end, params, Some(model), &json!(described))),
        status::OK,
        "{:?}",
        plugin.take().failure
    );
    let mut words = Vec::new();
    let mut per_call = Vec::new();
    let mut slowest = Duration::ZERO;
    let mut take = |plugin: &mut Instance| {
        let received = plugin.take();
        let before = words.len();
        words.extend(received.records.into_iter().map(|record| match record {
            Record::Word {
                start,
                end,
                text,
                speaker,
                ..
            } => {
                assert_eq!(speaker, None);
                Heard {
                    start: start as f64 / 120_000.0,
                    end: end as f64 / 120_000.0,
                    text,
                }
            }
            other => panic!("a word, not {other:?}"),
        }));
        per_call.push(words.len() - before);
    };
    for (index, (way, blocks)) in passes.iter().enumerate() {
        assert_eq!(plugin.pass(index as u32, *way), status::OK);
        for &block in blocks {
            let start_sample = block * BLOCK;
            let delivered = &samples[start_sample..(start_sample + BLOCK).min(samples.len())];
            let started = Instant::now();
            let status = plugin.audio(
                start_sample as i64 * 120_000 / RATE,
                start_sample as i64,
                delivered.len() as u32,
                16_000,
                1,
                delivered,
            );
            slowest = slowest.max(started.elapsed());
            assert_eq!(status, status::OK, "{:?}", plugin.take().failure);
            take(&mut plugin);
        }
    }
    let started = Instant::now();
    assert_eq!(plugin.end(), status::OK, "{:?}", plugin.take().failure);
    slowest = slowest.max(started.elapsed());
    take(&mut plugin);
    for pair in words.windows(2) {
        assert!(pair[1].start >= pair[0].end, "{pair:?}");
    }
    for word in &words {
        assert!(
            0.0 <= word.start && word.start < word.end && word.end <= seconds(samples.len()),
            "{word:?}"
        );
        assert!(
            !word.text.is_empty() && word.text.trim() == word.text,
            "{word:?}"
        );
    }
    Run {
        words,
        per_call,
        slowest,
    }
}

#[test]
fn the_fixture_is_transcribed_with_each_word_near_its_phrase() {
    let Some(model) = model() else { return };
    let (samples, spans) = fixture();
    let run = run(&model, &samples, &json!({}), &forward(&samples));
    let accuracy = accuracy(&run.texts(), &spoken_words(1));
    eprintln!("word accuracy {accuracy:.3}: {:?}", run.words);
    assert!(accuracy >= 0.9, "{accuracy}");
    for word in &run.words {
        assert!(
            spans
                .iter()
                .any(|(start, end)| start - NEAR_SECONDS <= word.start
                    && word.end <= end + NEAR_SECONDS),
            "{word:?} lies in no phrase of {spans:?}"
        );
    }
    for (start, end) in &spans {
        assert!(
            run.words
                .iter()
                .any(|word| (word.start - start).abs() <= NEAR_SECONDS),
            "no word starts the phrase {start}..{end}"
        );
        assert!(
            run.words
                .iter()
                .any(|word| (word.end - end).abs() <= NEAR_SECONDS),
            "no word ends the phrase {start}..{end}"
        );
    }
}

#[test]
fn a_long_input_is_transcribed_window_by_window_with_each_overlap_said_once() {
    let Some(model) = model() else { return };
    let copies = 12;
    let (samples, period) = repeated(copies, 2);
    let run = run(&model, &samples, &json!({}), &forward(&samples));
    eprintln!(
        "{:.0} s of audio, slowest call {:?}, words per call {:?}",
        seconds(samples.len()),
        run.slowest,
        run.per_call
    );
    assert!(run.slowest < Duration::from_secs(60), "{:?}", run.slowest);
    let (emitted_in_end, emitted_before) = run.per_call.split_last().expect("calls");
    let emitting_calls = emitted_before.iter().filter(|count| **count > 0).count();
    assert!(emitting_calls >= 8, "{:?}", run.per_call);
    assert!(
        *emitted_in_end * 5 < run.words.len(),
        "{emitted_in_end} of {} words came at the end",
        run.words.len()
    );
    let whole = accuracy(&run.texts(), &spoken_words(copies));
    eprintln!("word accuracy {whole:.3}");
    assert!(whole >= 0.9, "{whole}");
    for copy in 0..copies {
        let (from, to) = (copy as f64 * period, (copy + 1) as f64 * period);
        let heard: Vec<&str> = run
            .words
            .iter()
            .filter(|word| (from..to).contains(&word.start))
            .map(|word| word.text.as_str())
            .collect();
        let accuracy = accuracy(&heard, &spoken_words(1));
        assert!(accuracy >= 0.85, "copy {copy}: {accuracy} {heard:?}");
    }
}

#[test]
fn the_same_audio_gives_the_same_words_whatever_the_passes() {
    let Some(model) = model() else { return };
    let (samples, _) = repeated(3, 1);
    let blocks = samples.len().div_ceil(BLOCK);
    let seed = 37;
    let once = run(&model, &samples, &json!({}), &forward(&samples));
    let again = run(&model, &samples, &json!({}), &forward(&samples));
    assert_eq!(once.words, again.words);
    let from_a_seed = run(
        &model,
        &samples,
        &json!({}),
        &[
            (direction::FORWARD, (seed..blocks).collect()),
            (direction::BACKWARD, (0..seed).rev().collect()),
        ],
    );
    assert_eq!(once.words, from_a_seed.words);
    let (emitted_in_end, _) = from_a_seed.per_call.split_last().expect("calls");
    assert_eq!(*emitted_in_end, once.words.len());
}

#[test]
fn a_multilingual_model_detects_english_and_keeps_it() {
    let Some(model) = model_from("ALPHA2_TEST_WHISPER_MODEL_MULTILINGUAL") else {
        return;
    };
    let (samples, _) = repeated(2, 1);
    for params in [json!({}), json!({ "language": "en" })] {
        let run = run(&model, &samples, &params, &forward(&samples));
        let accuracy = accuracy(&run.texts(), &spoken_words(2));
        eprintln!("{params}: word accuracy {accuracy:.3}");
        assert!(accuracy >= 0.85, "{params}: {accuracy} {:?}", run.texts());
    }
}

fn ggml(vocabulary: i32, audio_context: i32, tensor_bytes: usize) -> Vec<u8> {
    let mut bytes = 0x6767_6d6c_u32.to_le_bytes().to_vec();
    let mut push = |values: &[i32]| {
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    };
    push(&[vocabulary, audio_context, 384, 6, 4, 448, 384, 6, 4, 80, 1]);
    push(&[80, 201]);
    push(&vec![0; 80 * 201]);
    push(&[0]);
    push(&[2, 1, 1, 2, 2]);
    bytes.push(b'x');
    bytes.extend(std::iter::repeat_n(0, tensor_bytes));
    bytes
}

#[test]
fn a_file_that_is_not_a_whole_whisper_model_fails_begin_saying_why() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("transcribe-wrong-models");
    std::fs::create_dir_all(&dir).expect("dir");
    let onnx = Path::new(env!("CARGO_MANIFEST_DIR")).join("../silence/resources/vad.onnx");
    let mut gguf = b"GGUF".to_vec();
    gguf.extend([0; 60]);
    let cases: [(&str, Vec<u8>, &str); 6] = [
        (
            "empty",
            Vec::new(),
            "not a whole whisper.cpp ggml model: it ends in the magic",
        ),
        (
            "onnx",
            std::fs::read(onnx).expect("onnx"),
            "does not start with the ggml magic",
        ),
        ("gguf", gguf, "it is a GGUF file"),
        (
            "context",
            ggml(51_864, 1_499, 8),
            "its audio context is 1499",
        ),
        (
            "vocabulary",
            ggml(32_000, 1_500, 8),
            "its vocabulary has 32000 tokens",
        ),
        ("truncated", ggml(51_864, 1_500, 4), "it ends in tensor x"),
    ];
    for (name, bytes, expected) in cases {
        let path = dir.join(format!("{name}.bin"));
        std::fs::write(&path, bytes).expect("write");
        let failure = refusal(Some(&path), &json!({}));
        assert!(failure.contains(expected), "{name}: {failure}");
    }
    let whole = dir.join("whole.bin");
    std::fs::write(&whole, ggml(51_864, 1_500, 8)).expect("write");
    let failure = refusal(Some(&whole), &json!({}));
    assert!(
        failure.contains("whisper.cpp could not load the model"),
        "{failure}"
    );
    let failure = refusal(Some(&whole), &json!({ "language": "de" }));
    assert!(failure.contains("transcribes English only"), "{failure}");
    let failure = refusal(None, &json!({}));
    assert!(failure.contains("the model slot is empty"), "{failure}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn params_outside_the_schema_fail_begin_naming_the_key() {
    for (params, key) in [
        (json!({ "language": "xx" }), "language"),
        (json!({ "language": "EN" }), "language"),
        (json!({ "language": "yue" }), "language"),
        (json!({ "language": 5 }), "language"),
        (json!({ "x": 1 }), "x"),
    ] {
        let failure = refusal(None, &params);
        assert!(failure.contains(&format!("params.{key}")), "{failure}");
    }
}
