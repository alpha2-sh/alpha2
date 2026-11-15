//! Fetches plugin files into a private cache keyed by SHA-256. A manifest is named by `https:`
//! URL, `file:` URL or path, and names its artifacts and resources by paths under itself or
//! `https:` URLs.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use alpha2_engine::extract::{MAX_MANIFEST_BYTES, Reference, reference};
use alpha2_host_core::plugins::{Fetch, Fetched, PluginError, codes, sha256_hex};
use sha2::{Digest, Sha256};

use crate::plugin_policy::{Origin, PluginPolicy};

/// How long one download may take, connecting included.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// How long connecting and waiting for the response head may each take; a cancel is seen only
/// between body chunks, so this bounds how late it is seen before the body starts.
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// The cache size past which least recently used files are removed, when
/// `ALPHA2_PLUGIN_CACHE_QUOTA` names none.
const DEFAULT_CACHE_QUOTA: u64 = 4 * 1024 * 1024 * 1024;
/// Files used this recently are never evicted, so a host sharing the cache keeps a file between
/// its check and its read.
const EVICTION_GRACE: Duration = Duration::from_secs(3600);
/// Numbers partial files, so request threads of one host never write the same one.
static PARTIALS: AtomicU64 = AtomicU64::new(0);
/// Redirect hops one download follows at most.
const MAX_REDIRECTS: usize = 5;
/// Bytes read from a body or a file at a time.
const CHUNK_BYTES: usize = 64 * 1024;

/// What one GET answered.
pub enum Answer {
    /// The body.
    Body(Box<dyn Read + Send>),
    /// A redirect to this `Location`, not followed.
    Redirect(String),
}

/// Starts HTTPS downloads; production uses ureq, tests script the answers.
pub trait Download: Send + Sync {
    /// One GET of `url`, without following a redirect.
    ///
    /// # Errors
    /// Why the request failed, without the URL.
    fn get(&self, url: &str) -> Result<Answer, String>;
}

struct Https;

impl Download for Https {
    fn get(&self, url: &str) -> Result<Answer, String> {
        let response = agent()
            .get(url)
            .call()
            .map_err(|error| redacted(url, &error.to_string()))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| format!("a {} redirect without a Location", response.status()))?;
            return Ok(Answer::Redirect(location.to_owned()));
        }
        Ok(Answer::Body(Box::new(response.into_body().into_reader())))
    }
}

/// Downloads over HTTPS through the platform's certificate verifier.
#[must_use]
pub fn https() -> Arc<dyn Download> {
    Arc::new(Https)
}

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

/// `url` as a message shows it: scheme, host, port and path. User info, query and fragment are
/// left out, since they may carry credentials (a signed URL's token). A path is shown whole.
pub(crate) fn shown_url(url: &str) -> String {
    if has_scheme(url).is_none() {
        return url.to_owned();
    }
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

/// A message about `url` from a library that may repeat it, with the URL shown as [`shown_url`].
fn redacted(url: &str, message: &str) -> String {
    message.replace(url, &shown_url(url))
}

/// A `file:` URL's path, percent-decoded, without the slash before a Windows drive letter.
fn file_url_path(url: &str) -> Result<PathBuf, PluginError> {
    let shown = || shown_url(url);
    let rest = url
        .strip_prefix("file://")
        .ok_or_else(|| fetch_failed(format!("{}: not a file URL", shown())))?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return Err(fetch_failed(format!(
            "{}: a file URL names a local absolute path",
            shown()
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
                _ => {
                    return Err(fetch_failed(format!(
                        "{}: a malformed percent escape",
                        shown()
                    )));
                }
            }
        } else {
            bytes.push(byte);
        }
    }
    let path =
        String::from_utf8(bytes).map_err(|_| fetch_failed(format!("{}: not UTF-8", shown())))?;
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
            "{}: only https: and file: URLs are fetched",
            shown_url(location)
        ))),
        _ => Ok(Location::Path(PathBuf::from(location))),
    }
}

