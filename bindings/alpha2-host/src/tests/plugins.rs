use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use super::requests::{SERVE_CHILD, ServeChild};
use super::*;
use alpha2_host_core::plugins::{TARGET, sha256_hex};

const VIDEO: &str = "h264-bt709-copy.mp4";

/// Builds the fixture plugins for wasm32 and this target once; returns their target directory.
fn built() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let target_dir = plugins.join("target");
        for target in ["wasm32-unknown-unknown", TARGET] {
            let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
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
        target_dir
    })
}

fn native_file(name: &str) -> String {
    if TARGET.contains("windows") {
        format!("{name}.dll")
    } else if TARGET.contains("apple") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

/// Writes fixture `name` (`digest` or `scripted`) into `dir` as a plugin directory with relative
/// URLs, offering wasm, native or both, and returns the manifest's path and SHA-256.
fn plugin_dir(dir: &Path, name: &str, wasm: bool, native: bool) -> (PathBuf, String) {
    let target_dir = built();
    let library = format!("alpha2_plugin_fixture_{name}");
    let mut artifacts = Vec::new();
    let mut place = |from: PathBuf, url: String, target: &str, kind: &str| {
        let bytes = std::fs::read(&from).expect("artifact");
        let to = dir.join(&url);
        std::fs::create_dir_all(to.parent().expect("parent")).expect("dir");
        std::fs::write(&to, &bytes).expect("copy");
        artifacts.push(json!({ "target": target, "type": kind, "url": url,
                               "sha256": sha256_hex(&bytes), "bytes": bytes.len() }));
    };
    if wasm {
        place(
            target_dir.join(format!("wasm32-unknown-unknown/release/{library}.wasm")),
            format!("{library}.wasm"),
            "wasm32",
            "wasm",
        );
    }
    if native {
        let file = native_file(&library);
        place(
            target_dir.join(TARGET).join("release").join(&file),
            format!("{TARGET}/{file}"),
            TARGET,
            "cdylib",
        );
    }
    let descriptor: Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../plugins/fixture-{name}/descriptor.json")),
        )
        .expect("descriptor"),
    )
    .expect("json");
    let manifest = json!({ "id": descriptor["id"], "version": "0.1.0", "abi": "alpha2-plugin/1", "kind": "extractor",
                           "artifacts": artifacts, "descriptor": descriptor });
    let text = serde_json::to_string_pretty(&manifest).expect("json");
    let path = dir.join("manifest.json");
    std::fs::write(&path, &text).expect("manifest");
    (path, sha256_hex(text.as_bytes()))
}

fn project() -> Value {
    json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 320, "height": 180, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
        "timeline": { "tracks": [] }
    })
}

fn extract(
    id: &str,
    extractor: &str,
    manifest: &Path,
    sha256: &str,
    plugin: &Value,
    params: &Value,
) -> String {
    let outputs = if extractor == "alpha2.test.digest" {
        json!({ "digest": "digest" })
    } else {
        json!({ "track": "track", "levels": "levels", "correction": "correction", "words": "words", "quiet": "quiet" })
    };
    let mut plugin = plugin.clone();
    plugin["manifest"] = json!(manifest);
    plugin["sha256"] = json!(sha256);
    json!({
        "protocolVersion": 2, "requestId": id, "operation": "extract",
        "project": project(),
        "assets": { "v": corpus(VIDEO) },
        "plugin": plugin,
        "request": {
            "extractor": extractor, "version": 1, "params": params,
            "input": { "from": "source", "source_id": "v", "range": { "start": 0, "end": 40_000 }, "size": 96 },
            "outputs": outputs
        }
    })
    .to_string()
}

fn answer(serving: &ServeChild, id: &str) -> Value {
    loop {
        let message = serving.message(Duration::from_secs(120));
        if message["requestId"] == id && message.get("ok").is_some() {
            return message;
        }
    }
}

