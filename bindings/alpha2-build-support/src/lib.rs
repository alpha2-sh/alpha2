//! Build-script helpers for the crates that link `FFmpeg` or compile its platform shims, plus the
//! C headers those shims share ([`shared_include_dir`]). Build-time only.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Hint shown when the build cannot find the pinned `FFmpeg` headers.
pub const FFMPEG_SDK_HINT: &str = "The native GPU interop shims need the pinned FFmpeg SDK headers, \
    including Vulkan. Run scripts/fetch-ffmpeg-sdk.sh <target> and point \
    FFMPEG_PKG_CONFIG_PATH at its lib/pkgconfig (on Windows, FFMPEG_INCLUDE_DIR at its \
    include and FFMPEG_LIBS_DIR at its lib), or build without the hwdecode and hwencode features.";

/// Directory of the C headers shared by `alpha2-ffmpeg-bridge` and `alpha2-native-gpu` (the
/// Vulkan frame ABI and the run-time loader), so both sides of the ABI compile one file.
#[must_use]
pub fn shared_include_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include")
}

/// Whether this is a docs.rs build, which has no `FFmpeg`. Build scripts then skip the C shims;
/// [`check_pinned_sdk`] and [`emit_ffmpeg_dev_rpath`] do nothing, but
/// [`generate_interop_bindings`] still runs because the Rust sources include its output.
#[must_use]
pub fn docs_rs() -> bool {
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    std::env::var_os("DOCS_RS").is_some()
}

/// Variables that decide which `FFmpeg` `rusty_ffmpeg` links.
const FFMPEG_LOCATION_VARIABLES: [&str; 7] = [
    "FFMPEG_DLL_PATH",
    "FFMPEG_PKG_CONFIG_PATH",
    "FFMPEG_LIBS_DIR",
    "FFMPEG_INCLUDE_DIR",
    "PKG_CONFIG",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
];

/// Where `rusty_ffmpeg` 0.16 takes `FFmpeg` from.
#[derive(Debug, PartialEq, Eq)]
enum FfmpegSource {
    /// pkg-config with `FFMPEG_PKG_CONFIG_PATH`, or the system search path when `None`.
    PkgConfig(Option<OsString>),
    /// Prebuilt libraries (`FFMPEG_LIBS_DIR` or the directory of `FFMPEG_DLL_PATH`) with headers
    /// from `FFMPEG_INCLUDE_DIR`.
    Directories {
        libs: PathBuf,
        include: Option<PathBuf>,
    },
    /// A Windows build with neither, which `rusty_ffmpeg` cannot link.
    Unset,
}

/// Mirrors `rusty_ffmpeg`'s lookup order on the build host, which ignores
/// `FFMPEG_PKG_CONFIG_PATH` and system pkg-config on Windows. Non-Unicode values count as unset,
/// as with its `env::var`.
fn ffmpeg_source(windows_host: bool, variable: impl Fn(&str) -> Option<String>) -> FfmpegSource {
    let include = || variable("FFMPEG_INCLUDE_DIR").map(PathBuf::from);
    if let Some(dll) = variable("FFMPEG_DLL_PATH") {
        let libs = Path::new(&dll).parent().unwrap_or(Path::new("")).to_owned();
        return FfmpegSource::Directories {
            libs,
            include: include(),
        };
    }
    if !windows_host && let Some(path) = variable("FFMPEG_PKG_CONFIG_PATH") {
        return FfmpegSource::PkgConfig(Some(path.into()));
    }
    if let Some(libs) = variable("FFMPEG_LIBS_DIR") {
        return FfmpegSource::Directories {
            libs: libs.into(),
            include: include(),
        };
    }
    if windows_host {
        FfmpegSource::Unset
    } else {
        FfmpegSource::PkgConfig(None)
    }
}

fn this_builds_ffmpeg_source() -> FfmpegSource {
    for name in FFMPEG_LOCATION_VARIABLES {
        println!("cargo:rerun-if-env-changed={name}");
    }
    ffmpeg_source(cfg!(windows), |name| std::env::var(name).ok())
}

