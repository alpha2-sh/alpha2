use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use alpha2_engine::extract::fixtures::{Digest, Scripted, digest_descriptor, video_input};
use alpha2_engine::extract::{
    Direction, Emit, ExtractRequest, ExtractSession, FrameFrom, Need, OutputSpec, SuppliedAudio,
    SuppliedFrame, Supplier, drive,
};
use alpha2_model::{DataInterval, DataSet, DataWord, Project};
use serde_json::{Value, json};

use super::*;

/// The fixture plugins built for wasm32 and this target, once per test process.
struct Built {
    wasm: [PathBuf; 2],
    native: [PathBuf; 2],
}

fn built() -> &'static Built {
    static BUILT: OnceLock<Built> = OnceLock::new();
    BUILT.get_or_init(|| {
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let target_dir = plugins.join("target");
        for target in ["wasm32-unknown-unknown", TARGET] {
            let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(["build", "--release", "--target", target, "--manifest-path"])
                .arg(plugins.join("Cargo.toml"))
                .arg("--target-dir")
                .arg(&target_dir)
                .args([
                    "-p",
                    "alpha2-plugin-fixture-digest",
                    "-p",
                    "alpha2-plugin-fixture-scripted",
                ])
                .env_remove("CARGO_TARGET_DIR")
                .env(
                    "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS",
                    "-Ctarget-feature=-relaxed-simd",
                )
                .status()
                .expect("cargo runs");
            assert!(status.success(), "the fixture plugins build for {target}");
        }
        let names = [
            "alpha2_plugin_fixture_digest",
            "alpha2_plugin_fixture_scripted",
        ];
        let native = |name: &str| {
            let file = if TARGET.contains("windows") {
                format!("{name}.dll")
            } else if TARGET.contains("apple") {
                format!("lib{name}.dylib")
            } else {
                format!("lib{name}.so")
            };
            target_dir.join(TARGET).join("release").join(file)
        };
        Built {
            wasm: names
                .map(|name| target_dir.join(format!("wasm32-unknown-unknown/release/{name}.wasm"))),
            native: names.map(native),
        }
    })
}

/// Fetches files by path, since a test manifest's URLs are local paths.
struct Local;

impl Fetch for Local {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<PathBuf, PluginError> {
        Ok(PathBuf::from(url))
    }
}

fn entry(path: &Path) -> Value {
    let bytes = std::fs::read(path).expect("artifact");
    json!({ "url": path.to_string_lossy(), "sha256": sha256_hex(&bytes), "bytes": bytes.len() })
}

fn descriptor_of(plugin: usize) -> Value {
    let file = ["digest", "scripted"][plugin];
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../plugins/fixtures/{file}/descriptor.json"));
    serde_json::from_slice(&std::fs::read(path).expect("descriptor")).expect("json")
}

/// A manifest for fixture `plugin` (0 digest, 1 scripted) offering wasm, native or both.
fn manifest(plugin: usize, wasm: bool, native: bool) -> Value {
    let built = built();
    let mut artifacts = Vec::new();
    if wasm {
        let mut artifact = entry(&built.wasm[plugin]);
        artifact["target"] = json!("wasm32");
        artifact["type"] = json!("wasm");
        artifacts.push(artifact);
    }
    if native {
        let mut artifact = entry(&built.native[plugin]);
        artifact["target"] = json!(TARGET);
        artifact["type"] = json!("cdylib");
        artifacts.push(artifact);
    }
    let descriptor = descriptor_of(plugin);
    json!({ "id": descriptor["id"], "version": "1.2.3", "abi": "alpha2-plugin/1", "kind": "extractor",
            "artifacts": artifacts, "descriptor": descriptor })
}

fn loaded(
    manifest: &Value,
    params: &Value,
    policy: &Policy,
) -> Result<PluginExtractor, PluginError> {
    let manifest = parse_manifest(&manifest.to_string())?;
    load(
        &manifest,
        params.as_object().cloned().unwrap_or_default(),
        policy,
        &mut Local,
    )
}

fn native_allowed() -> Policy {
    Policy {
        allow_native: true,
        ..Policy::default()
    }
}

fn project() -> Project {
    Project::from_json(
        &json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
            "timeline": { "tracks": [] }
        })
        .to_string(),
    )
    .expect("project")
}

fn request(descriptor: &Descriptor, params: &Value) -> ExtractRequest {
    serde_json::from_value(json!({
        "extractor": descriptor.id, "version": descriptor.version, "params": params,
        "input": { "from": "source", "source_id": "v", "range": { "start": 0, "end": 40_000 }, "size": 4 },
        "outputs": descriptor.outputs.iter().map(|o| (o.name.clone(), json!(format!("set-{}", o.name)))).collect::<serde_json::Map<_, _>>()
    }))
    .expect("request")
}

