//! Licence notices for a packed plugin: the plugin's own licence files under `licenses/`, and
//! `THIRD_PARTY_NOTICES.txt` for every crate linked into its artifacts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Whether `name` is a licence or notice file by its name.
fn licence_file(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["LICENSE", "LICENCE", "COPYING", "NOTICE"]
        .iter()
        .any(|prefix| upper.starts_with(prefix))
}

/// The licence and notice files directly in `directory`, sorted.
fn licence_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(licence_file)
        })
        .collect();
    files.sort();
    files
}

/// Copies the `LICENSE*` and `NOTICE*` files of the plugin crate's root and of its `model/`
/// directory to `out/licenses/` and `out/licenses/model/`.
pub fn copy_plugin_licences(plugin: &Path, out: &Path) -> Result<(), String> {
    for (from, to) in [
        (plugin.to_path_buf(), out.join("licenses")),
        (plugin.join("model"), out.join("licenses").join("model")),
    ] {
        for file in licence_files(&from) {
            std::fs::create_dir_all(&to).map_err(|error| format!("{}: {error}", to.display()))?;
            let target = to.join(file.file_name().unwrap_or_default());
            std::fs::copy(&file, &target)
                .map_err(|error| format!("{}: {error}", target.display()))?;
        }
    }
    Ok(())
}

/// The packages the package `root` links through normal dependencies on any target, in `metadata`
/// (`cargo metadata --format-version 1` with dependencies), without `root` itself. Procedural
/// macros run at build time and are not linked, nor is what only they depend on.
fn linked<'a>(metadata: &'a Value, root: &str) -> Result<Vec<&'a Value>, String> {
    let packages: BTreeMap<&str, &Value> = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata lists no packages")?
        .iter()
        .filter_map(|package| Some((package["id"].as_str()?, package)))
        .collect();
    let proc_macro = |id: &str| {
        packages.get(id).is_some_and(|package| {
            package["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|target| {
                    target["kind"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|kind| kind == "proc-macro")
                })
        })
    };
    let nodes: BTreeMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("cargo metadata has no resolve graph")?
        .iter()
        .filter_map(|node| Some((node["id"].as_str()?, node)))
        .collect();
    let mut seen = BTreeSet::new();
    let mut queue = vec![root];
    while let Some(id) = queue.pop() {
        let node = nodes
            .get(id)
            .ok_or_else(|| format!("{id} is not in the resolve graph"))?;
        for dependency in node["deps"].as_array().into_iter().flatten() {
            let normal = dependency["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind["kind"].is_null());
            if let Some(package) = dependency["pkg"].as_str()
                && normal
                && !proc_macro(package)
                && seen.insert(package)
            {
                queue.push(package);
            }
        }
    }
    seen.remove(root);
    let mut packages: Vec<&Value> = seen
        .iter()
        .filter_map(|id| packages.get(id).copied())
        .collect();
    packages.sort_by_key(|package| {
        (
            package["name"].as_str().unwrap_or_default().to_owned(),
            package["version"].as_str().unwrap_or_default().to_owned(),
        )
    });
    Ok(packages)
}

/// The licence and notice files in `directory` and the directories directly below it, by their
/// paths below `directory`.
fn licence_files_below(directory: &Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = licence_files(directory)
        .into_iter()
        .filter_map(|file| Some((file.file_name()?.to_str()?.to_owned(), file)))
        .collect();
    let mut subdirectories: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    subdirectories.sort();
    for subdirectory in subdirectories {
        let Some(prefix) = subdirectory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        for file in licence_files(&subdirectory) {
            if let Some(name) = file.file_name().and_then(|name| name.to_str()) {
                found.push((format!("{prefix}/{name}"), file.clone()));
            }
        }
    }
    found
}

/// `THIRD_PARTY_NOTICES.txt` for the crates package `root` links: each with its name, version,
/// licence expression and the text of the licence files in its source directory and the
/// directories directly below it.
pub fn third_party_notices(metadata: &Value, root: &str) -> Result<String, String> {
    let mut text = String::from(
        "Third-party software linked into this plugin's artifacts, with its licence terms.\n",
    );
    for package in linked(metadata, root)? {
        let name = package["name"].as_str().unwrap_or_default();
        let version = package["version"].as_str().unwrap_or_default();
        let licence = package["license"]
            .as_str()
            .or_else(|| package["license_file"].as_str())
            .unwrap_or("not stated");
        let _ = write!(
            text,
            "\n{}\n{name} {version}\nLicence: {licence}\n",
            "=".repeat(72)
        );
        let directory = package["manifest_path"]
            .as_str()
            .and_then(|path| Path::new(path).parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        for (name, file) in licence_files_below(&directory) {
            let body = std::fs::read_to_string(&file)
                .map_err(|error| format!("{}: {error}", file.display()))?;
            let _ = write!(text, "\n--- {name}\n\n{}\n", body.trim_end());
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(plugin: &str) -> (Value, String) {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(plugin)
            .join("Cargo.toml");
        let output = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--format-version", "1", "--manifest-path"])
            .arg(&manifest)
            .output()
            .expect("cargo metadata");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value = serde_json::from_slice(&output.stdout).expect("json");
        let wanted = std::fs::canonicalize(&manifest).expect("manifest");
        let root = metadata["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .find(|package| {
                package["manifest_path"]
                    .as_str()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    == Some(wanted.clone())
            })
            .expect("the plugin")["id"]
            .as_str()
            .expect("id")
            .to_owned();
        (metadata, root)
    }

    #[test]
    fn the_notices_name_every_linked_crate_with_its_licence_text() {
        let (metadata, root) = metadata("fixtures/digest");
        let text = third_party_notices(&metadata, &root).expect("notices");
        assert!(text.contains("\nserde_json "), "{text}");
        assert!(text.contains("Licence: MIT OR Apache-2.0"), "{text}");
        assert!(text.contains("--- LICENSE-MIT"), "{text}");
        assert!(
            !text.contains("\nalpha2-plugin-fixture-digest "),
            "the plugin itself"
        );
    }

    #[test]
    fn the_notices_carry_the_licence_of_the_c_sources_a_crate_builds() {
        let (metadata, root) = metadata("first-party/transcribe");
        let text = third_party_notices(&metadata, &root).expect("notices");
        let at = text.find("\nwhisper-rs-sys ").expect("whisper-rs-sys");
        let licence = text[at..]
            .find("--- whisper.cpp/LICENSE")
            .expect("its licence")
            + at;
        assert!(text[licence..].contains("The ggml authors"), "{text}");
    }

    #[test]
    fn the_silence_model_licence_is_copied() {
        let out = std::env::temp_dir().join(format!("alpha2-pack-licences-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../first-party/silence");
        copy_plugin_licences(&plugin, &out).expect("copied");
        let copied = std::fs::read_to_string(out.join("licenses/model/LICENSE")).expect("licence");
        assert!(copied.starts_with("MIT License"), "{copied}");
        std::fs::remove_dir_all(&out).expect("cleanup");
    }
}
