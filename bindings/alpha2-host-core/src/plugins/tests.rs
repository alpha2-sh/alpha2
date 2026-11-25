use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};

use alpha2_engine::extract::fixtures::{Digest, Scripted, digest_descriptor, video_input};
use alpha2_engine::extract::{
    Direction, Emit, ExtractRequest, ExtractSession, FrameFrom, Need, OutputSpec, SuppliedAudio,
    SuppliedFrame, Supplier, drive,
};
use alpha2_model::{DataInterval, DataSet, DataWord, Project};
use serde_json::{Value, json};

use super::*;

/// The manifest hash test loads record.
pub(super) const MANIFEST_SHA256: &str =
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// The fixture plugins built for wasm32 and this target, once per test process.
pub(super) struct Built {
    /// The directory test manifests lie in; their URLs are relative to it.
    pub(super) dir: PathBuf,
    pub(super) wasm: [PathBuf; 3],
    pub(super) native: [PathBuf; 3],
}

pub(super) fn built() -> &'static Built {
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
                    "-p",
                    "alpha2-plugin-fixture-supplied",
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
            "alpha2_plugin_fixture_supplied",
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
            dir: target_dir.clone(),
            wasm: names
                .map(|name| target_dir.join(format!("wasm32-unknown-unknown/release/{name}.wasm"))),
            native: names.map(native),
        }
    })
}

/// Fetches files under the directory test manifests lie in.
pub(super) struct Local;

impl Fetch for Local {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<Fetched, PluginError> {
        Ok(Fetched {
            path: built().dir.join(url),
            local: true,
        })
    }
}

/// A manifest entry for `path`, which lies under `built().dir`.
fn entry(path: &Path) -> Value {
    let bytes = std::fs::read(path).expect("artifact");
    let url = path
        .strip_prefix(&built().dir)
        .expect("under the manifest directory")
        .to_string_lossy()
        .replace('\\', "/");
    json!({ "url": url, "sha256": sha256_hex(&bytes), "bytes": bytes.len() })
}

/// A fresh directory under `built().dir` for files a test manifest names.
pub(super) fn packed(name: &'static str) -> PathBuf {
    static PACKED: Mutex<BTreeMap<&str, PathBuf>> = Mutex::new(BTreeMap::new());
    let mut packed = PACKED.lock().unwrap_or_else(PoisonError::into_inner);
    packed
        .entry(name)
        .or_insert_with(|| {
            let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
            let out = plugins.join(format!("target/tmp/{name}-packed"));
            let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(["run", "--manifest-path"])
                .arg(plugins.join("Cargo.toml"))
                .arg("--target-dir")
                .arg(plugins.join("target"))
                .args(["-p", "alpha2-plugin-pack", "--"])
                .arg(plugins.join("first-party").join(name))
                .arg("--out")
                .arg(&out)
                .env_remove("CARGO_TARGET_DIR")
                .status()
                .expect("cargo runs");
            assert!(status.success(), "plugins/first-party/{name} packs");
            out
        })
        .clone()
}

pub(super) struct Packed(pub(super) &'static str);

impl Fetch for Packed {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<Fetched, PluginError> {
        Ok(Fetched {
            path: packed(self.0).join(url),
            local: true,
        })
    }
}

pub(super) fn scratch(name: &str) -> PathBuf {
    let dir = built()
        .dir
        .join("tmp")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

pub(super) fn descriptor_of(plugin: usize) -> Value {
    let file = ["digest", "scripted", "supplied"][plugin];
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../plugins/fixtures/{file}/descriptor.json"));
    serde_json::from_slice(&std::fs::read(path).expect("descriptor")).expect("json")
}

/// A manifest for fixture `plugin` (0 digest, 1 scripted, 2 supplied) offering wasm, native or
/// both; the supplied fixture's declares its slots.
pub(super) fn manifest(plugin: usize, wasm: bool, native: bool) -> Value {
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
    let mut manifest = json!({ "id": descriptor["id"], "version": "1.2.3", "abi": "alpha2-plugin/1",
                               "kind": "extractor", "artifacts": artifacts, "descriptor": descriptor });
    if plugin == 2 {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/fixtures/supplied/resources.json");
        manifest["resources"] =
            serde_json::from_slice(&std::fs::read(path).expect("resources")).expect("json");
    }
    manifest
}

pub(super) fn loaded(
    manifest: &Value,
    params: &Value,
    policy: &Policy,
) -> Result<PluginExtractor, PluginError> {
    let manifest = parse_manifest(&manifest.to_string())?;
    load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &params.as_object().cloned().unwrap_or_default(),
            supplied: &BTreeMap::new(),
        },
        policy,
        &mut Local,
        &CancelToken::default(),
    )
}