/// A source with a 4x2 frame every 4000 ticks whose bytes depend on the frame and position.
struct Frames;

impl Supplier for Frames {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        let pts = |index: i64| (0..20).contains(&index).then_some(index * 4000);
        let index = match need {
            Need::SourceFrame {
                from: FrameFrom::Start(time),
                ..
            } => time.ticks() / 4000,
            Need::SourceFrame {
                from: FrameFrom::After(at),
                direction: Direction::Forward,
                ..
            } => at.ticks() / 4000 + 1,
            Need::SourceFrame {
                from: FrameFrom::After(at),
                ..
            } => at.ticks() / 4000 - 1,
            _ => return Err("not a source frame".to_owned()),
        };
        Ok(pts(index).map(|pts| SuppliedFrame {
            pts: MediaTime(pts),
            width: 4,
            height: 2,
            rgba: (0..32_u8)
                .map(|byte| {
                    byte.wrapping_mul(31)
                        .wrapping_add(u8::try_from(index).unwrap_or(0))
                })
                .collect(),
        }))
    }

    fn audio(&mut self, _need: &Need) -> Result<SuppliedAudio, String> {
        Err("no audio".to_owned())
    }
}

fn run(
    extractor: &mut dyn Extractor,
    params: &Value,
) -> Result<Vec<DataSet>, alpha2_engine::extract::DriveError> {
    let descriptor = extractor.describe().clone();
    let session = ExtractSession::new(&project(), &descriptor, request(&descriptor, params))
        .expect("session");
    drive(session, extractor, &mut Frames)
}

fn script() -> Value {
    json!({
        "per_delivery": [
            [
                { "output": "track", "time": 0, "values": [0.25, 0.5] },
                { "output": "levels", "time": 0, "values": [1.5, -2] },
                { "output": "correction", "time": 0, "values": [0.01, -0.02, 0.0, 1.25] }
            ],
            [{ "output": "track", "time": 4000, "values": [null, null] }],
            [{ "output": "quiet", "start": 0, "end": 8000, "label": "silence", "score": 0.75 }]
        ],
        "at_end": [
            { "output": "words", "start": 100, "end": 200, "text": "caf\u{e9}", "speaker": "a" },
            { "output": "quiet", "start": 12_000, "end": 16_000 }
        ]
    })
}

/// The engine's `scripted` fixture with the plugin's outputs and `script()`.
fn rust_scripted() -> Scripted {
    let descriptor: Descriptor = serde_json::from_value(descriptor_of(1)).expect("descriptor");
    let outputs: Vec<OutputSpec> = descriptor.outputs;
    let sample = |output: &str, time: i64, values: Vec<Option<f32>>| Emit::Sample {
        output: output.to_owned(),
        time: MediaTime(time),
        values,
    };
    let interval = |start: i64, end: i64, label: Option<&str>, score: Option<f32>| Emit::Interval {
        output: "quiet".to_owned(),
        interval: DataInterval {
            start: MediaTime(start),
            end: MediaTime(end),
            label: label.map(str::to_owned),
            score,
        },
    };
    Scripted::new(
        video_input(VideoFormat::Luma8, 960),
        outputs,
        vec![
            vec![
                sample("track", 0, vec![Some(0.25), Some(0.5)]),
                sample("levels", 0, vec![Some(1.5), Some(-2.0)]),
                sample(
                    "correction",
                    0,
                    vec![Some(0.01), Some(-0.02), Some(0.0), Some(1.25)],
                ),
            ],
            vec![sample("track", 4000, vec![None, None])],
            vec![interval(0, 8000, Some("silence"), Some(0.75))],
        ],
        vec![
            Emit::Word {
                output: "words".to_owned(),
                word: DataWord {
                    start: MediaTime(100),
                    end: MediaTime(200),
                    text: "caf\u{e9}".to_owned(),
                    speaker: Some("a".to_owned()),
                },
            },
            interval(12_000, 16_000, None, None),
        ],
    )
}

#[test]
fn the_fixture_plugins_give_the_data_the_rust_fixtures_do() {
    let mut digest = Digest::new(video_input(VideoFormat::Rgba8, 960));
    let expected = run(&mut digest, &json!({})).expect("rust digest");
    assert_eq!(expected[0].id.as_str(), "set-digest");
    for policy in [Policy::default(), native_allowed()] {
        let mut plugin =
            loaded(&manifest(0, true, true), &json!({}), &policy).expect("digest plugin");
        assert_eq!(
            run(&mut plugin, &json!({})).expect("plugin digest"),
            expected,
            "{policy:?}"
        );
        assert_eq!(plugin.made_by().version, "1.2.3");
    }

    let expected = run(&mut rust_scripted(), &script()).expect("rust scripted");
    assert_eq!(expected.len(), 5);
    for policy in [Policy::default(), native_allowed()] {
        let mut plugin =
            loaded(&manifest(1, true, true), &script(), &policy).expect("scripted plugin");
        assert_eq!(
            run(&mut plugin, &script()).expect("plugin scripted"),
            expected,
            "{policy:?}"
        );
    }
}

