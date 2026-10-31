//! Finding, verifying and loading the `OpenH264` library.
//!
//! alpha2 ships no H.264 encoder. Cisco's binary is downloaded by the user on
//! request (`npx @alpha2-sh/node install --openh264`, `python -m alpha2 install
//! --openh264`) into the cache the host archives share, and loaded here at
//! run time. Nothing downloads it implicitly.
//!
//! The host looks, in order, at:
//! 1. `ALPHA2_OPENH264_LIBRARY`, a path. Its SHA-256 must be Cisco's for
//!    this target unless `ALPHA2_OPENH264_UNVERIFIED=1` (a distro package
//!    that fetched Cisco's binary itself). A path that is set and fails is an
//!    error, not a reason to look elsewhere.
//! 2. The shared cache, `<cache>/openh264-<version>-<target>/<library>`,
//!    which must hash as Cisco's binary.
//! 3. A system package, reported as provider `system`: the loader's
//!    `libopenh264.so.8` on Linux (`ld.so` never searches the working
//!    directory), and on macOS only absolute paths, Homebrew's and `MacPorts`'
//!    `libopenh264.8.dylib` ([`MACOS_SYSTEM_CANDIDATES`]): `dlopen` of a leaf
//!    name there also searches the working directory. Windows has no such
//!    package.
//!
//! A library that is not hash-verified must still report an API version the
//! FFI was written against. `ALPHA2_OPENH264=0` turns all of it off, which
//! Cisco's licence requires the user be able to do.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use super::ffi;

/// The `OpenH264` release `release/openh264/manifest.toml` pins (`version`).
pub const OPENH264_VERSION: &str = "2.7.0";

/// The text Cisco's binary licence requires wherever the user controls the
/// codec (`notice` of the manifest).
pub const OPENH264_NOTICE: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// What a user runs to install Cisco's binary.
pub const OPENH264_INSTALL_COMMAND: &str =
    "python -m alpha2 install --openh264, or npx @alpha2-sh/node install --openh264";

/// API versions (major, minor) the FFI is written against. `codec_api.h`,
/// `codec_app_def.h` and `codec_def.h` of v2.6.0 and v2.7.0 are identical,
/// and both are the `.8` ABI.
const COMPATIBLE_API: &[(u32, u32)] = &[(2, 6), (2, 7)];

/// This target's file in the manifest (`[targets.*]`): the library name in
/// the cache, its SHA-256, and what the system loader is asked for, in order.
struct PinnedLibrary {
    target: &'static str,
    library: &'static str,
    sha256: &'static str,
    system_candidates: &'static [&'static str],
}

/// Where a macOS system package puts the `.8` library: Homebrew on Apple
/// silicon, Homebrew on Intel, `MacPorts`. Absolute paths only.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]
pub(crate) const MACOS_SYSTEM_CANDIDATES: &[&str] = &[
    "/opt/homebrew/lib/libopenh264.8.dylib",
    "/usr/local/lib/libopenh264.8.dylib",
    "/opt/local/lib/libopenh264.8.dylib",
];

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PINNED: Option<PinnedLibrary> = Some(PinnedLibrary {
    target: "linux-x64",
    library: "libopenh264-2.7.0-linux64.8.so",
    sha256: "1522f873a482923c5ec018a4415e1866898197528828c50f90fd300c94620df3",
    system_candidates: &["libopenh264.so.8"],
});
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const PINNED: Option<PinnedLibrary> = Some(PinnedLibrary {
    target: "linux-arm64",
    library: "libopenh264-2.7.0-linux-arm64.8.so",
    sha256: "9ac664b0154fd0868491c2a904397b8a05779874c0603b8b1f16ad15de1ffd82",
    system_candidates: &["libopenh264.so.8"],
});
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const PINNED: Option<PinnedLibrary> = Some(PinnedLibrary {
    target: "macos-arm64",
    library: "libopenh264-2.7.0-mac-arm64.dylib",
    sha256: "2e1ae3cd10c08328ca03a1f304fd2a1a2a7e5658c6d5a8582101a963f7995ef0",
    system_candidates: MACOS_SYSTEM_CANDIDATES,
});
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const PINNED: Option<PinnedLibrary> = Some(PinnedLibrary {
    target: "win-x64",
    library: "openh264-2.7.0-win64.dll",
    sha256: "0ef5e7f3b38753b15c13afa0418d4d7805fa228e6adfd3e425bbf758cff1c288",
    system_candidates: &[],
});
#[cfg(not(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
const PINNED: Option<PinnedLibrary> = None;