impl Base {
    /// Where a manifest's `url` points: under the manifest's own location, or an `https:` URL.
    fn resolve(&self, url: &str) -> Result<Location, PluginError> {
        let refused = |why: String| PluginError::new(codes::LOAD_FAILED, redacted(url, &why));
        match reference(url).map_err(refused)? {
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
            .max_redirects(0)
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .timeout_connect(Some(WAIT_TIMEOUT))
            .timeout_recv_response(Some(WAIT_TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    })
}

/// Where a `Location` points, resolved against the URL that answered with it.
fn redirect_target(from: &str, location: &str) -> String {
    if location.starts_with("//") {
        format!("https:{location}")
    } else if has_scheme(location).is_some() {
        location.to_owned()
    } else {
        join_url(from, location)
    }
}

/// The body at `url`, following at most [`MAX_REDIRECTS`] redirects, each target only when
/// `policy` allows its origin for a manifest from `manifest`; nothing is contacted before its
/// check.
fn open(
    download: &dyn Download,
    url: &str,
    policy: &PluginPolicy,
    manifest: Option<&Origin>,
) -> Result<Box<dyn Read + Send>, PluginError> {
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let origin = Origin::of_url(&current);
        if !origin
            .as_ref()
            .is_some_and(|origin| policy.allows_origin(origin, manifest))
        {
            return Err(policy.origin_refusal(&shown_url(&current), origin.as_ref()));
        }
        match download.get(&current).map_err(|error| {
            fetch_failed(format!(
                "{}: {}",
                shown_url(&current),
                redacted(&current, &error)
            ))
        })? {
            Answer::Body(body) => return Ok(body),
            Answer::Redirect(location) => current = redirect_target(&current, &location),
        }
    }
    Err(fetch_failed(format!(
        "{}: more than {MAX_REDIRECTS} redirects",
        shown_url(url)
    )))
}

/// Reads at most `limit` bytes of `reader`; `None` when there are more.
fn read_bounded(reader: impl Read, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The manifest at `location` (an `https:` URL, a `file:` URL or a path), checked against
/// `sha256`, and the base its relative URLs resolve against: the requested URL, also after a
/// redirect.
///
/// # Errors
/// `PLUGIN_REFUSED` when `policy` refuses the location, checked before anything is read or
/// contacted; `PLUGIN_FETCH_FAILED` when it cannot be read, is not a regular file or is longer than
/// `MAX_MANIFEST_BYTES`; `PLUGIN_HASH_MISMATCH` when its hash differs.
pub fn manifest(
    location: &str,
    sha256: &str,
    download: &dyn Download,
    policy: &PluginPolicy,
) -> Result<(String, Base), PluginError> {
    let limit = MAX_MANIFEST_BYTES as u64;
    let too_long = |what: &str| fetch_failed(format!("{what}: longer than {limit} bytes"));
    let (bytes, base) = match manifest_location(location)? {
        Location::Url(url) => {
            let origin = policy.check_manifest_url(&url)?;
            let body = open(download, &url, policy, Some(&origin))?;
            let bytes = read_bounded(body, limit)
                .map_err(|error| fetch_failed(format!("{}: {error}", shown_url(&url))))?
                .ok_or_else(|| too_long(&shown_url(&url)))?;
            (bytes, Base::Url(url))
        }
        Location::Path(path) => {
            policy.check_manifest_path(&path)?;
            let failed =
                |error: std::io::Error| fetch_failed(format!("{}: {error}", path.display()));
            let file = File::open(&path).map_err(failed)?;
            if !file.metadata().map_err(failed)?.is_file() {
                return Err(fetch_failed(format!(
                    "{}: not a regular file",
                    path.display()
                )));
            }
            let bytes = read_bounded(file, limit)
                .map_err(failed)?
                .ok_or_else(|| too_long(&path.display().to_string()))?;
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
            Base::Url(_) => format!(
                "the manifest at {} does not match the SHA-256 the request pins",
                shown_url(location)
            ),
        };
        return Err(PluginError::new(codes::HASH_MISMATCH, message));
    }
    let text = String::from_utf8(bytes).map_err(|_| fetch_failed("the manifest is not UTF-8"))?;
    Ok((text, base))
}

/// Where downloaded plugin files are kept: `ALPHA2_PLUGIN_CACHE` (relative to the working
/// directory), else `plugins` under the host's cache directory; `None` without either.
#[must_use]
pub fn cache_directory() -> Option<PathBuf> {
    cache_directory_from(|name| std::env::var_os(name))
}

fn cache_directory_from(variable: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    match variable("ALPHA2_PLUGIN_CACHE").filter(|value| !value.is_empty()) {
        Some(directory) => std::path::absolute(PathBuf::from(directory)).ok(),
        None => alpha2_os::host_cache_directory_from(variable).map(|cache| cache.join("plugins")),
    }
}

/// The cache size past which files are evicted: `ALPHA2_PLUGIN_CACHE_QUOTA` in bytes, 4 GiB when
/// unset or unreadable.
fn cache_quota() -> u64 {
    std::env::var("ALPHA2_PLUGIN_CACHE_QUOTA")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_CACHE_QUOTA)
}

/// Whether `file` holds the `size` bytes with the pinned `sha256`.
fn holds(file: &Path, sha256: &str, size: u64) -> bool {
    let Ok(file) = File::open(file) else {
        return false;
    };
    if file
        .metadata()
        .map_or(true, |metadata| metadata.len() != size)
    {
        return false;
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK_BYTES];
    let mut reader = file.take(size.saturating_add(1));
    let mut read = 0_u64;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                read += n as u64;
                hasher.update(&buffer[..n]);
            }
            Err(_) => return false,
        }
    }
    read == size && hex(&hasher.finalize()) == sha256
}

fn hex(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// Marks `file` as used now, which eviction reads; a failure only makes it older.
fn touch(file: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(file) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Removes the least recently used files of `directory` until it holds at most `quota` bytes,
/// keeping files used within [`EVICTION_GRACE`] of `now`.
fn evict(directory: &Path, quota: u64, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((metadata.modified().ok()?, metadata.len(), entry.path()))
        })
        .collect();
    let mut total = files
        .iter()
        .fold(0_u64, |total, file| total.saturating_add(file.1));
    if total <= quota {
        return;
    }
    files.sort();
    for (modified, length, path) in files {
        if total <= quota {
            break;
        }
        let recent = now
            .duration_since(modified)
            .map_or(true, |age| age < EVICTION_GRACE);
        if !recent && std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(length);
        }
    }
}

