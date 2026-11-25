//! Builds a plugin crate for wasm32 and native targets and writes an upload-ready directory: the
//! artifacts, the resources, `manifest.json` with relative URLs, SHA-256 hashes and sizes, and the
//! unlisted licence files under `licenses/` and `THIRD_PARTY_NOTICES.txt`.
//!
//! ```text
//! cargo run -p alpha2-plugin-pack -- <plugin-dir> --out <dir> [--target <triple>]... [--no-native]
//! cargo run -p alpha2-plugin-pack -- merge <pack-dir> <pack-dir>... --out <dir>
//! ```
//!
//! The plugin crate has `descriptor.json` at its root; files under `resources/` are copied, and
//! `resources.json` lists entries written to the manifest as they are: resources hosted elsewhere
//! (`name`, `url`, `sha256`, `bytes`) and slots the caller fills (`name`, `supplied`);
//! `about.json`, when present, is written as the manifest's `about`. Native targets default to the
//! host's. `merge` joins packs of one plugin made on several machines. Nothing is uploaded.

mod check;
mod merge;
mod notices;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The flags every wasm artifact is built with.
const WASM_RUSTFLAGS: &str =
    "-Ctarget-feature=-relaxed-simd --cfg getrandom_backend=\"unsupported\"";

const USAGE: &str =
    "usage: alpha2-plugin-pack <plugin-dir> --out <dir> [--target <triple>]... [--no-native]
       alpha2-plugin-pack merge <pack-dir> <pack-dir>... --out <dir>";

struct Options {
    plugin: PathBuf,
    out: PathBuf,
    targets: Vec<String>,
    native: bool,
}

enum Job {
    Pack(Options),
    Merge { inputs: Vec<PathBuf>, out: PathBuf },
}