/// Who provided the library that loaded.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum OpenH264Provider {
    /// Cisco's binary, verified by its SHA-256.
    Cisco,
    /// A library the system loader found, or one named with verification
    /// turned off: a distro package, version-checked only.
    System,
}

impl OpenH264Provider {
    /// Stable identifier for `capabilities` and the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cisco => "cisco-openh264",
            Self::System => "system",
        }
    }
}

/// An `OpenH264` library that loaded and passed its checks.
pub struct OpenH264Library {
    pub(crate) api: Arc<ffi::Api>,
    provider: OpenH264Provider,
    version: String,
    path: String,
}

impl std::fmt::Debug for OpenH264Library {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenH264Library")
            .field("provider", &self.provider)
            .field("version", &self.version)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl OpenH264Library {
    /// Who provided it.
    #[must_use]
    pub const fn provider(&self) -> OpenH264Provider {
        self.provider
    }

    /// The version it reports, `major.minor.revision`.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Where it was loaded from: a path, or the name the system loader was
    /// asked for.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Whether `OpenH264` can encode in this process, and if not, why.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum OpenH264Status {
    /// Loaded and checked.
    Available(Arc<OpenH264Library>),
    /// `ALPHA2_OPENH264=0`.
    Disabled,
    /// No library anywhere the host looks; the strings say where it looked.
    NotInstalled(String),
    /// A library was found and refused: the reason names it and why.
    Unusable(String),
}

impl OpenH264Status {
    /// Stable identifier for `capabilities`.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Available(_) => "available",
            Self::Disabled => "disabled",
            Self::NotInstalled(_) => "not-installed",
            Self::Unusable(_) => "unusable",
        }
    }

    /// The library, when one loaded.
    #[must_use]
    pub fn library(&self) -> Option<&Arc<OpenH264Library>> {
        match self {
            Self::Available(library) => Some(library),
            _ => None,
        }
    }

    /// Why it cannot encode, or `None` when it can.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Available(_) => None,
            Self::Disabled => Some("ALPHA2_OPENH264=0 turned OpenH264 off".to_owned()),
            Self::NotInstalled(reason) | Self::Unusable(reason) => Some(reason.clone()),
        }
    }

    /// What the user can do about it, or `None` when it can encode or the
    /// user turned it off.
    #[must_use]
    pub fn remedy(&self) -> Option<String> {
        match self {
            Self::Available(_) | Self::Disabled => None,
            Self::NotInstalled(_) | Self::Unusable(_) if PINNED.is_none() => None,
            Self::NotInstalled(_) => Some(format!(
                "install Cisco's OpenH264 binary: {OPENH264_INSTALL_COMMAND}"
            )),
            Self::Unusable(_) => Some(format!(
                "reinstall Cisco's OpenH264 binary with --force ({OPENH264_INSTALL_COMMAND} \
                 --force), or fix ALPHA2_OPENH264_LIBRARY"
            )),
        }
    }
}

/// `OpenH264` for this process, looked up once: the environment and the cache
/// are read the first time anything asks.
#[must_use]
pub fn openh264_status() -> &'static OpenH264Status {
    static STATUS: OnceLock<OpenH264Status> = OnceLock::new();
    STATUS.get_or_init(|| {
        let status = resolve(&Lookup::from_environment());
        if let OpenH264Status::Unusable(reason) = &status {
            alpha2_os::diagnostic!("alpha2 native: OpenH264 is not used: {reason}");
        }
        status
    })
}