#[test]
fn the_policy_picks_the_artifact() {
    let both = parse_manifest(&manifest(0, true, true).to_string()).expect("manifest");
    let kind = |policy: &Policy| select(&both, policy, TARGET).map(|artifact| artifact.kind);
    assert_eq!(kind(&Policy::default()), Ok(ArtifactKind::Wasm));
    assert_eq!(kind(&native_allowed()), Ok(ArtifactKind::Cdylib));
    assert_eq!(
        kind(&Policy {
            force_wasm: true,
            ..native_allowed()
        }),
        Ok(ArtifactKind::Wasm)
    );

    let native_only = manifest(0, false, true);
    let refused = loaded(&native_only, &json!({}), &Policy::default()).expect_err("refused");
    assert_eq!(refused.code, codes::NATIVE_REFUSED, "{refused}");
    assert!(loaded(&native_only, &json!({}), &native_allowed()).is_ok());

    let elsewhere = parse_manifest(
        &manifest(0, false, true)
            .to_string()
            .replace(TARGET, "riscv64gc-unknown-linux-gnu"),
    )
    .expect("manifest");
    let none = select(&elsewhere, &native_allowed(), TARGET).expect_err("no artifact");
    assert_eq!(none.code, codes::NO_ARTIFACT);
    assert!(
        none.message
            .contains("riscv64gc-unknown-linux-gnu (cdylib)"),
        "{none}"
    );
}

#[test]
fn a_file_that_is_not_the_pinned_one_is_refused() {
    let mut wrong = manifest(0, true, false);
    wrong["artifacts"][0]["sha256"] = json!("0".repeat(64));
    assert_eq!(
        loaded(&wrong, &json!({}), &Policy::default())
            .expect_err("refused")
            .code,
        codes::HASH_MISMATCH
    );
    let mut short = manifest(0, true, false);
    short["artifacts"][0]["bytes"] = json!(3);
    assert_eq!(
        loaded(&short, &json!({}), &Policy::default())
            .expect_err("refused")
            .code,
        codes::HASH_MISMATCH
    );
    let mut resourced = manifest(1, true, false);
    resourced["resources"] = json!([{ "name": "r", "url": built().wasm[0].to_string_lossy(), "sha256": "1".repeat(64), "bytes": 1 }]);
    assert_eq!(
        loaded(&resourced, &json!({}), &Policy::default())
            .expect_err("refused")
            .code,
        codes::HASH_MISMATCH
    );
}

#[test]
fn an_artifact_describing_another_extractor_is_refused() {
    let mut changed = manifest(0, true, true);
    changed["descriptor"]["input"]["max_size"] = json!(480);
    for policy in [Policy::default(), native_allowed()] {
        let refused = loaded(&changed, &json!({}), &policy).expect_err("refused");
        assert_eq!(refused.code, codes::DESCRIPTOR_MISMATCH, "{refused}");
    }
}