/// Loads `manifest` filling `supplied` under `policy`.
pub(super) fn loaded_with(
    manifest: &Value,
    params: &Value,
    supplied: &BTreeMap<String, PathBuf>,
    policy: &Policy,
) -> Result<PluginExtractor, PluginError> {
    let manifest = parse_manifest(&manifest.to_string())?;
    load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: params.as_object().expect("params"),
            supplied,
        },
        policy,
        &mut Local,
        &CancelToken::default(),
    )
}

pub(super) fn native_allowed() -> Policy {
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

pub(super) fn run(
    extractor: &mut dyn Extractor,
    params: &Value,
) -> Result<Vec<DataSet>, alpha2_engine::extract::DriveError> {
    let descriptor = extractor.describe().clone();
    let session = ExtractSession::new(&project(), &descriptor, request(&descriptor, params))
        .expect("session");
    drive(session, extractor, &mut Frames)
}

pub(super) fn script() -> Value {
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
        assert_eq!(plugin.made_by().manifest_sha256, MANIFEST_SHA256);
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
    let longer = loaded(&short, &json!({}), &Policy::default()).expect_err("refused");
    assert_eq!(longer.code, codes::HASH_MISMATCH);
    assert!(
        longer
            .message
            .ends_with("is longer than the 3 bytes the manifest pins"),
        "{longer}"
    );
    let mut resourced = manifest(1, true, false);
    let mut resource = entry(&built().wasm[0]);
    resource["name"] = json!("r");
    resource["sha256"] = json!("1".repeat(64));
    resource["bytes"] = json!(1);
    resourced["resources"] = json!([resource]);
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
    let dir = scratch("resource");
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
    assert_eq!(PLUGIN_CODES.len(), 12);
    let digest: Descriptor = serde_json::from_value(descriptor_of(0)).expect("descriptor");
    assert_eq!(
        digest,
        digest_descriptor(video_input(VideoFormat::Rgba8, 960))
    );
}

#[test]
fn a_manifest_naming_a_file_outside_its_directory_is_refused() {
    let file = scratch("outside").join("secret.txt");
    std::fs::write(&file, "secret").expect("file");
    let absolute = file.to_string_lossy().into_owned();
    for url in [
        format!("file://{absolute}"),
        absolute,
        "../secret.txt".to_owned(),
    ] {
        let mut named = manifest(1, true, false);
        named["resources"] =
            json!([{ "name": "r", "url": url, "sha256": sha256_hex(b"secret"), "bytes": 6 }]);
        let refused = parse_manifest(&named.to_string()).expect_err("refused");
        assert_eq!(refused.code, codes::LOAD_FAILED, "{url}: {refused}");
    }
    std::fs::remove_dir_all(file.parent().expect("dir")).expect("cleanup");
}

#[test]
fn a_mismatch_describes_a_downloaded_file_by_name_alone() {
    let pinned = sha256_hex(b"pinned");
    for bytes in [&b"pinnex"[..], b"pinned!"] {
        let remote =
            verify(bytes, &pinned, 6, "https://example.com/a.wasm", false).expect_err("mismatch");
        assert_eq!(remote.code, codes::HASH_MISMATCH);
        assert_eq!(
            remote.message,
            "https://example.com/a.wasm does not match the length and SHA-256 the manifest pins"
        );
        let local = verify(bytes, &pinned, 6, "a.wasm", true).expect_err("mismatch");
        assert_eq!(local.code, codes::HASH_MISMATCH);
        assert!(
            local.message.contains(&sha256_hex(bytes)) || local.message.contains("is 7 bytes"),
            "{}",
            local.message
        );
    }
    assert!(verify(b"pinned", &pinned, 6, "a.wasm", false).is_ok());
}

/// A wasm section: its id, then its body's length and the body.
fn section(id: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![id];
    leb128(body.len() as u64, &mut out);
    out.extend_from_slice(body);
    out
}

fn leb128(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = u8::try_from(value & 0x7f).expect("seven bits");
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A module of `sections` after the header.
fn module(sections: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    for section in sections {
        out.extend_from_slice(section);
    }
    out
}

/// The whole wasm ABI with one memory of one page, each function a constant result, except
/// `alpha2_pass` and `alpha2_end`, whose bodies are given as instructions.
fn abi_module(pass: &[u8], end: &[u8]) -> Vec<u8> {
    const I32: u8 = 0x7f;
    const I64: u8 = 0x7e;
    let signatures: [(&[u8], &[u8]); 6] = [
        (&[], &[I32]),
        (&[I32, I32], &[I32]),
        (&[I32, I32, I32], &[]),
        (&[], &[I64]),
        (&[I64, I32, I32, I32, I32, I32], &[I32]),
        (&[I64, I64, I32, I32, I32, I32, I32], &[I32]),
    ];
    let mut types = vec![u8::try_from(signatures.len()).expect("few")];
    for (params, results) in signatures {
        types.push(0x60);
        types.push(u8::try_from(params.len()).expect("few"));
        types.extend_from_slice(params);
        types.push(u8::try_from(results.len()).expect("few"));
        types.extend_from_slice(results);
    }
    // (name, type, body): `i32.const 1`, `i32.const 1024`, nothing, `i64.const 0`, `i32.const 0`.
    let functions: [(&str, u8, &[u8]); 9] = [
        ("alpha2_abi", 0, &[0x41, 0x01]),
        ("alpha2_alloc", 1, &[0x41, 0x80, 0x08]),
        ("alpha2_free", 2, &[]),
        ("alpha2_describe", 3, &[0x42, 0x00]),
        ("alpha2_begin", 1, &[0x41, 0x00]),
        ("alpha2_pass", 1, pass),
        ("alpha2_video", 4, &[0x41, 0x00]),
        ("alpha2_audio", 5, &[0x41, 0x00]),
        ("alpha2_end", 0, end),
    ];
    let mut declared = vec![9];
    let mut exports = vec![10, 6];
    exports.extend_from_slice(b"memory");
    exports.extend_from_slice(&[0x02, 0x00]);
    let mut code = vec![9];
    for (index, (name, ty, body)) in functions.iter().enumerate() {
        declared.push(*ty);
        exports.push(u8::try_from(name.len()).expect("short"));
        exports.extend_from_slice(name.as_bytes());
        exports.extend_from_slice(&[0x00, u8::try_from(index).expect("few")]);
        code.push(u8::try_from(body.len() + 2).expect("short"));
        code.push(0x00);
        code.extend_from_slice(body);
        code.push(0x0b);
    }
    module(&[
        section(1, &types),
        section(3, &declared),
        section(5, &[0x01, 0x00, 0x01]),
        section(7, &exports),
        section(10, &code),
    ])
}

fn instantiated(module: &[u8], memory_bytes: u64) -> Result<wasm::WasmTransport, PluginError> {
    let policy = Policy {
        memory_bytes,
        ..Policy::default()
    };
    wasm::WasmTransport::new(
        "test",
        module,
        &sha256_hex(module),
        resources::ResourceReader::new(resources::ResourceSet::default(), CancelToken::default()),
        Vec::new(),
        &policy,
        CancelToken::default(),
    )
}

#[test]
fn a_second_memory_and_a_64_bit_memory_are_unsupported() {
    // (memory 1) (memory 1)
    let two = module(&[section(5, &[0x02, 0x00, 0x01, 0x00, 0x01])]);
    // (memory i64 1)
    let wide = module(&[section(5, &[0x01, 0x04, 0x01])]);
    for refused in [two, wide] {
        let error = wasm::check_module(&refused).expect_err("refused");
        assert_eq!(error.code, codes::UNSUPPORTED_FEATURE, "{error}");
        assert!(
            error
                .message
                .contains("more than one memory or 64-bit memory"),
            "{error}"
        );
    }
}

#[test]
fn all_tables_together_are_bounded() {
    // 64 times (table 1048576 funcref)
    let mut tables = vec![64];
    for _ in 0..64 {
        tables.extend_from_slice(&[0x70, 0x00, 0x80, 0x80, 0x40]);
    }
    let error = instantiated(&module(&[section(4, &tables)]), 64 << 20)
        .err()
        .expect("refused");
    assert_eq!(error.code, codes::LIMIT, "{error}");
    assert!(error.message.contains("table elements"), "{error}");
}

#[test]
fn a_memory_whose_minimum_is_past_the_cap_is_a_limit() {
    // (memory 1025), one page past 64 MiB.
    let error = instantiated(&module(&[section(5, &[0x01, 0x00, 0x81, 0x08])]), 64 << 20)
        .err()
        .expect("refused");
    assert_eq!(
        (error.code, error.message.as_str()),
        (
            codes::LIMIT,
            "the plugin needed more than 67108864 bytes of memory"
        )
    );
}

#[test]
fn a_trap_after_a_refused_grow_in_an_earlier_call_is_a_trap() {
    // pass: (drop (memory.grow (i32.const 2048))) (i32.const 0); end: (unreachable)
    let mut transport = instantiated(
        &abi_module(&[0x41, 0x80, 0x10, 0x40, 0x00, 0x1a, 0x41, 0x00], &[0x00]),
        64 << 20,
    )
    .expect("loads");
    assert!(transport.pass(0, 0).is_ok());
    let error = transport.end().expect_err("traps");
    assert_eq!(error.code, Some(codes::TRAPPED), "{}", error.message);
}

#[test]
fn a_cap_past_2_gib_is_taken_as_2_gib() {
    // (memory 32769), one page past 2 GiB.
    let error = instantiated(
        &module(&[section(5, &[0x01, 0x00, 0x81, 0x80, 0x02])]),
        u64::MAX,
    )
    .err()
    .expect("refused");
    assert_eq!(error.code, codes::LIMIT, "{error}");
    assert!(error.message.contains("2147483648 bytes"), "{error}");
}

#[test]
fn an_emit_flood_ends_at_the_data_limit_of_its_output() {
    let policy = Policy {
        call_deadline: Duration::from_secs(60),
        ..Policy::default()
    };
    for flood in [
        json!({ "calls": 200_001 }),
        json!({ "calls": 0, "ignore_refusal": true }),
    ] {
        let params = json!({ "flood": flood });
        let mut plugin = loaded(&manifest(1, true, false), &params, &policy).expect("plugin");
        let started = std::time::Instant::now();
        let Err(alpha2_engine::extract::DriveError::Extractor(error)) = run(&mut plugin, &params)
        else {
            panic!("{flood}: the flood is refused");
        };
        assert_eq!(
            error.code,
            Some("INVALID_EMIT"),
            "{flood}: {}",
            error.message
        );
        assert!(
            error
                .message
                .contains("more than 200000 items to output quiet"),
            "{}",
            error.message
        );
        assert!(started.elapsed() < policy.call_deadline, "{flood}");
    }
}

#[test]
fn a_batch_past_a_cap_is_refused_whole_and_so_is_every_later_one() {
    let caps = [OutputCap {
        name: "quiet".to_owned(),
        items: 2,
    }];
    let interval = |output: u16| Record::Interval {
        output,
        start: 0,
        end: 1,
        label: None,
        score: None,
    };
    let batch = |records: &[Record]| {
        let mut bytes = Vec::new();
        for record in records {
            alpha2_plugin_abi::records::encode(record, &mut bytes);
        }
        bytes
    };
    let mut collected = Collected::default();
    assert_eq!(collected.emit(&batch(&[interval(0)]), &caps), status::OK);
    assert_eq!(
        collected.emit(&batch(&[interval(0), interval(0)]), &caps),
        status::FAILED
    );
    assert_eq!(collected.records.len(), 1);
    assert_eq!(collected.emit(&batch(&[]), &caps), status::FAILED);
    let refused = collected.outcome(status::OK).expect_err("refused");
    assert_eq!(refused.code, Some("INVALID_EMIT"));
    assert!(
        refused
            .message
            .contains("more than 2 items to output quiet"),
        "{}",
        refused.message
    );
    // A new call starts with nothing counted.
    assert_eq!(
        collected.emit(&batch(&[interval(0), interval(0)]), &caps),
        status::OK
    );
    assert_eq!(collected.outcome(status::OK).expect("taken").len(), 2);
    assert_eq!(
        collected.emit(&batch(&[interval(1)]), &caps),
        status::FAILED
    );
    let long = batch(&[Record::Word {
        output: 0,
        start: 0,
        end: 1,
        text: "a".repeat(MAX_DATA_LABEL_BYTES + 1),
        speaker: None,
    }]);
    let mut fresh = Collected::default();
    assert_eq!(fresh.emit(&long, &caps), status::FAILED);
    assert!(
        fresh
            .refusal
            .as_deref()
            .is_some_and(|reason| reason.contains("longer"))
    );
}

#[test]
#[allow(unsafe_code)]
fn a_native_table_shorter_than_its_abi_or_with_a_null_entry_is_refused() {
    use alpha2_plugin_abi::{NATIVE_ENTRY, PluginV1};
    use std::ffi::c_void;
    use std::mem::{offset_of, size_of};

    // SAFETY: the fixture library's initializers are the SDK's.
    let library = unsafe { libloading::Library::new(&built().native[0]) }.expect("loads");
    // SAFETY: the SDK exports the entry with this type; it returns the static table.
    let table = unsafe {
        library
            .get::<unsafe extern "C" fn() -> *const PluginV1>(NATIVE_ENTRY)
            .expect("entry")()
    };
    // SAFETY: the fixture's table lives while the library is loaded.
    let full = unsafe { native::checked(table) }.expect("a complete table");
    let mut short = full;
    short.size -= u32::try_from(size_of::<*const c_void>()).expect("small");
    // SAFETY: a whole table whose size field states one entry less.
    let refused = unsafe { native::checked(&raw const short) }.expect_err("short");
    assert_eq!(refused.code, codes::LOAD_FAILED, "{refused}");
    assert!(refused.message.contains("bytes"), "{refused}");

    let mut slots = [std::ptr::null::<c_void>(); size_of::<PluginV1>() / size_of::<usize>()];
    // SAFETY: both are `size_of::<PluginV1>()` bytes, and the array is pointer-aligned.
    unsafe { std::ptr::copy_nonoverlapping(&raw const full, slots.as_mut_ptr().cast(), 1) };
    slots[offset_of!(PluginV1, audio) / size_of::<usize>()] = std::ptr::null();
    // SAFETY: the copy of a whole table with one entry null.
    let refused = unsafe { native::checked(slots.as_ptr().cast()) }.expect_err("null entry");
    assert_eq!(refused.code, codes::LOAD_FAILED, "{refused}");
    assert!(refused.message.contains("null entry"), "{refused}");
}

/// Fetches from `built().dir` and counts the native libraries it is asked to prepare.
#[derive(Default)]
struct Counting {
    prepared: usize,
}

impl Fetch for Counting {
    fn fetch(&mut self, url: &str, sha256: &str, bytes: u64) -> Result<Fetched, PluginError> {
        Local.fetch(url, sha256, bytes)
    }

    fn prepare_native(
        &mut self,
        verified: &Path,
        _bytes: &[u8],
        _sha256: &str,
    ) -> Result<PathBuf, PluginError> {
        self.prepared += 1;
        Ok(verified.to_path_buf())
    }
}

#[test]
fn a_native_library_is_prepared_once_and_outlives_threads_it_left_running() {
    let manifest = parse_manifest(&manifest(1, false, true).to_string()).expect("manifest");
    let mut fetch = Counting::default();
    let params = json!({ "park_thread": true }).as_object().cloned();
    let mut first = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &params.clone().unwrap_or_default(),
            supplied: &BTreeMap::new(),
        },
        &native_allowed(),
        &mut fetch,
        &CancelToken::default(),
    )
    .expect("first load");
    run(&mut first, &json!({ "park_thread": true })).expect("first run");
    drop(first);
    let after_first = fetch.prepared;
    // Another test of this process may have loaded the library first.
    assert!(after_first <= 1, "{after_first}");
    let mut second = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &params.unwrap_or_default(),
            supplied: &BTreeMap::new(),
        },
        &native_allowed(),
        &mut fetch,
        &CancelToken::default(),
    )
    .expect("second load");
    run(&mut second, &json!({})).expect("second run");
    drop(second);
    assert_eq!(fetch.prepared, after_first);
    // The parked thread keeps running the library's code.
    std::thread::sleep(std::time::Duration::from_millis(50));
}