/// Everything a lookup reads from its surroundings, so a test can give its
/// own.
pub(crate) struct Lookup {
    /// `ALPHA2_OPENH264`.
    pub(crate) enabled: Option<String>,
    /// `ALPHA2_OPENH264_LIBRARY`.
    pub(crate) library: Option<PathBuf>,
    /// `ALPHA2_OPENH264_UNVERIFIED`.
    pub(crate) unverified: bool,
    /// The host cache directory, when one can be named.
    pub(crate) cache: Option<PathBuf>,
    /// The system libraries to try, in order: a leaf name goes to the
    /// loader, an absolute path is loaded only when it is a file.
    pub(crate) system: &'static [&'static str],
}

/// This target's system libraries ([`PinnedLibrary::system_candidates`]).
pub(crate) fn system_candidates() -> &'static [&'static str] {
    PINNED
        .as_ref()
        .map_or(&[], |pinned| pinned.system_candidates)
}

impl Lookup {
    fn from_environment() -> Self {
        let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        Self {
            enabled: set("ALPHA2_OPENH264").map(|value| value.to_string_lossy().into_owned()),
            library: set("ALPHA2_OPENH264_LIBRARY").map(PathBuf::from),
            unverified: set("ALPHA2_OPENH264_UNVERIFIED").is_some_and(|value| value == "1"),
            cache: host_cache_directory(),
            system: system_candidates(),
        }
    }
}

/// The directory the host archives and `OpenH264` are installed into, as the
/// Node and Python clients name it: `ALPHA2_HOST_CACHE_DIR` (relative to the
/// working directory), else `alpha2` in `$XDG_CACHE_HOME` (when absolute) or
/// `~/.cache` on Linux, `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on
/// Windows.
pub(crate) fn host_cache_directory() -> Option<PathBuf> {
    let variable = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
    };
    if let Some(cache) = variable("ALPHA2_HOST_CACHE_DIR") {
        return Some(if cache.is_absolute() {
            cache
        } else {
            std::env::current_dir().ok()?.join(cache)
        });
    }
    if cfg!(target_os = "windows") {
        let base = variable("LOCALAPPDATA")
            .or_else(|| variable("USERPROFILE").map(|home| home.join("AppData").join("Local")))?;
        return Some(base.join("alpha2"));
    }
    let home = variable("HOME");
    if cfg!(target_os = "macos") {
        return Some(home?.join("Library").join("Caches").join("alpha2"));
    }
    let base = variable("XDG_CACHE_HOME")
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".cache")))?;
    Some(base.join("alpha2"))
}

/// Where the install puts this target's library under `cache`.
pub(crate) fn cached_library_path(cache: &Path) -> Option<PathBuf> {
    let pinned = PINNED.as_ref()?;
    Some(
        cache
            .join(format!("openh264-{OPENH264_VERSION}-{}", pinned.target))
            .join(pinned.library),
    )
}

