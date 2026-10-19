//! Build-script helpers shared by the crates that link executables and tests
//! against `FFmpeg` (`alpha2-host`, `alpha2-native`, `alpha2-ffmpeg-bridge`,
//! `alpha2-native-gpu`) and the two of them that compile its platform shims
//! (`alpha2-ffmpeg-bridge`, `alpha2-native-gpu`), and the C headers those two
//! share ([`shared_include_dir`]). A `[build-dependencies]` crate: nothing
//! here reaches a runtime binary.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What to tell someone whose build cannot find the pinned `FFmpeg` headers.
pub const FFMPEG_SDK_HINT: &str = "The native GPU interop shims need the pinned FFmpeg SDK headers, \
    including Vulkan. Run scripts/fetch-ffmpeg-sdk.sh <target> and point \
    FFMPEG_PKG_CONFIG_PATH at its lib/pkgconfig (on Windows, FFMPEG_INCLUDE_DIR at its \
    include and FFMPEG_LIBS_DIR at its lib), or build without the hwdecode and hwencode features.";

/// The directory of the C headers that `alpha2-ffmpeg-bridge`'s shims and
/// `alpha2-native-gpu`'s helpers both compile: the Vulkan frame ABI between
/// them (`alpha2_vulkan_interop.h`) and the run-time Vulkan loader
/// (`alpha2_vulkan_loader.h`). They ship in this crate, the build-dependency
/// of both, so the two sides of that ABI read one file, in the repository and
/// in the published packages alike.
#[must_use]
pub fn shared_include_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include")
}

/// Whether this build documents the crate on docs.rs, which runs rustdoc
/// without linking and has no `FFmpeg`. A build script then compiles no C
/// shim and leaves the SDK check and the rpath out ([`check_pinned_sdk`] and
/// [`emit_ffmpeg_dev_rpath`] do nothing); it still generates the interop
/// declarations ([`generate_interop_bindings`]), which the Rust sources
/// include.
#[must_use]
pub fn docs_rs() -> bool {
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    std::env::var_os("DOCS_RS").is_some()
}

/// The environment variables that decide which `FFmpeg` `rusty_ffmpeg`
/// links, and so which one the shims and the rpath must follow.
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
    /// pkg-config, under this `PKG_CONFIG_PATH` (`FFMPEG_PKG_CONFIG_PATH`)
    /// or, without one, the system's search path (`link_system_ffmpeg`).
    PkgConfig(Option<OsString>),
    /// Prebuilt libraries in `libs` (`FFMPEG_LIBS_DIR`, or the directory of
    /// `FFMPEG_DLL_PATH`), with bindings generated from `include`
    /// (`FFMPEG_INCLUDE_DIR`).
    Directories {
        libs: PathBuf,
        include: Option<PathBuf>,
    },
    /// A Windows build with neither: `rusty_ffmpeg` has no way to link.
    Unset,
}

/// `rusty_ffmpeg`'s own order on this build host. Its build script, like
/// this one, runs on the host, and on a Windows host it never reads
/// `FFMPEG_PKG_CONFIG_PATH`. `FFMPEG_DLL_PATH` comes first everywhere; then,
/// off Windows, `FFMPEG_PKG_CONFIG_PATH`; then `FFMPEG_LIBS_DIR`; last, off
/// Windows, pkg-config on the system's path. `rusty_ffmpeg` reads each with
/// `env::var`, so a value that is not Unicode counts as unset.
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

/// [`ffmpeg_source`] for this build, rerunning it whenever one of the
/// variables it reads changes.
fn this_builds_ffmpeg_source() -> FfmpegSource {
    for name in FFMPEG_LOCATION_VARIABLES {
        println!("cargo:rerun-if-env-changed={name}");
    }
    ffmpeg_source(cfg!(windows), |name| std::env::var(name).ok())
}

/// The include directories of `libraries` for the `FFmpeg` that
/// `rusty_ffmpeg` links.
///
/// A shim compiled against another `FFmpeg`'s headers than the one linked
/// would read `AVVkFrame` and friends, which are not ABI-stable, at the wrong
/// offsets, so this follows `rusty_ffmpeg`'s own order for the build host
/// (`ffmpeg_source`): the headers are asked of pkg-config where it links
/// through pkg-config, and are `FFMPEG_INCLUDE_DIR` where it links prebuilt
/// libraries (`FFMPEG_LIBS_DIR`, always on Windows).
///
/// # Panics
/// Panics with [`FFMPEG_SDK_HINT`] when pkg-config is needed and is missing
/// or does not know the libraries, when prebuilt libraries come without
/// `FFMPEG_INCLUDE_DIR`, and when a library's headers are not in the
/// directories found.
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