/// Fetches every file from one path.
struct Bare(PathBuf);
impl Fetch for Bare {
    fn fetch(&mut self, _: &str, _: &str, _: u64) -> Result<Fetched, PluginError> {
        Ok(Fetched {
            path: self.0.clone(),
            local: true,
        })
    }
}

/// Loads the native digest fixture named by its bare file name, from the working directory, as a
/// child process started by the test below.
#[test]
#[ignore = "run as a child of a_native_library_named_without_a_directory_loads_the_checked_file"]
fn load_a_native_library_by_bare_name() {
    let Some(file) = std::env::var_os("ALPHA2_TEST_BARE_LIBRARY") else {
        return;
    };
    let mut entry = json!({ "target": TARGET, "type": "cdylib", "url": "lib" });
    let bytes = std::fs::read(&file).expect("library");
    entry["sha256"] = json!(sha256_hex(&bytes));
    entry["bytes"] = json!(bytes.len());
    let descriptor = descriptor_of(0);
    let manifest = json!({ "id": descriptor["id"], "version": "1.2.3", "abi": "alpha2-plugin/1",
                           "kind": "extractor", "artifacts": [entry], "descriptor": descriptor });
    let manifest = parse_manifest(&manifest.to_string()).expect("manifest");
    let mut plugin = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::new(),
        },
        &native_allowed(),
        &mut Bare(PathBuf::from(file)),
        &CancelToken::default(),
    )
    .expect("the checked library loads");
    run(&mut plugin, &json!({})).expect("run");
}

