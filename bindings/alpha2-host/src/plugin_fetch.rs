//! Fetches plugin files into a cache keyed by SHA-256. A manifest is named by `https:` URL,
//! `file:` URL or path, and names its artifacts and resources by paths under itself or `https:`
//! URLs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use alpha2_engine::extract::{MAX_MANIFEST_BYTES, Reference, reference};
use alpha2_host_core::plugins::{Fetch, Fetched, PluginError, codes, sha256_hex};

/// How long one download may take, connecting included.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Where a manifest came from, which its relative URLs resolve against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// An `https:` URL.
    Url(String),
    /// A local directory.
    Directory(PathBuf),
}

/// One file location: fetched over HTTPS, or read locally.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Location {
    Url(String),
    Path(PathBuf),
}

fn fetch_failed(message: impl Into<String>) -> PluginError {
    PluginError::new(codes::FETCH_FAILED, message)
}

/// A `file:` URL's path, percent-decoded, without the slash before a Windows drive letter.
fn file_url_path(url: &str) -> Result<PathBuf, PluginError> {
    let rest = url
        .strip_prefix("file://")
        .ok_or_else(|| fetch_failed(format!("{url}: not a file URL")))?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return Err(fetch_failed(format!(
            "{url}: a file URL names a local absolute path"
        )));
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex = [iter.next(), iter.next()];
            let digit = |b: Option<u8>| b.and_then(|b| char::from(b).to_digit(16));
            match (digit(hex[0]), digit(hex[1])) {
                (Some(high), Some(low)) => bytes.push(u8::try_from(high * 16 + low).unwrap_or(0)),
                _ => return Err(fetch_failed(format!("{url}: a malformed percent escape"))),
            }
        } else {
            bytes.push(byte);
        }
    }
    let path = String::from_utf8(bytes).map_err(|_| fetch_failed(format!("{url}: not UTF-8")))?;
    let windows_drive = path.len() > 2 && path.as_bytes()[2] == b':' && cfg!(windows);
    Ok(PathBuf::from(if windows_drive {
        &path[1..]
    } else {
        &path
    }))
}

fn has_scheme(url: &str) -> Option<&str> {
    let (scheme, _) = url.split_once(':')?;
    let valid = scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// Resolves `reference` against an `https:` base as RFC 3986 does for paths: an absolute path
/// replaces the base's, a relative one replaces its last segment, and dot segments are removed.
fn join_url(base: &str, reference: &str) -> String {
    let after_scheme = base.find("://").map_or(0, |at| at + 3);
    let path_start = base[after_scheme..]
        .find('/')
        .map_or(base.len(), |at| after_scheme + at);
    let authority = &base[..path_start];
    let base_path = base[path_start..].split(['?', '#']).next().unwrap_or("");
    let (reference_path, suffix) = match reference.find(['?', '#']) {
        Some(at) => (&reference[..at], &reference[at..]),
        None => (reference, ""),
    };
    let merged = if reference_path.starts_with('/') {
        reference_path.to_owned()
    } else {
        let directory = base_path.rfind('/').map_or("/", |at| &base_path[..=at]);
        format!("{directory}{reference_path}")
    };
    let mut segments: Vec<&str> = Vec::new();
    let ends_in_directory =
        merged.ends_with("/.") || merged.ends_with("/..") || merged.ends_with('/');
    for segment in merged.split('/').skip(1) {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut path = format!("/{}", segments.join("/"));
    if ends_in_directory && !path.ends_with('/') {
        path.push('/');
    }
    format!("{authority}{path}{suffix}")
}

/// Where the request's manifest is: an `https:` URL, a `file:` URL or a path.
fn manifest_location(location: &str) -> Result<Location, PluginError> {
    match has_scheme(location) {
        Some("https") => Ok(Location::Url(location.to_owned())),
        Some("file") => file_url_path(location).map(Location::Path),
        // A drive letter, `C:\...`, is a path.
        Some(scheme) if scheme.len() > 1 => Err(fetch_failed(format!(
            "{location}: only https: and file: URLs are fetched"
        ))),
        _ => Ok(Location::Path(PathBuf::from(location))),
    }
}

impl Base {
    /// Where a manifest's `url` points: under the manifest's own location, or an `https:` URL.
    fn resolve(&self, url: &str) -> Result<Location, PluginError> {
        match reference(url).map_err(|why| PluginError::new(codes::LOAD_FAILED, why))? {
            Reference::Https(url) => Ok(Location::Url(url.to_owned())),
            Reference::Relative(path) => Ok(match self {
                Base::Url(base) => Location::Url(join_url(base, path)),
                Base::Directory(directory) => Location::Path(directory.join(path)),
            }),
        }
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    })
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>, PluginError> {
    let mut response = agent()
        .get(url)
        .call()
        .map_err(|error| fetch_failed(format!("{url}: {error}")))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| fetch_failed(format!("{url}: {error}")))
}

/// The manifest at `location` (an `https:` URL, a `file:` URL or a path), checked against
/// `sha256`, and the base its relative URLs resolve against.
///
/// # Errors
/// `PLUGIN_FETCH_FAILED` when it cannot be read, `PLUGIN_HASH_MISMATCH` when its hash differs.
pub fn manifest(location: &str, sha256: &str) -> Result<(String, Base), PluginError> {
    let limit = MAX_MANIFEST_BYTES as u64;
    let (bytes, base) = match manifest_location(location)? {
        Location::Url(url) => (download(&url, limit)?, Base::Url(url)),
        Location::Path(path) => {
            let bytes = std::fs::read(&path)
                .map_err(|error| fetch_failed(format!("{}: {error}", path.display())))?;
            let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
            (bytes, Base::Directory(directory))
        }
    };
    let found = sha256_hex(&bytes);
    if found != sha256.to_ascii_lowercase() {
        let message = match base {
            Base::Directory(_) => {
                format!("the manifest has SHA-256 {found}; the request pins {sha256}")
            }
            Base::Url(_) => {
                format!("the manifest at {location} does not match the SHA-256 the request pins")
            }
        };
        return Err(PluginError::new(codes::HASH_MISMATCH, message));
    }
    let text = String::from_utf8(bytes).map_err(|_| fetch_failed("the manifest is not UTF-8"))?;
    Ok((text, base))
}

/// Where fetched plugin files are kept, `ALPHA2_PLUGIN_CACHE` or else the platform's cache.
#[must_use]
pub fn cache_directory() -> PathBuf {
    if let Some(directory) =
        std::env::var_os("ALPHA2_PLUGIN_CACHE").filter(|value| !value.is_empty())
    {
        return PathBuf::from(directory);
    }
    let home = || std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    let root = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home().join(".cache"), PathBuf::from)
    };
    root.join("alpha2").join("plugins")
}

