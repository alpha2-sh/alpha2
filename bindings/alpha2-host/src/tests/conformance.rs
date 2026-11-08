//! The `extract` cases of `tests/cases/surface-conformance.json`, served by the host on the CPU
//! renderer: a Rust client runs the digest fixture, the packed plugins run in the host, and each
//! case's data hashes as pinned. A source digest case also runs in process and hashes alike.
//! `ALPHA2_UPDATE_GOLDENS=1` rewrites the pinned hashes.

use std::fmt::Write as _;
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::requests::{SERVE_CHILD, ServeChild};
use super::*;
use alpha2_engine::extract::fixtures::{digest_values, fnv1a};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn table_path() -> PathBuf {
    root().join("tests/cases/surface-conformance.json")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The table's `data_hash` of data sets given as JSON.
fn data_hash(sets: &Value) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let bits = |value: &Value| {
        value.as_f64().map_or_else(
            || "-".to_owned(),
            |value| format!("{:08x}", (value as f32).to_bits()),
        )
    };
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let lines = sets
        .as_array()
        .expect("data sets")
        .iter()
        .map(|set| {
            let kind = set["kind"].as_str().expect("kind");
            let items = match kind {
                "intervals" => set["intervals"]
                    .as_array()
                    .expect("intervals")
                    .iter()
                    .map(|item| {
                        let score = if item["score"].is_null() {
                            String::new()
                        } else {
                            bits(&item["score"])
                        };
                        format!(
                            "{}:{}:{}:{score}",
                            item["start"],
                            item["end"],
                            text(&item["label"])
                        )
                    })
                    .collect::<Vec<_>>(),
                "words" => set["words"]
                    .as_array()
                    .expect("words")
                    .iter()
                    .map(|item| {
                        format!(
                            "{}:{}:{}:{}",
                            item["start"],
                            item["end"],
                            text(&item["text"]),
                            text(&item["speaker"])
                        )
                    })
                    .collect(),
                _ => {
                    let times = set["times"].as_array().expect("times");
                    let values = set["values"].as_array().expect("values");
                    let width = values.len() / times.len().max(1);
                    times
                        .iter()
                        .enumerate()
                        .map(|(index, time)| {
                            let row = values[index * width..(index + 1) * width]
                                .iter()
                                .map(bits)
                                .collect::<Vec<_>>()
                                .join(",");
                            format!("{time}:{row}")
                        })
                        .collect()
                }
            };
            format!("{} {kind}|{}", text(&set["id"]), items.join(";"))
        })
        .collect::<Vec<_>>();
    hex(&Sha256::digest(lines.join("\n").as_bytes()))
}

/// Packs `plugins/<name>` into `dir` and returns its manifest's path and SHA-256.
fn packed(dir: &Path, name: &str) -> (PathBuf, String) {
    let plugins = root().join("plugins");
    let out = dir.join(name);
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["run", "--release", "--manifest-path"])
        .arg(plugins.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(plugins.join("target"))
        .args(["-p", "alpha2-plugin-pack", "--"])
        .arg(plugins.join(name))
        .arg("--out")
        .arg(&out)
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("cargo runs");
    assert!(status.success(), "plugins/{name} packs");
    let manifest = out.join("manifest.json");
    let sha256 = hex(&Sha256::digest(std::fs::read(&manifest).expect("manifest")));
    (manifest, sha256)
}

fn assets(case: &Value) -> Value {
    let paths = case["assets"]
        .as_object()
        .expect("assets")
        .iter()
        .map(|(id, path)| (id.clone(), json!(root().join(path.as_str().expect("path")))))
        .collect::<serde_json::Map<_, _>>();
    Value::Object(paths)
}

/// Answers each step of `id` as the digest fixture does, until the response.
fn digest_client(serving: &mut ServeChild, id: &str) -> Value {
    loop {
        let (message, attachments) = serving.message_with_attachments(Duration::from_secs(300));
        if message["requestId"] != id || message["type"] == "progress" {
            continue;
        }
        if message["type"] != "extract.step" {
            return message;
        }
        let emits = if message["step"] == "video" {
            let [hi, lo] = digest_values(fnv1a(attachments.iter().copied()));
            json!([{ "output": "digest", "time": message["time"], "values": [hi, lo] }])
        } else {
            json!([])
        };
        serving.write(
            &json!({ "protocolVersion": 2, "operation": "extract.ack", "requestId": id,
                                "seq": message["seq"], "emits": emits })
            .to_string(),
        );
    }
}