#[cfg(target_os = "linux")]
#[test]
fn a_native_library_named_without_a_directory_loads_the_checked_file() {
    let dir = scratch("bare-native");
    let decoys = scratch("bare-native-decoy");
    let name = built().native[0].file_name().expect("name").to_owned();
    std::fs::copy(&built().native[0], dir.join(&name)).expect("library");
    // A library of the same name on the search path, which a bare name would reach first.
    std::fs::copy(&built().native[1], decoys.join(&name)).expect("decoy");
    let search = std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default();
    let mut paths = vec![decoys.clone()];
    paths.extend(std::env::split_paths(&search));
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "plugins::tests::load_a_native_library_by_bare_name",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(&dir)
        .env("ALPHA2_TEST_BARE_LIBRARY", &name)
        .env(
            "LD_LIBRARY_PATH",
            std::env::join_paths(paths).expect("paths"),
        )
        .output()
        .expect("child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
    std::fs::remove_dir_all(&decoys).expect("cleanup");
}

#[test]
fn a_cancel_stops_a_wasm_call_before_its_deadline_and_the_deadline_is_wall_clock() {
    let manifest = manifest(1, true, false);
    let cancel = CancelToken::default();
    let params = json!({ "spin_at": 1 });
    let policy = Policy {
        call_deadline: std::time::Duration::from_secs(10),
        ..Policy::default()
    };
    let mut plugin = load(
        LoadRequest {
            manifest: &parse_manifest(&manifest.to_string()).expect("manifest"),
            manifest_sha256: MANIFEST_SHA256,
            params: &params.as_object().cloned().unwrap_or_default(),
            supplied: &BTreeMap::new(),
        },
        &policy,
        &mut Local,
        &cancel,
    )
    .expect("loads");
    let started = std::time::Instant::now();
    let failed = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            cancel.cancel();
        });
        run(&mut plugin, &params).expect_err("cancelled")
    });
    assert!(
        matches!(&failed, alpha2_engine::extract::DriveError::Extractor(error)
            if error.code == Some(CANCELLED)),
        "{failed:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let refused = load(
        LoadRequest {
            manifest: &parse_manifest(&manifest.to_string()).expect("manifest"),
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::new(),
        },
        &Policy::default(),
        &mut Local,
        &cancel,
    )
    .expect_err("cancelled before a file is read");
    assert_eq!(refused.code, CANCELLED);
}

