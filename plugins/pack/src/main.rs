//! Builds a plugin crate for wasm32 and native targets and writes an upload-ready directory: the
//! artifacts, the resources and `manifest.json` with relative URLs, SHA-256 hashes and sizes.
//!
//! ```text
//! cargo run -p alpha2-plugin-pack -- <plugin-dir> --out <dir> [--target <triple>]... [--no-native]
//! ```
//!
//! The plugin crate has `descriptor.json` at its root; files under `resources/` are copied, and
//! `resources.json` lists resources hosted elsewhere (`name`, `url`, `sha256`, `bytes`). Native
//! targets default to the host's. Nothing is uploaded.

mod check;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const USAGE: &str =
    "usage: alpha2-plugin-pack <plugin-dir> --out <dir> [--target <triple>]... [--no-native]";

struct Options {
    plugin: PathBuf,
    out: PathBuf,
    targets: Vec<String>,
    native: bool,
}

fn options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
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
        // Results must not depend on the runtime, and modules run on one thread.
        command.env(
            "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS",
            "-Ctarget-feature=-relaxed-simd,-atomics",
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
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
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
    let descriptor: Value = serde_json::from_slice(&read(&options.plugin.join("descriptor.json"))?)
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

    let mut manifest = json!({
        "id": id,
        "version": package["version"],
        "abi": "alpha2-plugin/1",
        "kind": "extractor",
        "artifacts": artifacts,
        "descriptor": descriptor,
    });
    if !resources.is_empty() {
        manifest["resources"] = json!(resources);
    }
    let path = options.out.join("manifest.json");
    let mut text = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    text.push('\n');
    std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

fn main() -> ExitCode {
    match options().and_then(|options| pack(&options)) {
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