/// Runs pkg-config the way `rusty_ffmpeg`'s pkg-config crate does: the
/// executable `PKG_CONFIG` names, `pkg_config_path` as `PKG_CONFIG_PATH`
/// when given, and system directories reported rather than left out, so an
/// `FFmpeg` under `/usr/include` comes back as a directory like any other.
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

/// pkg-config output split into flags as pkg-config-rs's `split_flags`,
/// which `rusty_ffmpeg` links through, splits it: a backslash keeps the next
/// byte literally, and only unescaped whitespace separates flags. Every
/// pkg-config escapes shell characters and, all but pkgconf 3, every
/// non-ASCII byte; pkgconf also escapes the spaces of a path. Read literally,
/// `a(b)\&c` or an escaped Korean directory name is a directory that does not
/// exist, which a compiler skips without a word.
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

/// The paths of the flags in pkg-config `output` that start with `prefix`
/// (`-I`, `-L`).
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

/// Stops the build unless each of `libraries` has its `version.h` under one
/// of `directories`. A directory that does not exist is no error to a C
/// compiler, which goes on to its default include path and may find another
/// `FFmpeg` there of the same major, which the version guard lets through.
fn require_headers(directories: &[PathBuf], libraries: &[&str]) {
    for library in libraries {
        // Named as the shims `#include` it, so the message reads the same on
        // every host.
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
/// The shims read structures `FFmpeg` may lay out differently at any major
/// version: `AVVkFrame`, `AVVulkanFramesContext`, and the D3D12 and
/// `VideoToolbox` device and frame contexts. This and
/// [`FFMPEG_AVCODEC_MAJOR`] are the pinned SDK's
/// (`release/ffmpeg-lgpl/manifest.toml`). [`compile_ffmpeg_shim`] defines
/// them for `alpha2_ffmpeg_version.h`, which stops the build with `#error`
/// when the headers are another major's, instead of reading the wrong offsets.
/// At run time the libraries' sonames carry the same major, so the loader
/// already refuses a mismatched runtime.
pub const FFMPEG_AVUTIL_MAJOR: u32 = 60;

/// The `libavcodec` major version the `FFmpeg` shims are written against;
/// see [`FFMPEG_AVUTIL_MAJOR`].
pub const FFMPEG_AVCODEC_MAJOR: u32 = 62;

/// `FFmpeg` headers whose change means a different `FFmpeg`: a shim is
/// rebuilt when one of them changes, even under an unchanged SDK path.
const FFMPEG_VERSION_HEADERS: [&str; 2] = ["libavutil/version.h", "libavcodec/version_major.h"];

/// Compiles `sources` into the static library `library` and links it into the
/// calling package.
///
/// The `cc` crate picks the target's compiler, so this works with GCC, Clang,
/// MSVC, and clang-cl, and honours `CC`, `CC_<target>`, and `CFLAGS` (and
/// reruns when they change). Warnings are errors on every compiler.
///
/// `external_includes` are headers this repository does not own, `FFmpeg`'s:
/// they are searched like `includes` but their warnings are not ours to fix.
/// MSVC's `/W4` flags narrowing returns in `libavutil/common.h` (C4244), which
/// stopped the Windows build under warnings-as-errors.
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

/// Compiles a shim that reads `FFmpeg` structures, against the `FFmpeg`
/// headers in `ffmpeg_includes`.
///
/// This is [`compile_c_shim`] with [`FFMPEG_AVUTIL_MAJOR`] and
/// [`FFMPEG_AVCODEC_MAJOR`] defined for the version guard each such shim
/// includes, and with a rebuild whenever `FFmpeg`'s version headers change.
///
/// # Panics
/// Panics with `failure` when the compiler fails, which includes the guard's
/// `#error` for headers of another major version.
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

/// The version headers that exist under `include_dirs`.
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

/// Generates Rust declarations for the named types, and for every
/// `ALPHA2_*` constant, of `headers`, into `$OUT_DIR/<output>`.
///
/// The headers are the fixed-width ABI this repository's C shims share with
/// Rust. Generating the Rust side replaces hand-written `#[repr(C)]` copies,
/// which could drift from a header without any build noticing. Each struct
/// comes with bindgen's layout assertions: compile-time checks that Rust lays
/// it out the way clang lays out the header for the target being built.
///
/// # Panics
/// Panics when libclang cannot parse a header or the output cannot be
/// written.
pub fn generate_interop_bindings(headers: &[PathBuf], types: &[&str], output: &str) {
    let mut builder = bindgen::Builder::default()
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_var("ALPHA2_.*")
        .derive_default(true)
        .generate_comments(false)
        .layout_tests(true);
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

/// The toolchain floor the shared flags set, for a compile failure on a
/// compiler that has one. `cl.exe` takes `/std:c11` only with Windows SDK
/// 10.0.20348.0 or later: it turns on the conforming preprocessor, under which
/// an older SDK's `winbase.h` warns (C5105), and `/WX` makes that an error.
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

/// How a compiler is told a directory holds external headers.
#[derive(Clone, Copy)]
enum ExternalStyle {
    /// GCC and Clang.
    Isystem,
    /// `cl.exe`, whose `/external:I` still warns unless `/external:W0`.
    Msvc,
    /// clang-cl, where `/external:I` is `-isystem` and `/external:W0` is an
    /// unused argument, which `/WX` turns into an error (clang 14).
    ClangCl,
}

/// Compiler flags that search `directories` as external headers.
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

/// Adds the linked `FFmpeg`'s libdir as an rpath to the calling package's
/// executables and tests, on macOS.
///
/// Every package that links `FFmpeg` into a binary of its own calls this,
/// libraries included: `cargo test` links each one's unit tests, and a
/// library's tests have no dependent's rpath to borrow.
///
/// The pinned SDK's dylibs have `@rpath/libavcodec.62.dylib` install names,
/// and a plain `cargo build` writes no `LC_RPATH`, so the host could not load
/// them once SIP had stripped `DYLD_*` from whatever launched it (`npm run`,
/// an application's server). Passing the rpath through `RUSTFLAGS` works for
/// the host but reaches every target, and `rust-lld` rejects it in a wasm
/// build of the same workspace. Emitted from a build script it applies to
/// that one package only. The libdir is that of the `FFmpeg` `rusty_ffmpeg`
/// links (`ffmpeg_source`): `FFMPEG_LIBS_DIR` when it links prebuilt
/// libraries, otherwise libavcodec's `-L` as pkg-config reports it under the
/// same path and as `rusty_ffmpeg` reads it.
///
/// A caller that chose an rpath of its own keeps it: the release build passes
/// `@executable_path/../lib` in `RUSTFLAGS`, and a build machine's absolute
/// path must not end up in a shipped binary.
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

/// The directory of the libraries `source` links: libavcodec's `-L` under
/// pkg-config, the prebuilt libraries' directory otherwise.
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

/// Checks that the `FFmpeg` `rusty_ffmpeg` links is the one the build was
/// pointed at.
///
/// Without `FFMPEG_PKG_CONFIG_PATH` a macOS or Linux build links whatever
/// pkg-config finds (Homebrew's, a distribution's) and succeeds against
/// it; only the tests' pinned-SDK check says so, once they run. That is a
/// cargo warning. A set `FFMPEG_PKG_CONFIG_PATH` is searched before
/// pkg-config's default directories, not instead of them, so one that names
/// a directory without the SDK's `libavcodec.pc` falls through to the same
/// system `FFmpeg` without a word: that stops the build. On a Windows build
/// host, where `rusty_ffmpeg` ignores the variable, setting it is a warning.
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
            // pkg-config's own failure is rusty_ffmpeg's to report.
            if let Some(found) = pc_file_dir(&path)
                && let Some(mismatch) = found_outside(&path, &found)
            {
                panic!("{mismatch}");
            }
        }
        _ => {}
    }
}

/// The directory pkg-config reads `libavcodec.pc` from under
/// `pkg_config_path`.
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

/// A pkg-config variable's value with its escapes removed: pkgconf escapes
/// the spaces of `pcfiledir`, freedesktop pkg-config does not.
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

/// Why `found`, where pkg-config read libavcodec, is not a directory that
/// `pkg_config_path` names; `None` when it is.
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

/// Whether a development rpath belongs on this target: macOS only, and only
/// when the caller's `RUSTFLAGS` do not already carry one.
fn wants_dev_rpath(target_os: &str, encoded_rustflags: &str) -> bool {
    target_os == "macos" && !encoded_rustflags.contains("rpath")
}

/// The linker arguments that add `libdir` as an rpath. `-Wl,` splits at
/// commas, so a libdir with one goes through `-Xlinker` pairs instead.
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
