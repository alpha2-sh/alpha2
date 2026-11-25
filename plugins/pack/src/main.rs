//! Builds a plugin crate for wasm32 and native targets and writes an upload-ready directory: the
//! artifacts, the resources, `manifest.json` with relative URLs, SHA-256 hashes and sizes, and the
//! unlisted licence files under `licenses/` and `THIRD_PARTY_NOTICES.txt`.
//!
//! ```text
//! cargo run -p alpha2-plugin-pack -- <plugin-dir> --out <dir> [--target <triple>]... [--no-native]
//! cargo run -p alpha2-plugin-pack -- merge <pack-dir> <pack-dir>... --out <dir>
//! cargo run -p alpha2-plugin-pack -- builds-wasm <plugin-dir>
//! ```
//!
//! The plugin crate has `descriptor.json` at its root; files under `resources/` are copied, and
//! `resources.json` lists entries written to the manifest as they are: resources hosted elsewhere
//! (`name`, `url`, `sha256`, `bytes`) and slots the caller fills (`name`, `supplied`);
//! `about.json`, when present, is written as the manifest's `about`. Native targets default to the
//! host's. A crate whose `Cargo.toml` sets `[package.metadata.alpha2-plugin] wasm = false` is
//! packed without a wasm artifact and refuses `--no-native`; `builds-wasm` prints `true` or `false`
//! for a crate. `merge` joins packs of one plugin made on several machines. Nothing is uploaded.

mod check;
mod merge;
mod notices;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WASM_RUSTFLAGS: [&str; 3] = [
    "-Ctarget-feature=+simd128,-relaxed-simd",
    "--cfg",
    "getrandom_backend=\"unsupported\"",
];

const NATIVE_BUILD_ENV: [(&str, &str); 2] = [("GGML_NATIVE", "OFF"), ("GGML_CCACHE", "OFF")];

const USAGE: &str =
    "usage: alpha2-plugin-pack <plugin-dir> --out <dir> [--target <triple>]... [--no-native]
       alpha2-plugin-pack merge <pack-dir> <pack-dir>... --out <dir>
       alpha2-plugin-pack builds-wasm <plugin-dir>";

struct Options {
    plugin: PathBuf,
    out: PathBuf,
    targets: Vec<String>,
    native: bool,
}

enum Job {
    Pack(Options),
    Merge { inputs: Vec<PathBuf>, out: PathBuf },
    BuildsWasm(PathBuf),
}

fn job() -> Result<Job, String> {
    let mut args = std::env::args().skip(1).peekable();
    match args.peek().map(String::as_str) {
        Some("merge") => {}
        Some("builds-wasm") => {
            args.next();
            return match (args.next(), args.next()) {
                (Some(plugin), None) if !plugin.starts_with('-') => {
                    Ok(Job::BuildsWasm(PathBuf::from(plugin)))
                }
                _ => Err(USAGE.to_owned()),
            };
        }
        _ => return options(args).map(Job::Pack),
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

fn rustc() -> Command {
    Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
}

fn rustc_version(field: &str) -> Result<String, String> {
    run(rustc().arg("-vV"))?
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(": "))
        .map(str::to_owned)
        .ok_or_else(|| format!("rustc -vV names no {field}"))
}

fn host_triple() -> Result<String, String> {
    rustc_version("host")
}

fn local_root(metadata: &Value) -> Option<PathBuf> {
    let mut directories = metadata["packages"]
        .as_array()?
        .iter()
        .filter(|package| package["source"].is_null())
        .filter_map(|package| Path::new(package["manifest_path"].as_str()?).parent());
    let first = directories.next()?.to_owned();
    let root = directories.try_fold(first, |root, directory| {
        root.ancestors()
            .find(|ancestor| directory.starts_with(ancestor))
            .map(Path::to_owned)
    })?;
    root.parent().is_some().then_some(root)
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(std::env::home_dir()?.join(".cargo")))
}