/// Fetches a manifest's files: local ones are read in place, `https:` ones downloaded once into
/// the cache under their hash.
pub struct Fetcher {
    base: Base,
    /// The manifest's origin; `None` for a local manifest.
    origin: Option<Origin>,
    cache: Option<PathBuf>,
    quota: u64,
    download: Arc<dyn Download>,
    policy: &'static PluginPolicy,
    cancel: alpha2_media::CancelToken,
}

impl Fetcher {
    /// Resolves relative URLs against `base`, fetches from origins `policy` allows and keeps
    /// downloads in `cache`, made private.
    #[must_use]
    pub fn new(
        base: Base,
        cache: Option<PathBuf>,
        download: Arc<dyn Download>,
        policy: &'static PluginPolicy,
    ) -> Self {
        let origin = match &base {
            Base::Url(url) => Origin::of_url(url),
            Base::Directory(_) => None,
        };
        Self {
            base,
            origin,
            cache,
            quota: cache_quota(),
            download,
            policy,
            cancel: alpha2_media::CancelToken::default(),
        }
    }

    /// Stops a download once `cancel` is set.
    #[must_use]
    pub fn with_cancel(mut self, cancel: alpha2_media::CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// The cache directory, created or checked private.
    fn private_cache(&self) -> Result<&Path, PluginError> {
        let cache = self.cache.as_deref().ok_or_else(|| {
            fetch_failed(
                "no cache directory for downloaded plugin files: set ALPHA2_PLUGIN_CACHE to a \
                 directory only this user can write",
            )
        })?;
        alpha2_os::create_private_directory(cache).map_err(|error| {
            fetch_failed(format!(
                "{}: {error}; set ALPHA2_PLUGIN_CACHE to a directory only this user can write",
                cache.display()
            ))
        })?;
        Ok(cache)
    }

    fn cached(&self, url: &str, sha256: &str, size: u64) -> Result<PathBuf, PluginError> {
        let cache = self.private_cache()?;
        // The extension stays: Windows appends `.dll` to a library name without one.
        let name = url.rsplit('/').next().unwrap_or_default();
        let extension = Path::new(name.split(['?', '#']).next().unwrap_or_default())
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| extension.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        let file = cache.join(extension.map_or_else(
            || sha256.to_owned(),
            |extension| format!("{sha256}.{extension}"),
        ));
        if holds(&file, sha256, size) {
            touch(&file);
            return Ok(file);
        }
        let partial = cache.join(format!(
            "{sha256}.{}.{}.partial",
            std::process::id(),
            PARTIALS.fetch_add(1, Ordering::Relaxed)
        ));
        let written =
            self.download_into(url, &partial, sha256, size, &|| self.cancel.is_cancelled());
        let published = written.and_then(|()| match std::fs::rename(&partial, &file) {
            Ok(()) => Ok(()),
            Err(_) if holds(&file, sha256, size) => Ok(()),
            Err(error) => Err(fetch_failed(format!("{}: {error}", file.display()))),
        });
        if published.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        published?;
        evict(cache, self.quota, SystemTime::now());
        Ok(file)
    }

    /// Streams `url` into a new file at `partial`, which holds the pinned bytes when this returns
    /// `Ok`. Stops once more than `size` bytes arrive, or when `should_stop` says so.
    fn download_into(
        &self,
        url: &str,
        partial: &Path,
        sha256: &str,
        size: u64,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<(), PluginError> {
        let failed =
            |error: String| fetch_failed(format!("{}: {}", shown_url(url), redacted(url, &error)));
        let mismatch = || {
            PluginError::new(
                codes::HASH_MISMATCH,
                format!(
                    "{} does not match the length and SHA-256 the manifest pins",
                    shown_url(url)
                ),
            )
        };
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)
            .map_err(|error| fetch_failed(format!("{}: {error}", partial.display())))?;
        let mut body = open(&*self.download, url, self.policy, self.origin.as_ref())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; CHUNK_BYTES];
        let mut received = 0_u64;
        loop {
            if should_stop() {
                return Err(PluginError::new(
                    alpha2_host_core::plugins::CANCELLED,
                    format!("the download of {} was cancelled", shown_url(url)),
                ));
            }
            let n = match body.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(failed(error.to_string())),
            };
            received += n as u64;
            if received > size {
                return Err(mismatch());
            }
            hasher.update(&buffer[..n]);
            out.write_all(&buffer[..n])
                .map_err(|error| fetch_failed(format!("{}: {error}", partial.display())))?;
        }
        if received != size || hex(&hasher.finalize()) != sha256 {
            return Err(mismatch());
        }
        out.sync_all()
            .map_err(|error| fetch_failed(format!("{}: {error}", partial.display())))
    }
}