/// The source digest run in process over the host-core reader.
fn in_process(table: &Value, case: &Value) -> Value {
    let project = Project::from_json(
        &table["projects"][case["project"].as_str().expect("project")].to_string(),
    )
    .expect("project");
    let request: alpha2_engine::extract::ExtractRequest =
        serde_json::from_value(case["request"].clone()).expect("request");
    let descriptor: alpha2_engine::extract::Descriptor =
        serde_json::from_value(case["extractor"]["client"].clone()).expect("descriptor");
    let mut extractor = alpha2_engine::extract::fixtures::Digest::new(descriptor.input);
    let renderer = alpha2_render::CpuRenderer::new(1);
    let path = assets(case)["v"].as_str().expect("path").to_owned();
    let decoder = alpha2_native::NativeDecoder::open(path).expect("decoder");
    let reader = alpha2_host_core::extract::VideoReader::new(
        decoder,
        SourceId::new("v").expect("id"),
        alpha2_media::CancelToken::default(),
    );
    let mut supplier = alpha2_host_core::extract::ReaderSupplier::<
        _,
        alpha2_native::NativeAudioDecoder,
    >::new(&renderer, Some(reader), None);
    let sets =
        alpha2_host_core::extract::run(&project, request, &mut extractor, &mut supplier, None)
            .expect("run");
    serde_json::to_value(sets).expect("json")
}

#[test]
fn every_extract_case_hashes_as_pinned_on_every_path() {
    let _gpu = gpu_test_lock();
    let dir = scratch_directory("conformance");
    let mut table: Value =
        serde_json::from_slice(&std::fs::read(table_path()).expect("the table")).expect("json");
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[
            ("ALPHA2_RENDERER", "cpu"),
            (
                "ALPHA2_PLUGIN_CACHE",
                dir.join("cache").to_str().expect("utf-8"),
            ),
        ],
    );
    let mut plugins = HashMap::new();
    let mut stale = Vec::new();
    let snapshot = table.clone();
    for case in table["extract"].as_array_mut().expect("extract cases") {
        let name = case["name"].as_str().expect("name").to_owned();
        let mut line = json!({ "protocolVersion": 2, "requestId": name, "operation": "extract",
                               "project": snapshot["projects"][case["project"].as_str().expect("project")],
                               "assets": assets(case), "request": case["request"] });
        if let Some(plugin) = case["extractor"]["plugin"].as_str() {
            let (manifest, sha256) = plugins
                .entry(plugin.to_owned())
                .or_insert_with(|| packed(&dir, plugin))
                .clone();
            line["plugin"] = json!({ "manifest": manifest, "sha256": sha256 });
            serving.write(&line.to_string());
        } else {
            line["descriptor"] = case["extractor"]["client"].clone();
            serving.write(&line.to_string());
        }
        let answer = digest_client(&mut serving, &name);
        assert_eq!(answer["ok"], true, "{name}: {answer}");
        let found = data_hash(&answer["result"]["dataSets"]);
        if case["request"]["input"]["from"] == "source" && case["extractor"]["client"].is_object() {
            assert_eq!(
                data_hash(&in_process(&snapshot, case)),
                found,
                "{name} in process"
            );
        }
        if case["sha256"] != found.as_str() {
            stale.push(name);
            case["sha256"] = json!(found);
        }
    }
    assert_eq!(serving.finish(), Some(0));
    if env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let mut text = serde_json::to_string_pretty(&table).expect("json");
        text.push('\n');
        std::fs::write(table_path(), text).expect("write the table");
        return;
    }
    assert!(
        stale.is_empty(),
        "stale extract cases {stale:?}; rerun with ALPHA2_UPDATE_GOLDENS=1"
    );
}
