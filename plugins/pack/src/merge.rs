//! `merge`: one package from packs of the same plugin made on several machines.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::{manifest_text, read, sha256};

const MANIFEST: &str = "manifest.json";
const NOTICES: &str = "THIRD_PARTY_NOTICES.txt";
const LICENCES: &str = "licenses";
const WASM_TARGET: &str = "wasm32";
const SHARED_FIELDS: [&str; 6] = ["id", "version", "abi", "kind", "resources", "about"];

struct Pack {
    dir: PathBuf,
    manifest: Value,
    descriptor: String,
}

impl Pack {
    fn read(dir: &Path) -> Result<Self, String> {
        let text = read(&dir.join(MANIFEST))?;
        let at = |error: serde_json::Error| format!("{}: {error}", dir.join(MANIFEST).display());
        let manifest: Value = serde_json::from_slice(&text).map_err(at)?;
        let fields: BTreeMap<String, Box<RawValue>> = serde_json::from_slice(&text).map_err(at)?;
        let descriptor = fields
            .get("descriptor")
            .ok_or_else(|| format!("{}: the manifest has no descriptor", dir.display()))?
            .get()
            .to_owned();
        let pack = Self {
            dir: dir.to_owned(),
            manifest,
            descriptor,
        };
        for entry in pack.entries("artifacts").chain(pack.entries("resources")) {
            pack.check(entry)?;
        }
        Ok(pack)
    }

    fn entries(&self, list: &str) -> impl Iterator<Item = &Value> {
        self.manifest[list].as_array().into_iter().flatten()
    }

    fn natives(&self) -> impl Iterator<Item = &Value> {
        self.entries("artifacts")
            .filter(|artifact| artifact["target"] != WASM_TARGET)
    }

    fn check(&self, entry: &Value) -> Result<Option<Vec<u8>>, String> {
        if entry.get("supplied").is_some() {
            return Ok(None);
        }
        let url = entry["url"]
            .as_str()
            .ok_or_else(|| format!("{}: an entry has no url", self.dir.display()))?;
        if url.starts_with("https:") {
            return Ok(None);
        }
        let bytes = read(&self.dir.join(url))?;
        if u64::try_from(bytes.len()).ok() != entry["bytes"].as_u64()
            || Some(sha256(&bytes).as_str()) != entry["sha256"].as_str()
        {
            return Err(format!(
                "{}: {url} differs from what its manifest pins",
                self.dir.display()
            ));
        }
        Ok(Some(bytes))
    }
}

fn files(dir: &Path, name: &str) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut found = BTreeMap::new();
    let mut pending = vec![name.to_owned()];
    while let Some(relative) = pending.pop() {
        let path = dir.join(&relative);
        if path.is_dir() {
            for entry in
                std::fs::read_dir(&path).map_err(|error| format!("{}: {error}", path.display()))?
            {
                let entry = entry.map_err(|error| error.to_string())?;
                let child = entry.file_name();
                let child = child.to_str().ok_or("a licence file name is not UTF-8")?;
                pending.push(format!("{relative}/{child}"));
            }
        } else if path.is_file() {
            found.insert(relative, read(&path)?);
        }
    }
    Ok(found)
}