impl Fetch for Fetcher {
    fn fetch(&mut self, url: &str, sha256: &str, bytes: u64) -> Result<Fetched, PluginError> {
        Ok(match self.base.resolve(url)? {
            Location::Path(path) => Fetched { path, local: true },
            Location::Url(url) => {
                let origin = Origin::of_url(&url);
                if !origin
                    .as_ref()
                    .is_some_and(|origin| self.policy.allows_origin(origin, self.origin.as_ref()))
                {
                    return Err(self
                        .policy
                        .origin_refusal(&shown_url(&url), origin.as_ref()));
                }
                Fetched {
                    path: self.cached(&url, sha256, bytes)?,
                    local: false,
                }
            }
        })
    }

    /// Loads from `<cache>/native/<sha256>.<extension>`, written from the checked bytes. An existing
    /// copy is never opened for writing: Windows refuses that while another host has it loaded, and
    /// a mapped file must not change.
    fn prepare_native(
        &mut self,
        verified: &Path,
        bytes: &[u8],
        sha256: &str,
    ) -> Result<PathBuf, PluginError> {
        let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
        let directory = self
            .private_cache()
            .map_err(|error| failed(error.message))?
            .join("native");
        alpha2_os::create_private_directory(&directory)
            .map_err(|error| failed(format!("{}: {error}", directory.display())))?;
        let extension = verified
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            .unwrap_or(std::env::consts::DLL_EXTENSION);
        let file = directory.join(format!("{sha256}.{extension}"));
        let size = bytes.len() as u64;
        if usable_native_copy(&file, sha256, size) {
            return Ok(file);
        }
        let partial = directory.join(format!(
            "{sha256}.{}.{}.partial",
            std::process::id(),
            PARTIALS.fetch_add(1, Ordering::Relaxed)
        ));
        let placed = write_native_copy(&partial, bytes).and_then(|()| {
            match std::fs::rename(&partial, &file) {
                Ok(()) => Ok(()),
                Err(_) if usable_native_copy(&file, sha256, size) => Ok(()),
                Err(error) => Err(format!("{}: {error}", file.display())),
            }
        });
        if placed.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        placed.map_err(failed)?;
        Ok(file)
    }
}

/// Whether the copy at `file` may load: it holds the pinned bytes, or on macOS, where signing
/// changes them, its ad hoc signature verifies.
fn usable_native_copy(file: &Path, sha256: &str, size: u64) -> bool {
    if cfg!(target_os = "macos") {
        file.is_file()
            && std::process::Command::new("/usr/bin/codesign")
                .arg("--verify")
                .arg(file)
                .status()
                .is_ok_and(|status| status.success())
    } else {
        holds(file, sha256, size)
    }
}