/// Looks for `OpenH264` as [the module](self) describes.
pub(crate) fn resolve(lookup: &Lookup) -> OpenH264Status {
    if lookup.enabled.as_deref() == Some("0") {
        return OpenH264Status::Disabled;
    }
    if let Some(path) = &lookup.library {
        return if lookup.unverified {
            load(path, OpenH264Provider::System, None)
        } else {
            match PINNED.as_ref() {
                Some(pinned) => load(path, OpenH264Provider::Cisco, Some(pinned.sha256)),
                None => OpenH264Status::Unusable(format!(
                    "ALPHA2_OPENH264_LIBRARY={}: Cisco publishes no binary alpha2 knows the \
                     SHA-256 of for this platform; set ALPHA2_OPENH264_UNVERIFIED=1 to load it \
                     anyway",
                    path.display()
                )),
            }
        };
    }
    let mut looked = Vec::new();
    if let (Some(cache), Some(pinned)) = (&lookup.cache, PINNED.as_ref())
        && let Some(path) = cached_library_path(cache)
    {
        if path.is_file() {
            return load(&path, OpenH264Provider::Cisco, Some(pinned.sha256));
        }
        looked.push(path.display().to_string());
    }
    for name in lookup.system {
        let path = Path::new(name);
        let shown = if path.is_absolute() {
            (*name).to_owned()
        } else {
            format!("{name} on the loader path")
        };
        if path.is_absolute() && !path.is_file() {
            looked.push(shown);
            continue;
        }
        match ffi::Api::load(path) {
            Ok(api) => return checked(api, OpenH264Provider::System, name),
            Err(ffi::LoadError::Open(_)) => looked.push(shown),
            Err(error) => return OpenH264Status::Unusable(refusal(name, &error)),
        }
    }
    OpenH264Status::NotInstalled(if PINNED.is_none() {
        "Cisco publishes no OpenH264 binary for this platform".to_owned()
    } else if looked.is_empty() {
        "no OpenH264 library was found".to_owned()
    } else {
        format!("no OpenH264 library at {}", looked.join(" or "))
    })
}

/// Loads `path` after checking its SHA-256 against `sha256`, when given.
fn load(path: &Path, provider: OpenH264Provider, sha256: Option<&str>) -> OpenH264Status {
    let shown = path.display().to_string();
    if let Some(expected) = sha256 {
        let actual = match file_sha256(path) {
            Ok(actual) => actual,
            Err(error) => {
                return OpenH264Status::Unusable(format!("{shown} cannot be read: {error}"));
            }
        };
        if actual != expected {
            return OpenH264Status::Unusable(format!(
                "{shown} is not Cisco's OpenH264 {OPENH264_VERSION} binary for this platform \
                 (SHA-256 {actual}, expected {expected})"
            ));
        }
    }
    match ffi::Api::load(path) {
        Ok(api) => checked(api, provider, &shown),
        Err(error) => OpenH264Status::Unusable(refusal(&shown, &error) + &quarantine_note(path)),
    }
}

/// What a refusal adds for a library macOS quarantined: Gatekeeper refuses to
/// load an unnotarized one, and the attribute is what to remove. Empty
/// elsewhere.
fn quarantine_note(path: &Path) -> String {
    if !alpha2_os::quarantined(path) {
        return String::new();
    }
    format!(
        "; macOS quarantined it (a browser download), and Gatekeeper refuses an unnotarized \
         library: remove the attribute (xattr -d com.apple.quarantine '{}') or install it with \
         python -m alpha2 install --openh264 or npx @alpha2-sh/node install --openh264",
        path.display()
    )
}

/// Accepts a loaded library whose API version the FFI was written against.
fn checked(api: ffi::Api, provider: OpenH264Provider, shown: &str) -> OpenH264Status {
    let version = api.version();
    if !COMPATIBLE_API.contains(&(version.major, version.minor)) {
        return OpenH264Status::Unusable(format!(
            "{shown} is OpenH264 {}.{}.{}, and alpha2 is built against the 2.6/2.7 API",
            version.major, version.minor, version.revision
        ));
    }
    OpenH264Status::Available(Arc::new(OpenH264Library {
        api: Arc::new(api),
        provider,
        version: format!("{}.{}.{}", version.major, version.minor, version.revision),
        path: shown.to_owned(),
    }))
}

fn refusal(shown: &str, error: &ffi::LoadError) -> String {
    match error {
        ffi::LoadError::Open(message) => {
            format!("{shown} was found but is not loadable: {message}")
        }
        ffi::LoadError::Symbol(symbol, message) => {
            format!("{shown} is not an OpenH264 encoder library (no {symbol}: {message})")
        }
    }
}

/// The SHA-256 of the file at `path`, in lower-case hex.
pub(crate) fn file_sha256(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}