fn write(out: &Path, relative: &str, bytes: &[u8]) -> Result<(), String> {
    let path = out.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    std::fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

/// Writes the first pack to `out` with the native artifacts of the others added; returns the
/// manifest's path.
pub fn merge(inputs: &[PathBuf], out: &Path) -> Result<PathBuf, String> {
    let Some((first, others)) = inputs
        .split_first()
        .filter(|(_, others)| !others.is_empty())
    else {
        return Err("merge takes at least two pack directories".to_owned());
    };
    if out.exists()
        && std::fs::read_dir(out)
            .map_err(|error| format!("{}: {error}", out.display()))?
            .next()
            .is_some()
    {
        return Err(format!("{} is not empty", out.display()));
    }
    let base = Pack::read(first)?;
    let mut licences = files(&base.dir, LICENCES)?;
    licences.extend(files(&base.dir, NOTICES)?);

    let mut artifacts: Vec<Value> = base.entries("artifacts").cloned().collect();
    let mut added = Vec::new();
    for dir in others {
        let pack = Pack::read(dir)?;
        let differs = |what: &str| {
            format!(
                "{}: {what} differs from {}",
                pack.dir.display(),
                base.dir.display()
            )
        };
        for field in SHARED_FIELDS {
            if pack.manifest[field] != base.manifest[field] {
                return Err(differs(field));
            }
        }
        if pack.descriptor != base.descriptor {
            return Err(differs("descriptor"));
        }
        let mut theirs = files(&pack.dir, LICENCES)?;
        theirs.extend(files(&pack.dir, NOTICES)?);
        if let Some(name) = licences
            .keys()
            .chain(theirs.keys())
            .find(|name| licences.get(*name) != theirs.get(*name))
        {
            return Err(differs(name));
        }
        for artifact in pack.natives() {
            let target = artifact["target"].as_str().unwrap_or_default();
            if artifacts.iter().any(|known| known["target"] == target) {
                return Err(format!(
                    "{}: another pack has a {target} artifact already",
                    pack.dir.display()
                ));
            }
            if artifacts
                .iter()
                .any(|known| known["url"] == artifact["url"])
            {
                return Err(format!(
                    "{}: another pack has a file at {} already",
                    pack.dir.display(),
                    artifact["url"]
                ));
            }
            let bytes = pack.check(artifact)?.ok_or_else(|| {
                format!(
                    "{}: a native artifact is hosted elsewhere",
                    pack.dir.display()
                )
            })?;
            artifacts.push(artifact.clone());
            added.push((
                artifact["url"].as_str().unwrap_or_default().to_owned(),
                bytes,
            ));
        }
    }

    for entry in base.entries("artifacts").chain(base.entries("resources")) {
        if let Some(bytes) = base.check(entry)? {
            write(out, entry["url"].as_str().unwrap_or_default(), &bytes)?;
        }
    }
    for (url, bytes) in &added {
        write(out, url, bytes)?;
    }
    for (name, bytes) in &licences {
        write(out, name, bytes)?;
    }
    artifacts.sort_by_key(|artifact| {
        let target = artifact["target"].as_str().unwrap_or_default().to_owned();
        (target != WASM_TARGET, target)
    });
    let mut manifest = base.manifest;
    manifest["artifacts"] = json!(artifacts);
    let text = manifest_text(manifest, &base.descriptor)?;
    write(out, MANIFEST, text.as_bytes())?;
    Ok(out.join(MANIFEST))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serialized again, this number would read back one step lower.
    const DESCRIPTOR: &str = r#"{ "id": "alpha2.test", "version": 1, "params_schema": { "minimum": -99.999990023795055 } }"#;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("alpha2-pack-merge-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(dir: &Path, url: &str, bytes: &[u8]) -> Value {
        write(dir, url, bytes).expect("write");
        json!({ "url": url, "sha256": sha256(bytes), "bytes": bytes.len() })
    }

    fn pack(dir: &Path, target: &str, wasm: &[u8], version: &str) -> PathBuf {
        let mut module = entry(dir, "plugin.wasm", wasm);
        module["target"] = json!(WASM_TARGET);
        module["type"] = json!("wasm");
        native_pack(dir, target, version, Some(module))
    }

    fn native_pack(dir: &Path, target: &str, version: &str, module: Option<Value>) -> PathBuf {
        let mut native = entry(dir, &format!("{target}/plugin.so"), target.as_bytes());
        native["target"] = json!(target);
        native["type"] = json!("cdylib");
        let mut resource = entry(dir, "resources/model.onnx", b"model");
        resource["name"] = json!("model.onnx");
        write(dir, "licenses/model/LICENSE", b"model licence").expect("licence");
        write(dir, NOTICES, b"notices").expect("notices");
        let manifest = json!({
            "id": "alpha2.test",
            "version": version,
            "abi": "alpha2-plugin/1",
            "kind": "extractor",
            "artifacts": module.into_iter().chain([native]).collect::<Vec<_>>(),
            "resources": [resource, { "name": "weights", "supplied": { "shape": "file" } }],
            "about": { "name": "Test" },
        });
        let text = manifest_text(manifest, DESCRIPTOR).expect("manifest");
        write(dir, MANIFEST, text.as_bytes()).expect("manifest");
        dir.to_owned()
    }

    fn three_packs(scratch: &Scratch) -> Vec<PathBuf> {
        vec![
            pack(
                &scratch.0.join("lin"),
                "x86_64-unknown-linux-gnu",
                b"wasm lin",
                "1.0.0",
            ),
            pack(
                &scratch.0.join("win"),
                "x86_64-pc-windows-msvc",
                b"wasm win",
                "1.0.0",
            ),
            pack(
                &scratch.0.join("mac"),
                "aarch64-apple-darwin",
                b"wasm mac",
                "1.0.0",
            ),
        ]
    }

    #[test]
    fn the_merged_pack_has_every_native_target_and_the_first_packs_wasm() {
        let scratch = Scratch::new("all");
        let inputs = three_packs(&scratch);
        let out = scratch.0.join("out");
        let path = merge(&inputs, &out).expect("merge");
        let text = std::fs::read_to_string(&path).expect("manifest");
        assert!(text.contains("-99.999990023795055"), "{text}");
        let merged = Pack::read(&out).expect("the merged pack checks");
        let targets: Vec<&str> = merged
            .entries("artifacts")
            .map(|artifact| artifact["target"].as_str().expect("target"))
            .collect();
        assert_eq!(
            targets,
            [
                WASM_TARGET,
                "aarch64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "x86_64-unknown-linux-gnu"
            ]
        );
        assert_eq!(read(&out.join("plugin.wasm")).expect("wasm"), b"wasm lin");
        assert_eq!(
            merged.manifest["resources"],
            Pack::read(&inputs[0]).expect("lin").manifest["resources"]
        );
        assert_eq!(
            read(&out.join("licenses/model/LICENSE")).expect("licence"),
            b"model licence"
        );
        assert_eq!(read(&out.join(NOTICES)).expect("notices"), b"notices");
    }

    #[test]
    fn packs_without_a_wasm_artifact_merge_into_one_without_it() {
        let scratch = Scratch::new("native-only");
        let inputs = vec![
            native_pack(
                &scratch.0.join("lin"),
                "x86_64-unknown-linux-gnu",
                "1.0.0",
                None,
            ),
            native_pack(
                &scratch.0.join("mac"),
                "aarch64-apple-darwin",
                "1.0.0",
                None,
            ),
        ];
        let out = scratch.0.join("out");
        merge(&inputs, &out).expect("merge");
        let merged = Pack::read(&out).expect("the merged pack checks");
        let targets: Vec<&str> = merged
            .entries("artifacts")
            .map(|artifact| artifact["target"].as_str().expect("target"))
            .collect();
        assert_eq!(
            targets,
            ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]
        );
        assert!(!out.join("plugin.wasm").exists());
        assert_eq!(
            read(&out.join("aarch64-apple-darwin/plugin.so")).expect("native"),
            b"aarch64-apple-darwin"
        );
    }

    #[test]
    fn packs_of_another_version_are_refused() {
        let scratch = Scratch::new("version");
        let inputs = vec![
            pack(
                &scratch.0.join("lin"),
                "x86_64-unknown-linux-gnu",
                b"wasm",
                "1.0.0",
            ),
            pack(
                &scratch.0.join("win"),
                "x86_64-pc-windows-msvc",
                b"wasm",
                "1.0.1",
            ),
        ];
        let error = merge(&inputs, &scratch.0.join("out")).expect_err("refused");
        assert!(error.contains("version differs"), "{error}");
    }

    #[test]
    fn a_target_two_packs_have_is_refused() {
        let scratch = Scratch::new("twice");
        let inputs = vec![
            pack(
                &scratch.0.join("a"),
                "x86_64-unknown-linux-gnu",
                b"wasm",
                "1.0.0",
            ),
            pack(
                &scratch.0.join("b"),
                "x86_64-unknown-linux-gnu",
                b"wasm",
                "1.0.0",
            ),
        ];
        let error = merge(&inputs, &scratch.0.join("out")).expect_err("refused");
        assert!(
            error.contains("x86_64-unknown-linux-gnu artifact already"),
            "{error}"
        );
    }

    #[test]
    fn a_file_that_differs_from_its_manifest_is_refused() {
        let scratch = Scratch::new("tampered");
        let inputs = three_packs(&scratch);
        write(&inputs[1], "x86_64-pc-windows-msvc/plugin.so", b"other").expect("tamper");
        let error = merge(&inputs, &scratch.0.join("out")).expect_err("refused");
        assert!(
            error.contains("differs from what its manifest pins"),
            "{error}"
        );
    }

    #[test]
    fn other_licences_or_notices_are_refused() {
        let scratch = Scratch::new("licences");
        let inputs = three_packs(&scratch);
        write(&inputs[2], NOTICES, b"other notices").expect("notices");
        let error = merge(&inputs, &scratch.0.join("out")).expect_err("refused");
        assert!(error.contains("THIRD_PARTY_NOTICES.txt differs"), "{error}");
        let inputs = three_packs(&scratch);
        std::fs::remove_file(inputs[1].join("licenses/model/LICENSE")).expect("remove");
        let error = merge(&inputs, &scratch.0.join("out")).expect_err("refused");
        assert!(error.contains("licenses/model/LICENSE differs"), "{error}");
    }

    #[test]
    fn a_descriptor_written_otherwise_is_refused() {
        let scratch = Scratch::new("descriptor");
        let inputs = three_packs(&scratch);
        let path = inputs[1].join(MANIFEST);
        let text = std::fs::read_to_string(&path).expect("manifest");
        std::fs::write(
            &path,
            text.replace("-99.999990023795055", "-99.99999002379505"),
        )
        .expect("write");
        let error = merge(&inputs, &scratch.0.join("out")).expect_err("refused");
        assert!(error.contains("descriptor differs"), "{error}");
    }

    #[test]
    fn a_directory_with_files_is_not_written_into() {
        let scratch = Scratch::new("out");
        let inputs = three_packs(&scratch);
        let out = scratch.0.join("out");
        write(&out, "stale", b"stale").expect("stale");
        let error = merge(&inputs, &out).expect_err("refused");
        assert!(error.contains("is not empty"), "{error}");
    }
}