/// Writes `bytes` to the new file `partial` and syncs it; on macOS also signs it ad hoc, which
/// the loader requires of a library that is not notarized.
fn write_native_copy(partial: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial)
        .and_then(|mut out| out.write_all(bytes).and_then(|()| out.sync_all()))
        .map_err(|error| format!("{}: {error}", partial.display()))?;
    if cfg!(target_os = "macos") {
        let status = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(partial)
            .status()
            .map_err(|error| format!("codesign: {error}"))?;
        if !status.success() {
            return Err(format!("codesign failed for {}", partial.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A directory removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "alpha2-plugin-fetch-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    use crate::plugin_policy::DEFAULT_POLICY;
    use std::sync::Mutex;

    /// Answers each URL with fixed bytes, or a redirect when they start with `->`, and records
    /// every URL it was asked for.
    #[derive(Default)]
    struct Scripted {
        answers: HashMap<String, Vec<u8>>,
        asked: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: &[(&str, &[u8])]) -> Self {
            Self {
                answers: answers
                    .iter()
                    .map(|(url, body)| ((*url).to_owned(), body.to_vec()))
                    .collect(),
                asked: Mutex::default(),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("asked").clone()
        }
    }

    impl Download for Scripted {
        fn get(&self, url: &str) -> Result<Answer, String> {
            self.asked.lock().expect("asked").push(url.to_owned());
            let body = self.answers.get(url).ok_or("404 Not Found")?.clone();
            Ok(match body.strip_prefix(b"->") {
                Some(location) => {
                    Answer::Redirect(String::from_utf8(location.to_vec()).expect("utf-8"))
                }
                None => Answer::Body(Box::new(std::io::Cursor::new(body))),
            })
        }
    }

    const URL: &str = "https://example.com/p/a.wasm";
    const MANIFEST: &str = "https://example.com/p/manifest.json";

    fn fetcher(cache: &Path, body: &[u8]) -> Fetcher {
        Fetcher::new(
            Base::Url(MANIFEST.to_owned()),
            Some(cache.to_path_buf()),
            Arc::new(Scripted::new(&[(URL, body)])),
            &DEFAULT_POLICY,
        )
    }

    fn policy(flags: &[(&str, &str)]) -> &'static PluginPolicy {
        let mut policy = PluginPolicy::DEFAULT;
        for (flag, value) in flags {
            match *flag {
                "--plugin-origins" => policy.set_origins(value),
                "--plugin-root" => policy.set_root(value),
                _ => policy.set_mode(value),
            }
            .expect("flag");
        }
        Box::leak(Box::new(policy))
    }

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .map(|entries| {
                entries
                    .filter_map(|entry| Some(entry.ok()?.file_name().to_str()?.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn a_download_is_cached_under_its_hash_once_its_bytes_match() {
        let cache = Scratch::new("hit");
        let body = b"module bytes";
        let pinned = sha256_hex(body);
        let fetched = fetcher(&cache.0, body)
            .fetch("a.wasm", &pinned, body.len() as u64)
            .expect("fetched");
        assert_eq!(fetched.path, cache.0.join(format!("{pinned}.wasm")));
        assert!(!fetched.local);
        assert_eq!(names(&cache.0), [format!("{pinned}.wasm")]);
        // A hit needs no download.
        let again = fetcher(&cache.0, b"")
            .fetch("a.wasm", &pinned, body.len() as u64)
            .expect("hit");
        assert_eq!(again.path, fetched.path);
    }

    #[test]
    fn a_body_longer_than_pinned_or_with_another_digest_leaves_nothing() {
        let body = b"module bytes";
        let pinned = sha256_hex(body);
        for (served, size) in [
            (b"module bytes and more".as_slice(), body.len() as u64),
            (b"module_bytes".as_slice(), body.len() as u64),
            (b"module".as_slice(), body.len() as u64),
        ] {
            let cache = Scratch::new("mismatch");
            let error = fetcher(&cache.0, served)
                .fetch("a.wasm", &pinned, size)
                .expect_err("mismatch");
            assert_eq!(error.code, codes::HASH_MISMATCH);
            assert_eq!(
                error.message,
                format!("{URL} does not match the length and SHA-256 the manifest pins")
            );
            assert_eq!(names(&cache.0), Vec::<String>::new());
        }
    }

    #[test]
    fn a_body_that_never_ends_stops_past_the_pinned_length() {
        struct Endless;
        impl Download for Endless {
            fn get(&self, _: &str) -> Result<Answer, String> {
                Ok(Answer::Body(Box::new(std::io::repeat(7))))
            }
        }
        let cache = Scratch::new("endless");
        let mut fetcher = Fetcher::new(
            Base::Url(MANIFEST.to_owned()),
            Some(cache.0.clone()),
            Arc::new(Endless),
            &DEFAULT_POLICY,
        );
        let error = fetcher
            .fetch("a.wasm", &"0".repeat(64), 1 << 20)
            .expect_err("mismatch");
        assert_eq!(error.code, codes::HASH_MISMATCH);
        assert_eq!(names(&cache.0), Vec::<String>::new());
    }

    #[test]
    fn a_slow_body_stops_on_the_cancel() {
        /// 64 KiB a millisecond.
        struct Slow;
        impl Read for Slow {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(1));
                buffer.fill(7);
                Ok(buffer.len())
            }
        }
        struct Endless;
        impl Download for Endless {
            fn get(&self, _: &str) -> Result<Answer, String> {
                Ok(Answer::Body(Box::new(Slow)))
            }
        }
        let cache = Scratch::new("cancel");
        let cancel = alpha2_media::CancelToken::default();
        let mut fetcher = Fetcher::new(
            Base::Url(MANIFEST.to_owned()),
            Some(cache.0.clone()),
            Arc::new(Endless),
            &DEFAULT_POLICY,
        )
        .with_cancel(cancel.clone());
        let started = std::time::Instant::now();
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            });
            fetcher
                .fetch("a.wasm", &"0".repeat(64), 256 << 20)
                .expect_err("cancelled")
        });
        assert_eq!(error.code, alpha2_host_core::plugins::CANCELLED, "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(names(&cache.0), Vec::<String>::new());
    }

    #[test]
    fn two_threads_fetching_one_pin_both_succeed_and_leave_one_file() {
        let cache = Scratch::new("race");
        let body = vec![3_u8; 3 * CHUNK_BYTES + 5];
        let pinned = sha256_hex(&body);
        std::thread::scope(|scope| {
            let runs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        fetcher(&cache.0, &body).fetch("a.wasm", &pinned, body.len() as u64)
                    })
                })
                .collect();
            for run in runs {
                run.join().expect("thread").expect("fetched");
            }
        });
        assert_eq!(names(&cache.0), [format!("{pinned}.wasm")]);
    }

    #[test]
    fn the_cache_lives_under_the_named_or_home_cache_and_never_in_temp() {
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(cache_directory_from(environment(&[])), None);
        assert_eq!(
            cache_directory_from(environment(&[("ALPHA2_PLUGIN_CACHE", "/srv/plugins")])),
            Some(PathBuf::from("/srv/plugins"))
        );
        assert_eq!(
            cache_directory_from(environment(&[("ALPHA2_HOST_CACHE_DIR", "/srv/alpha2")])),
            Some(PathBuf::from("/srv/alpha2/plugins"))
        );
        if cfg!(target_os = "linux") {
            assert_eq!(
                cache_directory_from(environment(&[("XDG_CACHE_HOME", "relative")])),
                None
            );
            assert_eq!(
                cache_directory_from(environment(&[("HOME", "/home/a")])),
                Some(PathBuf::from("/home/a/.cache/alpha2/plugins"))
            );
        }
    }

    #[test]
    fn a_download_without_a_cache_directory_names_the_variable() {
        let mut fetcher = Fetcher::new(
            Base::Url(MANIFEST.to_owned()),
            None,
            Arc::new(Scripted::default()),
            &DEFAULT_POLICY,
        );
        let error = fetcher
            .fetch("a.wasm", &"0".repeat(64), 1)
            .expect_err("no cache");
        assert_eq!(error.code, codes::FETCH_FAILED);
        assert!(error.message.contains("ALPHA2_PLUGIN_CACHE"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let cache = Scratch::new("shared");
        std::fs::create_dir_all(&cache.0).expect("dir");
        std::fs::set_permissions(&cache.0, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let error = fetcher(&cache.0, b"x")
            .fetch("a.wasm", &sha256_hex(b"x"), 1)
            .expect_err("refused");
        assert_eq!(error.code, codes::FETCH_FAILED);
        assert!(error.message.contains("ALPHA2_PLUGIN_CACHE"), "{error}");
        assert_eq!(names(&cache.0), Vec::<String>::new());
        let fresh = Scratch::new("fresh");
        fetcher(&fresh.0, b"x")
            .fetch("a.wasm", &sha256_hex(b"x"), 1)
            .expect("fetched");
        let mode = std::fs::metadata(&fresh.0)
            .expect("dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_manifest_that_is_not_a_regular_file_is_refused_at_once() {
        let error = manifest("/dev/zero", &"0".repeat(64), &*https(), &DEFAULT_POLICY)
            .expect_err("refused");
        assert_eq!(error.code, codes::FETCH_FAILED);
        assert!(error.message.contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_manifest_longer_than_the_bound_is_refused() {
        let long = vec![b' '; MAX_MANIFEST_BYTES + 1];
        let download = Scripted::new(&[(MANIFEST, &long)]);
        let error =
            manifest(MANIFEST, &"0".repeat(64), &download, &DEFAULT_POLICY).expect_err("refused");
        assert_eq!(error.code, codes::FETCH_FAILED);
        assert!(error.message.contains("longer than"), "{error}");
    }

    #[test]
    fn eviction_removes_the_oldest_files_and_keeps_recent_ones() {
        let cache = Scratch::new("evict");
        std::fs::create_dir_all(&cache.0).expect("dir");
        let now = SystemTime::now();
        let hours = |count: u64| now - Duration::from_secs(count * 3600);
        for (name, modified) in [
            ("oldest", hours(5)),
            ("old", hours(3)),
            ("newer", hours(2)),
            ("recent", now - Duration::from_secs(60)),
        ] {
            let path = cache.0.join(name);
            std::fs::write(&path, [0_u8; 100]).expect("file");
            File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(modified))
                .expect("mtime");
        }
        evict(&cache.0, 250, now);
        assert_eq!(names(&cache.0), ["newer", "recent"]);
        // Only recent files are left over the quota: they stay.
        evict(&cache.0, 50, now);
        assert_eq!(names(&cache.0), ["recent"]);
    }

    fn fetch_with(
        base: &str,
        answers: &[(&str, &[u8])],
        policy: &'static PluginPolicy,
        url: &str,
        body: &[u8],
    ) -> (Result<Fetched, PluginError>, Vec<String>) {
        let cache = Scratch::new("policy");
        let download = Arc::new(Scripted::new(answers));
        let base = if base.starts_with("https:") {
            Base::Url(base.to_owned())
        } else {
            Base::Directory(PathBuf::from(base))
        };
        let mut fetcher = Fetcher::new(base, Some(cache.0.clone()), download.clone(), policy);
        let result = fetcher.fetch(url, &sha256_hex(body), body.len() as u64);
        (result, download.asked())
    }

    #[test]
    fn a_reference_to_another_origin_is_fetched_only_when_listed() {
        let cdn = "https://cdn.example.net/a.wasm";
        let answers: &[(&str, &[u8])] = &[(cdn, b"x")];
        let (refused, asked) = fetch_with(MANIFEST, answers, &DEFAULT_POLICY, cdn, b"x");
        let refused = refused.expect_err("refused");
        assert_eq!(refused.code, codes::REFUSED);
        assert!(refused.message.contains("--plugin-origins"), "{refused}");
        assert_eq!(asked, Vec::<String>::new());
        let listed = policy(&[(
            "--plugin-origins",
            "https://example.com,https://cdn.example.net",
        )]);
        let (fetched, asked) = fetch_with(MANIFEST, answers, listed, cdn, b"x");
        fetched.expect("fetched");
        assert_eq!(asked, [cdn]);
        // A local manifest has no origin of its own.
        let (local, asked) = fetch_with("/plugins/p", answers, &DEFAULT_POLICY, cdn, b"x");
        assert_eq!(local.expect_err("refused").code, codes::REFUSED);
        assert_eq!(asked, Vec::<String>::new());
        let none = policy(&[("--plugin-origins", "none")]);
        let (refused, asked) = fetch_with(MANIFEST, &[(URL, b"x")], none, "a.wasm", b"x");
        assert_eq!(refused.expect_err("refused").code, codes::REFUSED);
        assert_eq!(asked, Vec::<String>::new());
    }

    #[test]
    fn redirects_stay_on_allowed_origins_and_end_after_five_hops() {
        let elsewhere: &[(&str, &[u8])] = &[
            (URL, b"->https://cdn.example.net/a.wasm"),
            ("https://cdn.example.net/a.wasm", b"x"),
        ];
        let (refused, asked) = fetch_with(MANIFEST, elsewhere, &DEFAULT_POLICY, "a.wasm", b"x");
        let refused = refused.expect_err("refused");
        assert_eq!(refused.code, codes::REFUSED, "{refused}");
        assert_eq!(asked, [URL]);
        let same: &[(&str, &[u8])] = &[
            (URL, b"->/q/a.wasm?v=2"),
            ("https://example.com/q/a.wasm?v=2", b"->b.wasm"),
            ("https://example.com/q/b.wasm", b"->//example.com/c.wasm"),
            ("https://example.com/c.wasm", b"x"),
        ];
        let (fetched, asked) = fetch_with(MANIFEST, same, &DEFAULT_POLICY, "a.wasm", b"x");
        fetched.expect("followed");
        assert_eq!(asked.len(), 4);
        let to_http: &[(&str, &[u8])] = &[(URL, b"->http://example.com/a.wasm")];
        let (refused, _) = fetch_with(MANIFEST, to_http, &DEFAULT_POLICY, "a.wasm", b"x");
        assert_eq!(refused.expect_err("refused").code, codes::REFUSED);
        let hops: Vec<(String, Vec<u8>)> = (0..6)
            .map(|hop| {
                (
                    format!("https://example.com/p/{hop}.wasm"),
                    format!("->{}.wasm", hop + 1).into_bytes(),
                )
            })
            .chain([("https://example.com/p/6.wasm".to_owned(), b"x".to_vec())])
            .collect();
        let answers: Vec<(&str, &[u8])> = hops
            .iter()
            .map(|(url, body)| (url.as_str(), body.as_slice()))
            .collect();
        let (five, _) = fetch_with(MANIFEST, &answers[1..], &DEFAULT_POLICY, "1.wasm", b"x");
        five.expect("five hops");
        let (six, asked) = fetch_with(MANIFEST, &answers, &DEFAULT_POLICY, "0.wasm", b"x");
        let six = six.expect_err("six hops");
        assert_eq!(six.code, codes::FETCH_FAILED);
        assert!(six.message.contains("more than 5 redirects"), "{six}");
        assert_eq!(asked.len(), 6);
    }

    #[test]
    fn a_redirected_manifest_resolves_against_the_requested_url() {
        let text = b"{}";
        let answers: &[(&str, &[u8])] = &[
            (MANIFEST, b"->/v2/manifest.json"),
            ("https://example.com/v2/manifest.json", text),
        ];
        let download = Scripted::new(answers);
        let (_, base) =
            manifest(MANIFEST, &sha256_hex(text), &download, &DEFAULT_POLICY).expect("manifest");
        assert_eq!(base, Base::Url(MANIFEST.to_owned()));
        let away: &[(&str, &[u8])] = &[(MANIFEST, b"->https://other.example.org/m.json")];
        let refused = manifest(
            MANIFEST,
            &sha256_hex(text),
            &Scripted::new(away),
            &DEFAULT_POLICY,
        )
        .expect_err("refused");
        assert_eq!(refused.code, codes::REFUSED);
    }

    #[test]
    fn the_operator_policy_refuses_a_manifest_before_it_is_read_or_contacted() {
        let download = Scripted::default();
        let none = policy(&[("--plugin-origins", "none")]);
        let refused = manifest(MANIFEST, &"0".repeat(64), &download, none).expect_err("refused");
        assert_eq!(refused.code, codes::REFUSED);
        assert!(
            refused.message.contains("--plugin-origins none"),
            "{refused}"
        );
        let listed = policy(&[("--plugin-origins", "https://cdn.example.net")]);
        let refused = manifest(MANIFEST, &"0".repeat(64), &download, listed).expect_err("refused");
        assert_eq!(refused.code, codes::REFUSED);
        assert_eq!(download.asked(), Vec::<String>::new());

        let root = Scratch::new("root");
        let inside = root.0.join("inside");
        let outside = Scratch::new("outside");
        std::fs::create_dir_all(&inside).expect("dir");
        std::fs::create_dir_all(&outside.0).expect("dir");
        std::fs::write(inside.join("manifest.json"), b"{}").expect("manifest");
        std::fs::write(outside.0.join("manifest.json"), b"{}").expect("manifest");
        let rooted = policy(&[("--plugin-root", inside.to_str().expect("utf-8"))]);
        let pinned = sha256_hex(b"{}");
        let read = |path: &Path, policy| {
            manifest(path.to_str().expect("utf-8"), &pinned, &download, policy)
        };
        read(&inside.join("manifest.json"), rooted).expect("inside");
        for refused in [
            outside.0.join("manifest.json"),
            inside
                .join("../inside/../../")
                .join(outside.0.file_name().expect("name"))
                .join("manifest.json"),
            inside.join("none.json"),
        ] {
            let error = read(&refused, rooted).expect_err("refused");
            assert_eq!(error.code, codes::REFUSED, "{}", refused.display());
            assert!(error.message.contains("--plugin-root"), "{error}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.0.join("manifest.json"), inside.join("link.json"))
                .expect("link");
            let error = read(&inside.join("link.json"), rooted).expect_err("refused");
            assert_eq!(error.code, codes::REFUSED);
        }
        let no_local = policy(&[("--plugin-root", "none")]);
        let error = read(&inside.join("manifest.json"), no_local).expect_err("refused");
        assert_eq!(error.code, codes::REFUSED);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_native_library_loads_from_a_private_copy_of_the_checked_bytes() {
        let cache = Scratch::new("native");
        let plugin = Scratch::new("native-plugin");
        std::fs::create_dir_all(&plugin.0).expect("dir");
        let source = plugin.0.join("libplugin.so");
        let checked = b"checked library bytes";
        let pinned = sha256_hex(checked);
        // The file is replaced after the loader read and checked it.
        std::fs::write(&source, b"swapped library bytes").expect("swap");
        let mut fetcher = Fetcher::new(
            Base::Directory(plugin.0.clone()),
            Some(cache.0.clone()),
            Arc::new(Scripted::default()),
            &DEFAULT_POLICY,
        );
        let copy = fetcher
            .prepare_native(&source, checked, &pinned)
            .expect("prepared");
        assert_eq!(copy, cache.0.join("native").join(format!("{pinned}.so")));
        assert_eq!(std::fs::read(&copy).expect("copy"), checked);
        // A copy with other bytes under the pinned name is replaced, not used.
        std::fs::write(&copy, b"stale").expect("stale");
        let again = fetcher
            .prepare_native(&source, checked, &pinned)
            .expect("prepared");
        assert_eq!(std::fs::read(&again).expect("copy"), checked);
        assert_eq!(names(&cache.0.join("native")), [format!("{pinned}.so")]);
        let mut homeless = Fetcher::new(
            Base::Directory(plugin.0.clone()),
            None,
            Arc::new(Scripted::default()),
            &DEFAULT_POLICY,
        );
        let error = homeless
            .prepare_native(&source, checked, &pinned)
            .expect_err("no cache");
        assert_eq!(error.code, codes::LOAD_FAILED);
        assert!(error.message.contains("ALPHA2_PLUGIN_CACHE"), "{error}");
    }

    #[test]
    fn a_url_in_a_message_has_no_user_info_query_or_fragment() {
        for (url, shown) in [
            (
                "https://user:secret@cdn.example.com:8443/p/manifest.json?token=abc#part",
                "https://cdn.example.com:8443/p/manifest.json",
            ),
            (
                "https://example.com/m.json?X-Amz-Signature=abc",
                "https://example.com/m.json",
            ),
            ("https://token@example.com", "https://example.com"),
            (
                "https://example.com/a@b.wasm",
                "https://example.com/a@b.wasm",
            ),
            (
                "file:///tmp/a%20b/manifest.json#x",
                "file:///tmp/a%20b/manifest.json",
            ),
            ("ftp://user:pw@example.com/m", "ftp://example.com/m"),
            ("/plugins/p?/manifest.json", "/plugins/p?/manifest.json"),
            (r"C:\plugins\manifest.json", r"C:\plugins\manifest.json"),
        ] {
            assert_eq!(shown_url(url), shown, "{url}");
        }
        let secret =
            |error: &PluginError| error.message.contains("pw") || error.message.contains("abc");
        let refused = manifest_location("ftp://user:pw@example.com/m?token=abc").expect_err("ftp");
        assert!(!secret(&refused), "{}", refused.message);
        let reference = Base::Directory(PathBuf::from("/p"))
            .resolve("http://user:pw@example.com/a.wasm?token=abc")
            .expect_err("http");
        assert!(!secret(&reference), "{}", reference.message);
        // Port 1 refuses the connection; the failure must not repeat the credentials.
        let failed = manifest(
            "https://user:pw@127.0.0.1:1/m.json?token=abc",
            &"0".repeat(64),
            &*https(),
            &DEFAULT_POLICY,
        )
        .expect_err("nothing listens");
        assert_eq!(failed.code, codes::FETCH_FAILED);
        assert!(!secret(&failed), "{}", failed.message);
    }

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
        let (text, base) = manifest(
            path.to_str().expect("utf-8"),
            &pinned,
            &*https(),
            &DEFAULT_POLICY,
        )
        .expect("read");
        assert_eq!((text.as_str(), base), ("{}", Base::Directory(dir.clone())));
        let wrong = manifest(
            path.to_str().expect("utf-8"),
            &"0".repeat(64),
            &*https(),
            &DEFAULT_POLICY,
        )
        .expect_err("refused");
        assert_eq!(wrong.code, codes::HASH_MISMATCH);
        let missing = manifest(
            dir.join("none.json").to_str().expect("utf-8"),
            &pinned,
            &*https(),
            &DEFAULT_POLICY,
        )
        .expect_err("refused");
        assert_eq!(missing.code, codes::FETCH_FAILED);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