/// Fetches a manifest's files: local ones are read in place, `https:` ones downloaded once into
/// the cache under their hash.
pub struct Fetcher {
    base: Base,
    cache: PathBuf,
}

impl Fetcher {
    /// Resolves relative URLs against `base` and keeps downloads in `cache`.
    #[must_use]
    pub fn new(base: Base, cache: PathBuf) -> Self {
        Self { base, cache }
    }

    fn cached(&self, url: &str, sha256: &str, size: u64) -> Result<PathBuf, PluginError> {
        // The extension stays: Windows appends `.dll` to a library name without one.
        let name = url.rsplit('/').next().unwrap_or_default();
        let extension = Path::new(name.split(['?', '#']).next().unwrap_or_default())
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| extension.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        let file = self.cache.join(extension.map_or_else(
            || sha256.to_owned(),
            |extension| format!("{sha256}.{extension}"),
        ));
        if std::fs::read(&file).is_ok_and(|bytes| sha256_hex(&bytes) == sha256) {
            return Ok(file);
        }
        let bytes = download(url, size)?;
        std::fs::create_dir_all(&self.cache)
            .map_err(|error| fetch_failed(format!("{}: {error}", self.cache.display())))?;
        // Written beside the target and renamed, so a reader never sees half a file.
        let partial = self
            .cache
            .join(format!("{sha256}.{}.partial", std::process::id()));
        let written = std::fs::File::create(&partial)
            .and_then(|mut out| out.write_all(&bytes).and_then(|()| out.sync_all()))
            .and_then(|()| std::fs::rename(&partial, &file));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&partial);
            return Err(fetch_failed(format!("{}: {error}", file.display())));
        }
        Ok(file)
    }
}

impl Fetch for Fetcher {
    fn fetch(&mut self, url: &str, sha256: &str, bytes: u64) -> Result<Fetched, PluginError> {
        Ok(match self.base.resolve(url)? {
            Location::Path(path) => Fetched { path, local: true },
            Location::Url(url) => Fetched {
                path: self.cached(&url, sha256, bytes)?,
                local: false,
            },
        })
    }