/// rustc applies the last `--remap-path-prefix` that matches, so a nested directory comes later.
fn path_prefixes(metadata: &Value, target_dir: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let sysroot = PathBuf::from(run(rustc().args(["--print", "sysroot"]))?.trim());
    let commit = rustc_version("commit-hash")?;
    let mut prefixes = Vec::new();
    if let Some(root) = local_root(metadata) {
        prefixes.push((root, "/alpha2".to_owned()));
    }
    if let Some(home) = cargo_home() {
        prefixes.push((home, "/cargo".to_owned()));
    }
    prefixes.push((
        sysroot.join("lib").join("rustlib").join("src").join("rust"),
        format!("/rustc/{commit}"),
    ));
    prefixes.push((target_dir.to_owned(), "/target".to_owned()));
    Ok(prefixes)
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

fn build(
    manifest: &Path,
    target: &str,
    wasm: bool,
    prefixes: &[(PathBuf, String)],
) -> Result<(), String> {
    let mut command = cargo();
    command.args(["build", "--release", "--target", target, "--manifest-path"]);
    command.arg(manifest);
    let mut rustflags: Vec<String> = Vec::new();
    if wasm {
        // Relaxed SIMD results differ between runtimes; the target never enables threads. A
        // plugin has no randomness, so getrandom reports it unsupported instead of importing it.
        rustflags.extend(WASM_RUSTFLAGS.map(str::to_owned));
    } else {
        command.envs(NATIVE_BUILD_ENV);
    }
    let mut cflags = Vec::new();
    for (from, to) in prefixes {
        let from = from
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", from.display()))?;
        rustflags.push(format!("--remap-path-prefix={from}={to}"));
        if !from.contains(char::is_whitespace) {
            cflags.push(format!("-ffile-prefix-map={from}={to}"));
        }
    }
    command.env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\u{1f}"));
    if !wasm && !target.contains("msvc") {
        for variable in ["CFLAGS", "CXXFLAGS"] {
            let given = std::env::var(variable).unwrap_or_default();
            let flags = std::iter::once(given.as_str())
                .chain(cflags.iter().map(String::as_str))
                .filter(|flag| !flag.is_empty())
                .collect::<Vec<_>>();
            command.env(variable, flags.join(" "));
        }
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

fn builds_wasm(package: &Value) -> Result<bool, String> {
    match &package["metadata"]["alpha2-plugin"]["wasm"] {
        Value::Null => Ok(true),
        Value::Bool(wasm) => Ok(*wasm),
        other => Err(format!(
            "package.metadata.alpha2-plugin.wasm is {other}, not true or false"
        )),
    }
}

fn metadata(manifest_path: &Path) -> Result<Value, String> {
    serde_json::from_str(&run(cargo()
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(manifest_path))?)
    .map_err(|error| error.to_string())
}

fn package<'a>(metadata: &'a Value, manifest_path: &Path) -> Result<&'a Value, String> {
    metadata["packages"]
        .as_array()
        .and_then(|packages| {
            let wanted = std::fs::canonicalize(manifest_path).ok()?;
            packages.iter().find(|package| {
                package["manifest_path"]
                    .as_str()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    == Some(wanted.clone())
            })
        })
        .ok_or_else(|| "cargo metadata does not list the plugin crate".to_owned())
}

fn pack(options: &Options) -> Result<PathBuf, String> {
    let manifest_path = options.plugin.join("Cargo.toml");
    let metadata = metadata(&manifest_path)?;
    let package = package(&metadata, &manifest_path)?;
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

    let wasm = builds_wasm(package)?;
    if !wasm && !options.native {
        return Err(format!(
            "{} is native only (package.metadata.alpha2-plugin.wasm = false): --no-native leaves \
             nothing to pack",
            package["name"].as_str().unwrap_or_default()
        ));
    }

    let prefixes = path_prefixes(&metadata, &target_dir)?;
    std::fs::create_dir_all(&options.out).map_err(|error| error.to_string())?;
    let mut artifacts = Vec::new();
    if wasm {
        build(&manifest_path, "wasm32-unknown-unknown", true, &prefixes)?;
        let module =
            read(&target_dir.join(format!("wasm32-unknown-unknown/release/{library}.wasm")))?;
        check::wasm_module(&module)?;
        let mut entry = place(&options.out, &format!("{library}.wasm"), &module)?;
        entry["target"] = json!("wasm32");
        entry["type"] = json!("wasm");
        artifacts.push(entry);
    }
    let mut targets = options.targets.clone();
    if options.native && targets.is_empty() {
        targets.push(host_triple()?);
    }
    for target in targets.iter().filter(|_| options.native) {
        build(&manifest_path, target, false, &prefixes)?;
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
        Job::Pack(options) => pack(&options).map(|path| path.display().to_string()),
        Job::Merge { inputs, out } => {
            merge::merge(&inputs, &out).map(|path| path.display().to_string())
        }
        Job::BuildsWasm(plugin) => {
            let manifest_path = plugin.join("Cargo.toml");
            let metadata = metadata(&manifest_path)?;
            builds_wasm(package(&metadata, &manifest_path)?).map(|wasm| wasm.to_string())
        }
    });
    match done {
        Ok(text) => {
            println!("{text}");
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
    fn a_crate_builds_wasm_unless_its_metadata_says_wasm_false() {
        assert_eq!(builds_wasm(&json!({ "metadata": null })), Ok(true));
        assert_eq!(
            builds_wasm(&json!({ "metadata": { "other": { "wasm": false } } })),
            Ok(true)
        );
        assert_eq!(
            builds_wasm(&json!({ "metadata": { "alpha2-plugin": { "wasm": true } } })),
            Ok(true)
        );
        assert_eq!(
            builds_wasm(&json!({ "metadata": { "alpha2-plugin": { "wasm": false } } })),
            Ok(false)
        );
        let error = builds_wasm(&json!({ "metadata": { "alpha2-plugin": { "wasm": "no" } } }))
            .expect_err("refused");
        assert!(error.contains("not true or false"), "{error}");
    }

    #[test]
    fn a_native_only_crate_refuses_no_native_before_building() {
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../first-party/transcribe");
        let out =
            std::env::temp_dir().join(format!("alpha2-pack-no-native-{}", std::process::id()));
        let error = pack(&Options {
            plugin,
            out: out.clone(),
            targets: Vec::new(),
            native: false,
        })
        .expect_err("refused");
        assert!(error.contains("native only"), "{error}");
        assert!(!out.exists(), "nothing written");
    }

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