fn job() -> Result<Job, String> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) != Some("merge") {
        return options(args).map(Job::Pack);
    }
    args.next();
    let (mut inputs, mut out) = (Vec::new(), None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().map(PathBuf::from),
            _ if !arg.starts_with('-') => inputs.push(PathBuf::from(arg)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(Job::Merge {
        inputs,
        out: out.ok_or(USAGE)?,
    })
}

fn options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let (mut plugin, mut out, mut targets, mut native) = (None, None, Vec::new(), true);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().map(PathBuf::from),
            "--target" => targets.push(args.next().ok_or(USAGE)?),
            "--no-native" => native = false,
            _ if plugin.is_none() && !arg.starts_with('-') => plugin = Some(PathBuf::from(arg)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(Options {
        plugin: plugin.ok_or(USAGE)?,
        out: out.ok_or(USAGE)?,
        targets,
        native,
    })
}

fn run(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{command:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{command:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn host_triple() -> Result<String, String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    run(Command::new(rustc).arg("-vV"))?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV names no host".to_owned())
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn sha256(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Copies `bytes` to `url` under `out` and describes the file as a manifest entry does.
fn place(out: &Path, url: &str, bytes: &[u8]) -> Result<Value, String> {
    let path = out.join(url);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    std::fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(json!({ "url": url, "sha256": sha256(bytes), "bytes": bytes.len() }))
}

fn native_file(library: &str, target: &str) -> String {
    if target.contains("windows") {
        format!("{library}.dll")
    } else if target.contains("apple") {
        format!("lib{library}.dylib")
    } else {
        format!("lib{library}.so")
    }
}

fn build(manifest: &Path, target: &str, wasm: bool) -> Result<(), String> {
    let mut command = cargo();
    command.args(["build", "--release", "--target", target, "--manifest-path"]);
    command.arg(manifest);
    if wasm {
        // Relaxed SIMD results differ between runtimes; the target never enables threads. A
        // plugin has no randomness, so getrandom reports it unsupported instead of importing it.
        command.env(
            "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS",
            WASM_RUSTFLAGS,
        );
    }
    run(&mut command).map(drop)
}

/// The files under `<plugin>/resources/`, copied to `out`, and those `resources.json` lists.
fn resources(plugin: &Path, out: &Path) -> Result<Vec<Value>, String> {
    let mut resources = Vec::new();
    let local = plugin.join("resources");
    if local.is_dir() {
        let mut files = std::fs::read_dir(&local)
            .map_err(|error| error.to_string())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        files.sort();
        for path in files.iter().filter(|path| path.is_file()) {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("bad resource name")?;
            let mut entry = place(out, &format!("resources/{name}"), &read(path)?)?;
            entry["name"] = json!(name);
            resources.push(entry);
        }
    }
    let remote = plugin.join("resources.json");
    if remote.is_file() {
        let listed: Vec<Value> = serde_json::from_slice(&read(&remote)?)
            .map_err(|error| format!("resources.json: {error}"))?;
        resources.extend(listed);
    }

    Ok(resources)
}

fn pack(options: &Options) -> Result<PathBuf, String> {
    let manifest_path = options.plugin.join("Cargo.toml");
    let metadata: Value = serde_json::from_str(&run(cargo()
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(&manifest_path))?)
    .map_err(|error| error.to_string())?;
    let package = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            let wanted = std::fs::canonicalize(&manifest_path).ok()?;
            packages.iter().find(|package| {
                package["manifest_path"]
                    .as_str()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    == Some(wanted.clone())
            })
        })
        .ok_or("cargo metadata does not list the plugin crate")?;
    let library = package["name"]
        .as_str()
        .ok_or("unnamed package")?
        .replace('-', "_");
    let target_dir = PathBuf::from(
        metadata["target_directory"]
            .as_str()
            .ok_or("no target dir")?,
    );
    let descriptor_text = String::from_utf8(read(&options.plugin.join("descriptor.json"))?)
        .map_err(|_| "descriptor.json is not UTF-8".to_owned())?;
    let descriptor: Value = serde_json::from_str(&descriptor_text)
        .map_err(|error| format!("descriptor.json: {error}"))?;
    let id = descriptor["id"]
        .as_str()
        .ok_or("descriptor.json has no id")?
        .to_owned();

    std::fs::create_dir_all(&options.out).map_err(|error| error.to_string())?;
    let mut artifacts = Vec::new();
    build(&manifest_path, "wasm32-unknown-unknown", true)?;
    let module = read(&target_dir.join(format!("wasm32-unknown-unknown/release/{library}.wasm")))?;
    check::wasm_module(&module)?;
    let mut entry = place(&options.out, &format!("{library}.wasm"), &module)?;
    entry["target"] = json!("wasm32");
    entry["type"] = json!("wasm");
    artifacts.push(entry);
    let mut targets = options.targets.clone();
    if options.native && targets.is_empty() {
        targets.push(host_triple()?);
    }
    for target in targets.iter().filter(|_| options.native) {
        build(&manifest_path, target, false)?;
        let file = native_file(&library, target);
        let bytes = read(&target_dir.join(target).join("release").join(&file))?;
        let mut entry = place(&options.out, &format!("{target}/{file}"), &bytes)?;
        entry["target"] = json!(target);
        entry["type"] = json!("cdylib");
        artifacts.push(entry);
    }

    let resources = resources(&options.plugin, &options.out)?;
    notices::copy_plugin_licences(&options.plugin, &options.out)?;
    let root = package["id"].as_str().ok_or("the plugin crate has no id")?;
    let notices_path = options.out.join("THIRD_PARTY_NOTICES.txt");
    std::fs::write(
        &notices_path,
        notices::third_party_notices(&metadata, root)?,
    )
    .map_err(|error| format!("{}: {error}", notices_path.display()))?;

    let mut manifest = json!({
        "id": id,
        "version": package["version"],
        "abi": "alpha2-plugin/1",
        "kind": "extractor",
        "artifacts": artifacts,
    });
    if !resources.is_empty() {
        manifest["resources"] = json!(resources);
    }
    let about = options.plugin.join("about.json");
    if about.is_file() {
        let about: Value = serde_json::from_slice(&read(&about)?)
            .map_err(|error| format!("about.json: {error}"))?;
        if !about.is_object() {
            return Err("about.json is not an object".to_owned());
        }
        manifest["about"] = about;
    }
    let path = options.out.join("manifest.json");
    let text = manifest_text(manifest, &descriptor_text)?;
    std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

/// The manifest's text with `descriptor` (the crate's `descriptor.json`) as written. The artifact
/// returns those bytes from `describe`, and a loader compares the two parsed; a number serialized
/// again may not parse back to the same value, so the bytes are kept.
fn manifest_text(mut manifest: Value, descriptor: &str) -> Result<String, String> {
    const PLACEHOLDER: &str = "alpha2-plugin-pack:descriptor";
    manifest["descriptor"] = json!(PLACEHOLDER);
    let text = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    let mut text = text.replacen(&format!("\"{PLACEHOLDER}\""), descriptor.trim(), 1);
    text.push('\n');
    Ok(text)
}

fn main() -> ExitCode {
    let done = job().and_then(|job| match job {
        Job::Pack(options) => pack(&options),
        Job::Merge { inputs, out } => merge::merge(&inputs, &out),
    });
    match done {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_keeps_the_descriptor_text_as_written() {
        // Serialized again, this number would read back one step lower.
        let descriptor = r#"{ "id": "a.b", "params_schema": { "minimum": -99.999990023795055 } }"#;
        let text =
            manifest_text(json!({ "id": "a.b", "artifacts": [] }), descriptor).expect("manifest");
        assert!(text.contains("-99.999990023795055"), "{text}");
        let manifest: Value = serde_json::from_str(&text).expect("json");
        let written: Value = serde_json::from_str(descriptor).expect("json");
        assert_eq!(manifest["descriptor"], written);
        assert_eq!(manifest["id"], "a.b");
    }
}