/// Include directories of `libraries` for the `FFmpeg` that `rusty_ffmpeg` links.
///
/// Shims must compile against the linked `FFmpeg`'s headers, since `AVVkFrame` and similar
/// structs are not ABI-stable.
///
/// # Panics
/// Panics with [`FFMPEG_SDK_HINT`] when the headers cannot be located.
#[must_use]
pub fn ffmpeg_include_dirs(libraries: &[&str]) -> Vec<PathBuf> {
    let directories = match this_builds_ffmpeg_source() {
        FfmpegSource::PkgConfig(path) => {
            let output = pkg_config(path.as_deref(), "--cflags-only-I", libraries)
                .unwrap_or_else(|error| panic!("{error}. {FFMPEG_SDK_HINT}"));
            flag_paths(&output, b"-I")
        }
        FfmpegSource::Directories {
            include: Some(include),
            ..
        } => vec![include],
        FfmpegSource::Directories { include: None, .. } => {
            panic!(
                "FFMPEG_INCLUDE_DIR must name the headers of the FFmpeg in FFMPEG_LIBS_DIR. {FFMPEG_SDK_HINT}"
            )
        }
        FfmpegSource::Unset => panic!("no FFmpeg to compile against. {FFMPEG_SDK_HINT}"),
    };
    require_headers(&directories, libraries);
    directories
}