/// Runs the scripted fixture as wasm and as a native library with `params`.
fn run_both(params: &Value) -> Vec<Result<Vec<DataSet>, alpha2_engine::extract::DriveError>> {
    [Policy::default(), native_allowed()]
        .iter()
        .map(|policy| {
            let manifest = manifest(1, true, true);
            let mut plugin = loaded(&manifest, params, policy).expect("loads");
            run(&mut plugin, params)
        })
        .collect()
}

/// Logs 100,000 lines through both transports, as a child of the tests below.
#[test]
#[ignore = "run as a child of the plugin log tests"]
fn log_a_hundred_thousand_lines_through_both_transports() {
    if std::env::var_os("ALPHA2_TEST_PLUGIN_LOG").is_none() {
        return;
    }
    // The parent closes or drains standard error before this ends.
    std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).expect("read standard input");
    let params = json!({ "log": { "text": "a plugin line", "lines": 100_000 } });
    for outcome in run_both(&params) {
        outcome.expect("the run ends");
    }
}

/// Runs the logging child; `keep_stderr` reads its standard error, else it is closed first.
#[cfg(unix)]
fn logging_child(keep_stderr: bool) -> (std::process::ExitStatus, String, Vec<u8>) {
    use std::io::Read as _;
    use std::process::Stdio;
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "plugins::tests::log_a_hundred_thousand_lines_through_both_transports",
            "--ignored",
            "--nocapture",
        ])
        .env("ALPHA2_TEST_PLUGIN_LOG", "1")
        // The fixture build writes nothing to standard error then.
        .env("CARGO_TERM_QUIET", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("child");
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if keep_stderr && let Some(mut stderr) = stderr {
            stderr.read_to_end(&mut bytes).expect("stderr");
        }
        bytes
    });
    if !keep_stderr {
        // The reader dropped its end; wait until it has.
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("child");
    let stderr = reader.join().expect("reader");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
    )
}