    #[cfg(target_os = "macos")]
    fn prepare_native(&mut self, verified: &Path) -> Result<PathBuf, PluginError> {
        // A downloaded library is signed ad hoc before it loads; the copy keeps the original's
        // bytes, which the manifest pins, untouched.
        let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
        let name = verified
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("plugin.dylib");
        let signed = self.cache.join("signed").join(name);
        std::fs::create_dir_all(signed.parent().unwrap_or(&self.cache))
            .and_then(|()| std::fs::copy(verified, &signed))
            .map_err(|error| failed(format!("{}: {error}", signed.display())))?;
        let status = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&signed)
            .status()
            .map_err(|error| failed(format!("codesign: {error}")))?;
        if !status.success() {
            return Err(failed(format!("codesign failed for {}", signed.display())));
        }
        Ok(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls_resolve_against_the_manifest() {
        let base = "https://example.com/plugins/silence/manifest.json?v=1";
        assert_eq!(
            join_url(base, "silence.wasm"),
            "https://example.com/plugins/silence/silence.wasm"
        );
        assert_eq!(
            join_url(base, "x86_64/lib.so"),
            "https://example.com/plugins/silence/x86_64/lib.so"
        );
        assert_eq!(
            join_url(base, "../models/vad.onnx"),
            "https://example.com/plugins/models/vad.onnx"
        );
        assert_eq!(
            join_url(base, "/root.wasm?x#y"),
            "https://example.com/root.wasm?x#y"
        );
        assert_eq!(
            join_url("https://example.com", "a.wasm"),
            "https://example.com/a.wasm"
        );
        assert_eq!(
            join_url(base, "./a/../b/./c.wasm"),
            "https://example.com/plugins/silence/b/c.wasm"
        );
    }

    #[test]
    fn a_manifest_names_https_urls_and_paths_under_itself() {
        let remote = Base::Url("https://example.com/p/manifest.json".to_owned());
        assert_eq!(
            remote.resolve("https://cdn.example.com/a.wasm"),
            Ok(Location::Url("https://cdn.example.com/a.wasm".to_owned()))
        );
        assert_eq!(
            remote.resolve("a.wasm"),
            Ok(Location::Url("https://example.com/p/a.wasm".to_owned()))
        );
        let local = Base::Directory(PathBuf::from("/plugins/p"));
        assert_eq!(
            local.resolve("x/a.wasm"),
            Ok(Location::Path(PathBuf::from("/plugins/p/x/a.wasm")))
        );
        for base in [remote, local] {
            for refused in [
                "file:///etc/hostname",
                "/etc/hostname",
                "../a.wasm",
                "http://example.com/a.wasm",
                "data:text/plain,x",
            ] {
                assert_eq!(
                    base.resolve(refused).expect_err(refused).code,
                    codes::LOAD_FAILED
                );
            }
        }
    }

    #[test]
    fn a_request_names_its_manifest_by_https_or_file_url_or_path() {
        assert_eq!(
            manifest_location("https://example.com/p/manifest.json"),
            Ok(Location::Url(
                "https://example.com/p/manifest.json".to_owned()
            ))
        );
        assert_eq!(
            manifest_location("file:///tmp/a%20b/manifest.json"),
            Ok(Location::Path(PathBuf::from("/tmp/a b/manifest.json")))
        );
        assert_eq!(
            manifest_location("p/manifest.json"),
            Ok(Location::Path(PathBuf::from("p/manifest.json")))
        );
        for refused in [
            "http://example.com/manifest.json",
            "ftp://example.com/m",
            "file://server/share/manifest.json",
        ] {
            assert_eq!(
                manifest_location(refused).expect_err(refused).code,
                codes::FETCH_FAILED
            );
        }
    }

    #[test]
    fn a_manifest_is_checked_against_its_pinned_hash() {
        let dir =
            std::env::temp_dir().join(format!("alpha2-plugin-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("manifest.json");
        std::fs::write(&path, b"{}").expect("manifest");
        let pinned = sha256_hex(b"{}");
        let (text, base) = manifest(path.to_str().expect("utf-8"), &pinned).expect("read");
        assert_eq!((text.as_str(), base), ("{}", Base::Directory(dir.clone())));
        let wrong = manifest(path.to_str().expect("utf-8"), &"0".repeat(64)).expect_err("refused");
        assert_eq!(wrong.code, codes::HASH_MISMATCH);
        let missing =
            manifest(dir.join("none.json").to_str().expect("utf-8"), &pinned).expect_err("refused");
        assert_eq!(missing.code, codes::FETCH_FAILED);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