#[test]
fn a_wasm_plugin_records_the_bytes_the_rust_reader_pins() {
    let _gpu = gpu_test_lock();
    let dir = scratch_directory("plugin-digest");
    let (manifest, sha256) = plugin_dir(&dir, "digest", true, true);
    let pinned: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/extract-recipe.json"),
        )
        .expect("recipe"),
    )
    .expect("json");
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[(
            "ALPHA2_PLUGIN_CACHE",
            dir.join("cache").to_str().expect("utf-8"),
        )],
    );
    for (id, plugin) in [
        ("wasm", json!({})),
        ("native", json!({ "allow_native": true })),
    ] {
        serving.write(&extract(
            id,
            "alpha2.test.digest",
            &manifest,
            &sha256,
            &plugin,
            &json!({}),
        ));
        let response = answer(&serving, id);
        assert_eq!(response["ok"], true, "{response}");
        let set = &response["result"]["dataSets"][0];
        let found = set["times"]
            .as_array()
            .expect("times")
            .iter()
            .enumerate()
            .map(|(index, time)| {
                // Each half is a 16-bit value over 65536, exact in f32.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let half = |at: usize| {
                    (set["values"][at].as_f64().expect("value") * 65_536.0).round() as u64
                };
                json!([
                    time,
                    format!("{:08x}", half(index * 2) << 16 | half(index * 2 + 1))
                ])
            })
            .collect::<Vec<_>>();
        assert_eq!(json!(found), pinned[VIDEO], "{id}");
        assert_eq!(set["made_by"]["plugin"]["version"], "0.1.0", "{id}");
        assert!(set["made_from"].is_object(), "{id}");
    }
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn plugins_the_policy_or_the_pins_refuse_end_with_their_codes() {
    let _gpu = gpu_test_lock();
    let native_only = scratch_directory("plugin-native-only");
    let (native_manifest, native_sha) = plugin_dir(&native_only, "digest", false, true);
    let both = scratch_directory("plugin-both");
    let (manifest, sha256) = plugin_dir(&both, "digest", true, true);
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    let cases = [
        (
            "refused",
            extract(
                "refused",
                "alpha2.test.digest",
                &native_manifest,
                &native_sha,
                &json!({}),
                &json!({}),
            ),
            "PLUGIN_NATIVE_REFUSED",
        ),
        (
            "forced",
            extract(
                "forced",
                "alpha2.test.digest",
                &native_manifest,
                &native_sha,
                &json!({ "allow_native": true, "force_wasm": true }),
                &json!({}),
            ),
            "PLUGIN_NO_ARTIFACT",
        ),
        (
            "pinned",
            extract(
                "pinned",
                "alpha2.test.digest",
                &manifest,
                &"0".repeat(64),
                &json!({}),
                &json!({}),
            ),
            "PLUGIN_HASH_MISMATCH",
        ),
        (
            "missing",
            extract(
                "missing",
                "alpha2.test.digest",
                &both.join("none.json"),
                &sha256,
                &json!({}),
                &json!({}),
            ),
            "PLUGIN_FETCH_FAILED",
        ),
        (
            "wrong",
            extract(
                "wrong",
                "alpha2.test.scripted",
                &manifest,
                &sha256,
                &json!({}),
                &json!({}),
            ),
            "INVALID_REQUEST",
        ),
    ];
    for (id, request, code) in cases {
        serving.write(&request);
        let response = answer(&serving, id);
        assert_eq!(response["error"]["code"], code, "{id}: {response}");
    }
    // A tampered artifact is refused even though the manifest's own hash matches.
    std::fs::write(both.join("alpha2_plugin_fixture_digest.wasm"), b"\0asm").expect("tamper");
    serving.write(&extract(
        "tampered",
        "alpha2.test.digest",
        &manifest,
        &sha256,
        &json!({}),
        &json!({}),
    ));
    assert_eq!(
        answer(&serving, "tampered")["error"]["code"],
        "PLUGIN_HASH_MISMATCH"
    );
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn a_wasm_plugin_past_its_limits_fails_and_the_host_stays_ready() {
    let _gpu = gpu_test_lock();
    let dir = scratch_directory("plugin-limits");
    let (manifest, sha256) = plugin_dir(&dir, "scripted", true, false);
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[(
            "ALPHA2_TEST_SERVE_ARGS",
            "--plugin-call-timeout 300 --plugin-memory-limit 67108864",
        )],
    );
    for (id, params, code) in [
        ("spin", json!({ "spin_at": 1 }), "PLUGIN_LIMIT"),
        ("hog", json!({ "hog_at": 1 }), "PLUGIN_LIMIT"),
        ("panic", json!({ "panic_at": 1 }), "PLUGIN_TRAPPED"),
        ("fail", json!({ "fail_at": 1 }), "EXTRACTOR_FAILED"),
        (
            "emit",
            json!({ "per_delivery": [[{ "output": "levels", "time": 0, "values": [1e30, 0] }]] }),
            "INVALID_EMIT",
        ),
    ] {
        serving.write(&extract(
            id,
            "alpha2.test.scripted",
            &manifest,
            &sha256,
            &json!({}),
            &params,
        ));
        let response = answer(&serving, id);
        assert_eq!(response["error"]["code"], code, "{id}: {response}");
        assert_eq!(response["hostState"], "ready", "{id}");
    }
    serving.write(&extract(
        "ok",
        "alpha2.test.scripted",
        &manifest,
        &sha256,
        &json!({}),
        &json!({
            "per_delivery": [[{ "output": "levels", "time": 0, "values": [1, 2] }]]
        }),
    ));
    let ok = answer(&serving, "ok");
    assert_eq!(ok["ok"], true, "{ok}");
    assert_eq!(ok["result"]["dataSets"].as_array().map(Vec::len), Some(5));
    assert_eq!(serving.finish(), Some(0));
}