/// Runs pkg-config as `rusty_ffmpeg`'s pkg-config crate does, including system directories so
/// an `FFmpeg` under `/usr/include` is reported.
fn pkg_config(
    pkg_config_path: Option<&OsStr>,
    query: &str,
    libraries: &[&str],
) -> Result<Vec<u8>, String> {
    let program = std::env::var_os("PKG_CONFIG").unwrap_or_else(|| "pkg-config".into());
    let mut command = Command::new(&program);
    command
        .arg(query)
        .args(libraries)
        .env("PKG_CONFIG_ALLOW_SYSTEM_CFLAGS", "1")
        .env("PKG_CONFIG_ALLOW_SYSTEM_LIBS", "1");
    if let Some(path) = pkg_config_path {
        command.env("PKG_CONFIG_PATH", path);
    }
    let output = command.output().map_err(|error| {
        format!(
            "{} is required to find FFmpeg ({error})",
            program.to_string_lossy()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "pkg-config could not find {}: {}",
            libraries.join(", "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// Splits pkg-config output as pkg-config-rs's `split_flags` does: a backslash escapes the next
/// byte and only unescaped whitespace separates flags. pkg-config escapes shell characters and
/// often non-ASCII bytes, and a compiler silently skips a misread directory.
fn split_flags(output: &[u8]) -> Vec<Vec<u8>> {
    let mut flags = Vec::new();
    let mut flag = Vec::new();
    let mut escaped = false;
    for &byte in output {
        match byte {
            _ if escaped => {
                escaped = false;
                flag.push(byte);
            }
            b'\\' => escaped = true,
            b'\t' | b'\n' | b'\r' | b' ' => {
                if !flag.is_empty() {
                    flags.push(std::mem::take(&mut flag));
                }
            }
            _ => flag.push(byte),
        }
    }
    if !flag.is_empty() {
        flags.push(flag);
    }
    flags
}

fn flag_paths(output: &[u8], prefix: &[u8]) -> Vec<PathBuf> {
    split_flags(output)
        .into_iter()
        .filter_map(|flag| {
            let path = flag.strip_prefix(prefix)?;
            (!path.is_empty()).then(|| path_from_bytes(path.to_vec()))
        })
        .collect()
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes).into()
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
        .into()
}

/// Stops the build unless each library's `version.h` is under `directories`. A missing directory
/// is no error to a C compiler, which may then find a different `FFmpeg` of the same major.
fn require_headers(directories: &[PathBuf], libraries: &[&str]) {
    for library in libraries {
        let header = format!("{library}/version.h");
        assert!(
            directories
                .iter()
                .any(|directory| directory.join(&header).is_file()),
            "no FFmpeg include directory has {header}: looked in {directories:?}. {FFMPEG_SDK_HINT}"
        );
    }
}

/// The `libavutil` major version the `FFmpeg` shims are written against.
///
/// The shims read structs whose layout can change at any major version. Must match the pinned
/// SDK (`release/ffmpeg-lgpl/manifest.toml`); `alpha2_ffmpeg_version.h` fails the build on a
/// mismatch.
pub const FFMPEG_AVUTIL_MAJOR: u32 = 60;

/// The `libavcodec` major version; see [`FFMPEG_AVUTIL_MAJOR`].
pub const FFMPEG_AVCODEC_MAJOR: u32 = 62;

/// Shims rebuild when these change, even under an unchanged SDK path.
const FFMPEG_VERSION_HEADERS: [&str; 2] = ["libavutil/version.h", "libavcodec/version_major.h"];

/// Compiles `sources` into the static library `library` and links it into the calling package.
///
/// Warnings are errors. `external_includes` (such as `FFmpeg`'s headers) are searched with
/// their warnings suppressed.
///
/// # Panics
/// Panics with `failure` when the compiler fails.
pub fn compile_c_shim(
    library: &str,
    sources: &[&str],
    includes: &[PathBuf],
    external_includes: &[PathBuf],
    flags: &[&str],
    failure: &str,
) {
    compile(
        library,
        sources,
        includes,
        external_includes,
        flags,
        &[],
        failure,
    );
}

/// [`compile_c_shim`] for a shim that reads `FFmpeg` structs, with the pinned major versions
/// defined for its version guard.
///
/// # Panics
/// Panics with `failure` when the compiler fails, including on a major version mismatch.
pub fn compile_ffmpeg_shim(
    library: &str,
    sources: &[&str],
    includes: &[PathBuf],
    ffmpeg_includes: &[PathBuf],
    failure: &str,
) {
    for header in ffmpeg_version_headers(ffmpeg_includes) {
        println!("cargo:rerun-if-changed={}", header.display());
    }
    let avutil = FFMPEG_AVUTIL_MAJOR.to_string();
    let avcodec = FFMPEG_AVCODEC_MAJOR.to_string();
    compile(
        library,
        sources,
        includes,
        ffmpeg_includes,
        &[],
        &[
            ("ALPHA2_FFMPEG_AVUTIL_MAJOR", avutil.as_str()),
            ("ALPHA2_FFMPEG_AVCODEC_MAJOR", avcodec.as_str()),
        ],
        failure,
    );
}

fn ffmpeg_version_headers(include_dirs: &[PathBuf]) -> Vec<PathBuf> {
    include_dirs
        .iter()
        .flat_map(|directory| {
            FFMPEG_VERSION_HEADERS
                .iter()
                .map(move |header| directory.join(header))
        })
        .filter(|header| header.is_file())
        .collect()
}

/// Generates Rust declarations for `types` and every `ALPHA2_*` constant in `headers` into
/// `$OUT_DIR/<output>`, with bindgen's layout assertions.
///
/// # Panics
/// Panics when libclang cannot parse a header or the output cannot be written.
pub fn generate_interop_bindings(headers: &[PathBuf], types: &[&str], output: &str) {
    let mut builder = bindgen::Builder::default()
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_var("ALPHA2_.*")
        .derive_default(true)
        .generate_comments(false)
        .layout_tests(true);
    if let Some(target) = std::env::var("TARGET").ok().and_then(|target| {
        clang_target_override(&target, |key| {
            println!("cargo:rerun-if-env-changed={key}");
            std::env::var(key).ok()
        })
    }) {
        builder = builder.clang_arg(format!("--target={target}"));
    }
    for header in headers {
        let header = header
            .to_str()
            .unwrap_or_else(|| panic!("bindgen needs a UTF-8 header path: {}", header.display()));
        builder = builder.header(header);
    }
    for name in types {
        builder = builder.allowlist_type(format!("^{name}$"));
    }
    let bindings = builder
        .generate()
        .unwrap_or_else(|error| panic!("bindgen could not read {headers:?}: {error}"));
    let directory =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for build scripts"));
    bindings
        .write_to_file(directory.join(output))
        .unwrap_or_else(|error| panic!("could not write {output}: {error}"));
}

/// The Android API level declarations are parsed at when the toolchain names none: the supported
/// minimum.
const MIN_ANDROID_API_LEVEL: u32 = 29;

/// The clang triple for targets whose Rust spelling clang rejects. Bindgen 0.71 passes
/// `aarch64-apple-ios-sim` where clang wants `arm64-apple-ios-simulator`, and NDK r28+ headers
/// refuse an Android triple without an API level ([`android_api_level`]).
fn clang_target_override(
    target: &str,
    variable: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if target.ends_with("-linux-android") || target.ends_with("-linux-androideabi") {
        return Some(format!("{target}{}", android_api_level(target, variable)));
    }
    let (arch, rest) = target.split_once("-apple-")?;
    let os = rest.strip_suffix("-sim")?;
    let arch = if arch == "aarch64" { "arm64" } else { arch };
    Some(format!("{arch}-apple-{os}-simulator"))
}

/// The API level the build targets for Android `target`: an explicit `ANDROID_PLATFORM`
/// (`android-29` or `29`), else the level in the C toolchain cargo-ndk or the NDK's compiler
/// wrappers configure (`--target=aarch64-linux-android29`, `aarch64-linux-android29-clang`), else
/// [`MIN_ANDROID_API_LEVEL`].
fn android_api_level(target: &str, variable: impl Fn(&str) -> Option<String>) -> u32 {
    let underscored = target.replace('-', "_");
    let platform = ["CARGO_NDK_ANDROID_PLATFORM", "ANDROID_PLATFORM"]
        .into_iter()
        .filter_map(&variable)
        .find_map(|value| {
            let digits = value.trim().trim_start_matches("android-");
            digits.parse::<u32>().ok()
        });
    let toolchain = || {
        [
            "_CARGO_NDK_LINK_TARGET".to_owned(),
            format!("CFLAGS_{target}"),
            format!("CFLAGS_{underscored}"),
            "TARGET_CFLAGS".to_owned(),
            format!("CC_{target}"),
            format!("CC_{underscored}"),
            "TARGET_CC".to_owned(),
        ]
        .iter()
        .filter_map(|key| variable(key))
        .find_map(|value| api_level_after_triple(&value))
    };
    platform.or_else(toolchain).unwrap_or(MIN_ANDROID_API_LEVEL)
}

/// The digits right after an Android triple in `value`, as clang spells an API level into it.
fn api_level_after_triple(value: &str) -> Option<u32> {
    value
        .match_indices("-linux-android")
        .find_map(|(at, triple)| {
            let rest = &value[at + triple.len()..];
            let rest = rest.strip_prefix("eabi").unwrap_or(rest);
            let digits = rest
                .find(|character: char| !character.is_ascii_digit())
                .map_or(rest, |end| &rest[..end]);
            digits.parse().ok()
        })
}

#[allow(clippy::too_many_arguments)]
fn compile(
    library: &str,
    sources: &[&str],
    includes: &[PathBuf],
    external_includes: &[PathBuf],
    flags: &[&str],
    defines: &[(&str, &str)],
    failure: &str,
) {
    for source in sources {
        println!("cargo:rerun-if-changed={source}");
    }
    let mut build = cc::Build::new();
    build
        .files(sources)
        .includes(includes)
        .std("c11")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true);
    for (name, value) in defines {
        build.define(name, *value);
    }
    let compiler = build.get_compiler();
    let style = if compiler.is_like_clang_cl() {
        ExternalStyle::ClangCl
    } else if compiler.is_like_msvc() {
        ExternalStyle::Msvc
    } else {
        ExternalStyle::Isystem
    };
    for flag in external_include_flags(external_includes, style) {
        build.flag(flag);
    }
    for flag in flags {
        build.flag(flag);
    }
    if let Err(error) = build.try_compile(library) {
        panic!("{failure}: {error}{}", toolchain_floor(style));
    }
}

/// Hint appended to a compile failure. `cl.exe` `/std:c11` needs Windows SDK 10.0.20348.0 or
/// later because older `winbase.h` warns (C5105) under the conforming preprocessor.
fn toolchain_floor(style: ExternalStyle) -> &'static str {
    match style {
        ExternalStyle::Msvc => {
            ". The shims are C11 (/std:c11), which MSVC compiles only with Windows SDK \
             10.0.20348.0 or later (Visual Studio 2019 16.8+, or 2022); install a newer \
             Windows SDK if the errors are in its headers"
        }
        ExternalStyle::Isystem | ExternalStyle::ClangCl => "",
    }
}

#[derive(Clone, Copy)]
enum ExternalStyle {
    Isystem,
    /// `cl.exe` needs `/external:W0` as well as `/external:I`.
    Msvc,
    /// clang-cl rejects `/external:W0` as unused under `/WX`.
    ClangCl,
}

fn external_include_flags(directories: &[PathBuf], style: ExternalStyle) -> Vec<OsString> {
    let mut flags = Vec::new();
    for directory in directories {
        if let ExternalStyle::Isystem = style {
            flags.push("-isystem".into());
            flags.push(directory.into());
        } else {
            let mut flag = OsString::from("/external:I");
            flag.push(directory);
            flags.push(flag);
        }
    }
    if let ExternalStyle::Msvc = style
        && !directories.is_empty()
    {
        flags.push("/external:W0".into());
    }
    flags
}

/// On macOS, adds the linked `FFmpeg`'s libdir as an rpath to the calling package's executables
/// and tests.
///
/// The SDK dylibs use `@rpath` install names and SIP strips `DYLD_*`, so dev binaries need an
/// `LC_RPATH`. It is emitted per package because an rpath in `RUSTFLAGS` breaks wasm builds of
/// the workspace. Every package that links `FFmpeg` into its own tests must call this. Skipped
/// when `RUSTFLAGS` already carries an rpath, as release builds do.
pub fn emit_ffmpeg_dev_rpath() {
    if docs_rs() {
        return;
    }
    let source = this_builds_ffmpeg_source();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let rustflags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if !wants_dev_rpath(&target_os, &rustflags) {
        return;
    }
    if let Some(libdir) = ffmpeg_libdir(source) {
        for directive in rpath_directives(&libdir) {
            println!("{directive}");
        }
    }
}

/// Puts the pinned `FFmpeg`'s libdir on the link search path from the calling package.
///
/// `rusty_ffmpeg` adds that directory too, but cargo orders search paths by the dependency graph,
/// and a system directory another dependency adds (`alsa-sys` adds `/usr/lib/<triple>`) can come
/// first and link a distribution's older `FFmpeg` instead. The packages that call `FFmpeg` come
/// before their audio dependencies, so a path they add wins. Only a pinned `FFmpeg`
/// (`FFMPEG_PKG_CONFIG_PATH` or `FFMPEG_LIBS_DIR`) is added.
pub fn emit_ffmpeg_link_search() {
    if docs_rs() {
        return;
    }
    let source = this_builds_ffmpeg_source();
    if matches!(source, FfmpegSource::PkgConfig(None) | FfmpegSource::Unset) {
        return;
    }
    if let Some(libdir) = ffmpeg_libdir(source) {
        println!("cargo:rustc-link-search=native={}", libdir.display());
    }
}

fn ffmpeg_libdir(source: FfmpegSource) -> Option<PathBuf> {
    match source {
        FfmpegSource::PkgConfig(path) => {
            let output = pkg_config(path.as_deref(), "--libs-only-L", &["libavcodec"]).ok()?;
            flag_paths(&output, b"-L").into_iter().next()
        }
        FfmpegSource::Directories { libs, .. } => Some(libs),
        FfmpegSource::Unset => None,
    }
}

/// Checks that `rusty_ffmpeg` links the `FFmpeg` the build was pointed at.
///
/// Warns when `FFMPEG_PKG_CONFIG_PATH` is unset on macOS or Linux (any system `FFmpeg` would
/// link) or set on a Windows host (ignored there). pkg-config searches that path before its
/// defaults, not instead of them, so a path without `libavcodec.pc` silently falls through.
///
/// # Panics
/// Panics when pkg-config finds libavcodec outside `FFMPEG_PKG_CONFIG_PATH`.
pub fn check_pinned_sdk() {
    if docs_rs() {
        return;
    }
    let source = this_builds_ffmpeg_source();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if cfg!(windows) && std::env::var_os("FFMPEG_PKG_CONFIG_PATH").is_some() {
        println!(
            "cargo:warning=FFMPEG_PKG_CONFIG_PATH is ignored on a Windows build host: \
             rusty_ffmpeg links FFMPEG_LIBS_DIR, with headers from FFMPEG_INCLUDE_DIR."
        );
    }
    match source {
        FfmpegSource::PkgConfig(None) if matches!(target_os.as_str(), "macos" | "linux") => {
            println!(
                "cargo:warning=FFMPEG_PKG_CONFIG_PATH is not set, so this build links whatever \
                 FFmpeg pkg-config finds, not the pinned SDK. Set it to \
                 \"$(scripts/fetch-ffmpeg-sdk.sh)/lib/pkgconfig\"."
            );
        }
        FfmpegSource::PkgConfig(Some(path)) => {
            if let Some(found) = pc_file_dir(&path)
                && let Some(mismatch) = found_outside(&path, &found)
            {
                panic!("{mismatch}");
            }
        }
        _ => {}
    }
}

fn pc_file_dir(pkg_config_path: &OsStr) -> Option<PathBuf> {
    let output = pkg_config(
        Some(pkg_config_path),
        "--variable=pcfiledir",
        &["libavcodec"],
    )
    .ok()?;
    let value = output.trim_ascii_end();
    (!value.is_empty()).then(|| path_from_bytes(unescape(value)))
}

/// pkgconf escapes spaces in `pcfiledir`; freedesktop pkg-config does not.
fn unescape(value: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut escaped = false;
    for &byte in value {
        if byte == b'\\' && !escaped {
            escaped = true;
        } else {
            escaped = false;
            bytes.push(byte);
        }
    }
    bytes
}

fn found_outside(pkg_config_path: &OsStr, found: &Path) -> Option<String> {
    let canonical = found.canonicalize().ok();
    let named = std::env::split_paths(pkg_config_path)
        .any(|entry| canonical.is_some() && entry.canonicalize().ok() == canonical);
    (!named).then(|| {
        format!(
            "FFMPEG_PKG_CONFIG_PATH is {}, but pkg-config reads libavcodec from {}, \
             outside it: the path has no libavcodec.pc, so pkg-config fell through to \
             another FFmpeg. Point it at the SDK's lib/pkgconfig, \
             \"$(scripts/fetch-ffmpeg-sdk.sh)/lib/pkgconfig\".",
            pkg_config_path.to_string_lossy(),
            found.display()
        )
    })
}

fn wants_dev_rpath(target_os: &str, encoded_rustflags: &str) -> bool {
    target_os == "macos" && !encoded_rustflags.contains("rpath")
}

/// `-Wl,` splits at commas, so a libdir containing one uses `-Xlinker` pairs.
fn rpath_directives(libdir: &Path) -> Vec<String> {
    let Some(libdir) = libdir.to_str() else {
        return vec![format!(
            "cargo:warning=no development rpath: the FFmpeg libdir {} is not UTF-8",
            libdir.display()
        )];
    };
    if libdir.contains(',') {
        ["-Xlinker", "-rpath", "-Xlinker", libdir]
            .iter()
            .map(|argument| format!("cargo:rustc-link-arg={argument}"))
            .collect()
    } else {
        vec![format!("cargo:rustc-link-arg=-Wl,-rpath,{libdir}")]
    }
}

#[cfg(test)]
mod tests;