#[test]
fn resources_reach_the_plugin() {
    let dir = std::env::temp_dir().join(format!("alpha2-plugin-resource-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("greeting.txt");
    std::fs::write(&file, "hello").expect("resource");
    let mut with = manifest(1, true, true);
    let mut resource = entry(&file);
    resource["name"] = json!("greeting");
    with["resources"] = json!([resource]);
    for policy in [Policy::default(), native_allowed()] {
        let params = json!({ "resource": "greeting" });
        let mut plugin = loaded(&with, &params, &policy).expect("plugin");
        let sets = run(&mut plugin, &params).expect("run");
        let words = sets
            .iter()
            .find(|set| set.id.as_str() == "set-words")
            .expect("words");
        assert!(format!("{words:?}").contains("\"hello\""), "{words:?}");
        let mut plugin = loaded(&with, &json!({ "resource": "missing" }), &policy).expect("plugin");
        let failed = run(&mut plugin, &json!({ "resource": "missing" })).expect_err("fails");
        assert!(
            failed.to_string().contains("no resource named missing"),
            "{failed}"
        );
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

fn failure_code(params: &Value, policy: &Policy) -> (Option<&'static str>, String) {
    let mut plugin = loaded(&manifest(1, true, true), params, policy).expect("plugin");
    match run(&mut plugin, params) {
        Err(alpha2_engine::extract::DriveError::Extractor(error)) => (error.code, error.message),
        other => panic!("expected an extractor failure, got {other:?}"),
    }
}

#[test]
fn failures_traps_and_limits_are_typed() {
    let small = Policy {
        memory_bytes: 64 << 20,
        call_deadline: Duration::from_millis(300),
        ..Policy::default()
    };
    let (code, message) = failure_code(&json!({ "fail_at": 1 }), &small);
    assert_eq!(
        (code, message.as_str()),
        (None, "scripted failure at delivery 1")
    );
    assert_eq!(
        failure_code(&json!({ "panic_at": 1 }), &small).0,
        Some(codes::TRAPPED)
    );
    assert_eq!(
        failure_code(&json!({ "panic_at": 1 }), &native_allowed()).0,
        Some(codes::TRAPPED)
    );
    let (code, message) = failure_code(&json!({ "hog_at": 1 }), &small);
    assert_eq!(code, Some(codes::LIMIT), "{message}");
    assert!(message.contains("memory"), "{message}");
    let (code, message) = failure_code(&json!({ "spin_at": 1 }), &small);
    assert_eq!(code, Some(codes::LIMIT), "{message}");
    assert!(message.contains("longer than 300 ms"), "{message}");
    // An emit to an output the descriptor lacks is the plugin's failure, not a bad record.
    let (code, message) = failure_code(
        &json!({ "per_delivery": [[{ "output": "nope", "time": 0, "values": [1] }]] }),
        &small,
    );
    assert_eq!(code, None);
    assert!(message.contains("no output named nope"), "{message}");
    // A value the session refuses ends the run as an invalid emit.
    let mut plugin = loaded(&manifest(1, true, false), &json!({}), &small).expect("plugin");
    let params =
        json!({ "per_delivery": [[{ "output": "levels", "time": 0, "values": [1e30, 0] }]] });
    let mut plugin_with = loaded(&manifest(1, true, false), &params, &small).expect("plugin");
    assert!(run(&mut plugin, &json!({})).is_ok());
    assert!(matches!(
        run(&mut plugin_with, &params),
        Err(alpha2_engine::extract::DriveError::Session(_))
    ));
}

#[test]
fn modules_that_break_the_rules_are_refused() {
    const HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    // One function `() -> v128`: `v128.const 0`, `i32x4.relaxed_trunc_f32x4_s`.
    let mut relaxed = HEADER.to_vec();
    relaxed.extend_from_slice(&[
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7b, 0x03, 0x02, 0x01, 0x00,
    ]);
    relaxed.extend_from_slice(&[0x0a, 0x19, 0x01, 0x17, 0x00, 0xfd, 0x0c]);
    relaxed.extend_from_slice(&[0; 16]);
    relaxed.extend_from_slice(&[0xfd, 0x81, 0x02, 0x0b]);
    assert_eq!(
        wasm::check_module(&relaxed).expect_err("relaxed").code,
        codes::UNSUPPORTED_FEATURE
    );
    // A shared memory needs threads.
    let mut shared = HEADER.to_vec();
    shared.extend_from_slice(&[0x05, 0x04, 0x01, 0x03, 0x01, 0x01]);
    assert_eq!(
        wasm::check_module(&shared).expect_err("shared").code,
        codes::UNSUPPORTED_FEATURE
    );
    // An import from outside `alpha2`.
    let mut wasi = HEADER.to_vec();
    wasi.extend_from_slice(&[0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x02, 0x10, 0x01, 0x04]);
    wasi.extend_from_slice(b"wasi");
    wasi.extend_from_slice(&[0x07]);
    wasi.extend_from_slice(b"fd_read");
    wasi.extend_from_slice(&[0x00, 0x00]);
    assert_eq!(
        wasm::check_module(&wasi).expect_err("wasi").code,
        codes::UNSUPPORTED_FEATURE
    );
    assert_eq!(
        wasm::check_module(b"nope").expect_err("garbage").code,
        codes::LOAD_FAILED
    );
    assert!(wasm::check_module(&std::fs::read(&built().wasm[0]).expect("wasm")).is_ok());
}

#[test]
fn manifests_of_other_abis_are_refused_by_code() {
    let mut later = manifest(0, true, false);
    later["abi"] = json!("alpha2-plugin/2");
    assert_eq!(
        parse_manifest(&later.to_string()).expect_err("abi").code,
        codes::ABI_UNSUPPORTED
    );
    assert_eq!(
        parse_manifest("{}").expect_err("invalid").code,
        codes::LOAD_FAILED
    );
    assert_eq!(PLUGIN_CODES.len(), 10);
    let digest: Descriptor = serde_json::from_value(descriptor_of(0)).expect("descriptor");
    assert_eq!(
        digest,
        digest_descriptor(video_input(VideoFormat::Rgba8, 960))
    );
}