#[cfg(unix)]
#[test]
fn a_plugin_logging_to_a_closed_standard_error_neither_panics_nor_aborts() {
    let (status, stdout, _) = logging_child(false);
    assert!(
        status.success() && stdout.contains("1 passed"),
        "{status}\n{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn a_run_writes_at_most_its_log_budget_and_one_notice() {
    let (status, stdout, stderr) = logging_child(true);
    assert!(status.success(), "{status}\n{stdout}");
    let text = String::from_utf8_lossy(&stderr);
    let notices = text.matches("further lines are dropped").count();
    assert_eq!(notices, 2, "one notice per transport");
    // Two runs, each at most the budget plus its line prefixes and one notice.
    let lines = text.lines().count();
    let budget = 2 * (LOG_BUDGET_BYTES + 100) + lines * "plugin: \n".len();
    assert!(
        stderr.len() <= budget,
        "{} bytes, {lines} lines",
        stderr.len()
    );
}

#[test]
fn a_long_fail_message_arrives_cut_to_its_bound() {
    let message = format!(
        "{}\u{e9}{}",
        "x".repeat(MAX_FAIL_BYTES - 1),
        "y".repeat(1 << 20)
    );
    let params = json!({ "fail_at": 1, "fail_message": message });
    for outcome in run_both(&params) {
        match outcome {
            Err(alpha2_engine::extract::DriveError::Extractor(error)) => {
                // The two-byte character the bound splits is dropped whole.
                assert_eq!(error.message, "x".repeat(MAX_FAIL_BYTES - 1));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn bounded_text_cuts_at_a_character_boundary() {
    assert_eq!(bounded_text("ab\u{e9}".as_bytes(), 3), "ab");
    assert_eq!(bounded_text("ab\u{e9}".as_bytes(), 4), "ab\u{e9}");
    assert_eq!(bounded_text(b"a\xffb", 8), "a\u{fffd}b");
    let mut budget = LogBudget::default();
    let line = vec![b'z'; MAX_LOG_BYTES * 2];
    assert_eq!(
        budget.line(&line).map(|line| line.len()),
        Some(MAX_LOG_BYTES)
    );
}

/// The words the supplied fixture emits for `params`, and what its data records.
pub(super) fn supplied_words(
    policy: &Policy,
    supplied: &BTreeMap<String, PathBuf>,
    params: &Value,
) -> Result<(Vec<String>, MadeByPlugin), PluginError> {
    let mut plugin = loaded_with(&manifest(2, true, true), params, supplied, policy)?;
    let sets = run(&mut plugin, params).map_err(|error| PluginError {
        code: match &error {
            alpha2_engine::extract::DriveError::Extractor(error) => {
                error.code.unwrap_or("EXTRACTOR_FAILED")
            }
            _ => "INTERNAL",
        },
        message: error.to_string(),
        details: match error {
            alpha2_engine::extract::DriveError::Extractor(error) => error.details,
            _ => None,
        },
    })?;
    let words = match &sets[0].values {
        alpha2_model::DataValues::Words(words) => {
            words.iter().map(|word| word.text.clone()).collect()
        }
        other => panic!("{other:?}"),
    };
    Ok((words, plugin.made_by()))
}

pub(super) fn supplied_policy(native: bool) -> Policy {
    Policy {
        allow_native: native,
        resource_root: ResourceRoot::Any,
        ..Policy::default()
    }
}

#[test]
fn supplied_files_reach_both_transports_and_their_hashes_reach_the_data() {
    let dir = scratch("supplied-both");
    let model = dir.join("model.bin");
    std::fs::write(&model, b"\x01\x02\x03weights").expect("model");
    std::fs::create_dir_all(dir.join("extra/sub")).expect("dir");
    std::fs::write(dir.join("extra/sub/a.txt"), b"alpha").expect("file");
    std::fs::write(dir.join("extra/.hidden"), b"x").expect("file");
    let supplied = BTreeMap::from([
        ("model".to_owned(), model.clone()),
        ("extra".to_owned(), dir.join("extra")),
    ]);
    let params =
        json!({ "read": ["model", "extra/sub/a.txt", "extra/.hidden", "vocab"], "context": true });
    for native in [false, true] {
        let (words, made_by) =
            supplied_words(&supplied_policy(native), &supplied, &params).expect("runs");
        assert_eq!(
            words,
            [
                "model 10 0102037765696768",
                "extra/sub/a.txt 5 616c706861",
                "extra/.hidden none",
                "vocab none",
                r#"context {"extra":{"files":[{"bytes":5,"name":"extra/sub/a.txt"}]},"model":{"files":[{"bytes":10,"name":"model"}]}}"#,
            ],
            "native {native}"
        );
        assert_eq!(made_by.manifest_sha256, MANIFEST_SHA256);
        assert_eq!(
            made_by
                .supplied
                .iter()
                .map(|file| (file.name.as_str(), file.sha256.as_str(), file.bytes))
                .collect::<Vec<_>>(),
            [
                ("extra/sub/a.txt", sha256_hex(b"alpha").as_str(), 5),
                ("model", sha256_hex(b"\x01\x02\x03weights").as_str(), 10),
            ],
            "native {native}"
        );
    }
}

#[test]
fn a_native_plugin_gets_paths_and_a_wasm_one_none() {
    let dir = scratch("supplied-paths");
    let model = dir.join("model.bin");
    std::fs::write(&model, b"w").expect("model");
    std::fs::create_dir_all(dir.join("extra/onnx")).expect("dir");
    std::fs::write(dir.join("extra/onnx/a"), b"a").expect("file");
    std::fs::write(dir.join("extra/b"), b"b").expect("file");
    let supplied = BTreeMap::from([
        ("model".to_owned(), model.clone()),
        ("extra".to_owned(), dir.join("extra")),
    ]);
    let params = json!({ "paths": ["model", "extra/onnx", "vocab"] });
    let canonical = |path: &Path| resources::canonical(path).expect("canonical");
    let (words, made_by) =
        supplied_words(&supplied_policy(true), &supplied, &params).expect("native");
    assert_eq!(
        words,
        [
            format!("path model {}", canonical(&model).display()),
            format!(
                "path extra/onnx {}",
                canonical(&dir.join("extra")).join("onnx").display()
            ),
            "path vocab none".to_owned(),
        ]
    );
    assert_eq!(
        made_by
            .supplied
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        ["extra/onnx/a", "model"]
    );
    let (words, made_by) =
        supplied_words(&supplied_policy(false), &supplied, &params).expect("wasm");
    assert_eq!(
        words,
        ["path model none", "path extra/onnx none", "path vocab none"]
    );
    assert_eq!(made_by.supplied, Vec::new());
}

#[test]
fn slots_are_resolved_before_the_artifact_is_fetched_and_refused_with_details() {
    struct Refusing;
    impl Fetch for Refusing {
        fn fetch(&mut self, url: &str, _: &str, _: u64) -> Result<Fetched, PluginError> {
            panic!("fetched {url} before the slots were resolved")
        }
    }
    let manifest = parse_manifest(&manifest(2, true, true).to_string()).expect("manifest");
    let refused = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::new(),
        },
        &supplied_policy(false),
        &mut Refusing,
        &CancelToken::default(),
    )
    .expect_err("a required slot is empty");
    assert_eq!(refused.code, codes::RESOURCE_INVALID);
    assert_eq!(
        refused.details,
        json!({ "slot": "model", "reason": "missing" })
            .as_object()
            .cloned()
    );
    let model = scratch("supplied-refused").join("model.bin");
    std::fs::write(&model, b"w").expect("model");
    let refused = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::from([("model".to_owned(), model)]),
        },
        &Policy::default(),
        &mut Refusing,
        &CancelToken::default(),
    )
    .expect_err("the default policy takes no supplied files");
    assert_eq!(refused.code, codes::REFUSED);
}

#[test]
fn a_read_failure_ends_the_call_whatever_the_plugin_returns() {
    let dir = scratch("supplied-latched");
    let model = dir.join("model.bin");
    std::fs::write(&model, b"w").expect("model");
    std::fs::create_dir_all(dir.join("extra")).expect("dir");
    std::fs::write(dir.join("extra/big"), b"123456").expect("file");
    let mut capped = manifest(2, true, true);
    capped["resources"][1]["supplied"]["max_bytes"] = json!(4);
    let supplied = BTreeMap::from([
        ("model".to_owned(), model),
        ("extra".to_owned(), dir.join("extra")),
    ]);
    let params = json!({ "read": ["extra/big"] });
    for native in [false, true] {
        let mut plugin =
            loaded_with(&capped, &params, &supplied, &supplied_policy(native)).expect("loads");
        let error = run(&mut plugin, &params).expect_err("the read fails");
        let alpha2_engine::extract::DriveError::Extractor(error) = error else {
            panic!("{error:?}")
        };
        assert_eq!(error.code, Some(codes::RESOURCE_INVALID), "native {native}");
        assert_eq!(
            error.details,
            json!({ "slot": "extra", "reason": "too_large" })
                .as_object()
                .cloned()
        );
    }
}
