//! The extraction cases of `tests/cases/surface-conformance.json`, as every surface checks them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The data hash `tests/cases/surface-conformance.json` pins for data sets given as JSON: SHA-256
/// of one line per set, its values as `f32` bits.
///
/// # Panics
/// Data sets that are not in the host protocol's JSON form.
#[must_use]
pub fn data_hash(sets: &Value) -> String {
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

/// Packs the plugin crate at `path` (from the repository `root`) into `dir` with the plugin
/// workspace's packer and returns its manifest's path and SHA-256.
///
/// # Panics
/// The packer does not build or run.
#[must_use]
pub fn pack_plugin(root: &Path, dir: &Path, path: &str) -> (PathBuf, String) {
    let plugins = root.join("plugins");
    let out = dir.join(path);
    let status =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["run", "--release", "--manifest-path"])
            .arg(plugins.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(plugins.join("target"))
            .args(["-p", "alpha2-plugin-pack", "--"])
            .arg(root.join(path))
            .arg("--out")
            .arg(&out)
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("cargo runs");
    assert!(status.success(), "{path} packs");
    let manifest = out.join("manifest.json");
    let sha256 = hex(&Sha256::digest(std::fs::read(&manifest).expect("manifest")));
    (manifest, sha256)
}
