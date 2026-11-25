//! Loads extractor plugins. The trust policy picks a manifest's artifact, which is checked against
//! its hash and loaded as an [`Extractor`] the session drives like any other. wasm modules run in
//! wasmtime, sandboxed, with capped memory and bounded calls. Native libraries run only for
//! plugins the caller allows, in this process or in a worker process of their own
//! ([`Isolation`]).
//!
//! The caller fetches: [`load`] asks a [`Fetch`] for each file by URL and hash.

mod native;
mod process;
#[cfg(test)]
mod process_tests;
mod resources;
#[cfg(test)]
mod resources_tests;
#[cfg(test)]
mod scenes_tests;
#[cfg(test)]
mod silence_tests;
#[cfg(test)]
mod tests;
mod wasm;
pub mod wire;
pub mod worker;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alpha2_engine::extract::{
    ArtifactKind, AudioInput, Descriptor, Emitter, Extractor, ExtractorError, ManifestError,
    PassInfo, PluginArtifact, PluginManifest, RunContext, VideoFormat, VideoInput, WASM_TARGET,
};
use alpha2_media::CancelToken;
use alpha2_model::{
    DataInterval, DataWord, MAX_DATA_CHANNELS, MAX_DATA_LABEL_BYTES, MadeByPlugin, SuppliedFile,
};
use alpha2_plugin_abi::records::{Limits, Record, records};
use alpha2_plugin_abi::{direction, status, video_format};
use alpha2_time::MediaTime;
use sha2::{Digest, Sha256};

/// The error codes plugins fail with, the same on every surface.
pub mod codes {
    /// A manifest, artifact or resource could not be fetched.
    pub const FETCH_FAILED: &str = "PLUGIN_FETCH_FAILED";
    /// A file's SHA-256 or length differs from what the manifest pins.
    pub const HASH_MISMATCH: &str = "PLUGIN_HASH_MISMATCH";
    /// The plugin implements an ABI this build does not.
    pub const ABI_UNSUPPORTED: &str = "PLUGIN_ABI_UNSUPPORTED";
    /// No artifact runs here; the message lists the targets the manifest offers.
    pub const NO_ARTIFACT: &str = "PLUGIN_NO_ARTIFACT";
    /// Only a native artifact runs here, and the caller did not allow native code.
    pub const NATIVE_REFUSED: &str = "PLUGIN_NATIVE_REFUSED";
    /// The host's plugin policy refuses the plugin or one of its files.
    pub const REFUSED: &str = "PLUGIN_REFUSED";
    /// The artifact's descriptor differs from the manifest's.
    pub const DESCRIPTOR_MISMATCH: &str = "PLUGIN_DESCRIPTOR_MISMATCH";
    /// The wasm module uses relaxed SIMD, threads, shared memory, more than one memory or 64-bit
    /// memory, or imports something else.
    pub const UNSUPPORTED_FEATURE: &str = "PLUGIN_UNSUPPORTED_FEATURE";
    /// The manifest is not one, or the artifact did not load.
    pub const LOAD_FAILED: &str = "PLUGIN_LOAD_FAILED";
    /// A wasm trap, or a native panic caught at the boundary.
    pub const TRAPPED: &str = "PLUGIN_TRAPPED";
    /// The wasm module ran past its memory cap or table bound, or a call past its deadline.
    pub const LIMIT: &str = "PLUGIN_LIMIT";
    /// A file the caller supplied for a resource slot does not fit the slot; `details` names the
    /// slot and the reason.
    pub const RESOURCE_INVALID: &str = "PLUGIN_RESOURCE_INVALID";
}

/// The code of a load or call that stopped because the caller's cancel token was set. It is not a
/// plugin code: the request ends as cancelled.
pub const CANCELLED: &str = "CANCELLED";

/// Every plugin error code.
pub const PLUGIN_CODES: [&str; 12] = [
    codes::FETCH_FAILED,
    codes::HASH_MISMATCH,
    codes::ABI_UNSUPPORTED,
    codes::NO_ARTIFACT,
    codes::NATIVE_REFUSED,
    codes::REFUSED,
    codes::DESCRIPTOR_MISMATCH,
    codes::UNSUPPORTED_FEATURE,
    codes::LOAD_FAILED,
    codes::TRAPPED,
    codes::LIMIT,
    codes::RESOURCE_INVALID,
];

/// Why a plugin did not load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginError {
    /// One of [`codes`].
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
    /// What a caller reads besides the code: `slot` and `reason` for
    /// [`codes::RESOURCE_INVALID`], `exit`, `signal` or `wire` for an isolated child that failed.
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
}

impl PluginError {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// The error with `details`.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Map<String, serde_json::Value>) -> Self {
        self.details = Some(details);
        self
    }

    /// A [`codes::RESOURCE_INVALID`] error for `slot`, for one of [`slot_reasons`].
    pub fn resource_invalid(slot: &str, reason: &str, message: impl Into<String>) -> Self {
        let mut details = serde_json::Map::new();
        details.insert("slot".to_owned(), slot.into());
        details.insert("reason".to_owned(), reason.into());
        Self::new(codes::RESOURCE_INVALID, message).with_details(details)
    }

    /// The error as a call's failure.
    #[must_use]
    pub fn into_call_error(self) -> ExtractorError {
        let error = ExtractorError::with_code(self.code, self.message);
        match self.details {
            Some(details) => error.with_details(details),
            None => error,
        }
    }
}

/// The `reason` of a [`codes::RESOURCE_INVALID`] error.
pub mod slot_reasons {
    /// A required slot was not filled.
    pub const MISSING: &str = "missing";
    /// The caller filled a slot the manifest does not declare.
    pub const UNKNOWN_SLOT: &str = "unknown_slot";
    /// A file where the slot takes a directory, or the other way round.
    pub const SHAPE: &str = "shape";
    /// A file longer than the slot's `max_bytes`, or than a wasm module can read.
    pub const TOO_LARGE: &str = "too_large";
    /// A directory with more files than a slot holds.
    pub const TOO_MANY_FILES: &str = "too_many_files";
    /// A directory deeper than a slot holds, or one that is its own ancestor.
    pub const TOO_DEEP: &str = "too_deep";
    /// A file whose resource name is longer than a slot's names may be.
    pub const NAME_TOO_LONG: &str = "name_too_long";
    /// A file whose name is not UTF-8.
    pub const NOT_UTF8: &str = "not_utf8";
    /// This loader fills no slots.
    pub const UNSUPPORTED: &str = "unsupported";
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PluginError {}

impl From<ManifestError> for PluginError {
    fn from(error: ManifestError) -> Self {
        let code = match error {
            ManifestError::UnsupportedAbi(_) => codes::ABI_UNSUPPORTED,
            _ => codes::LOAD_FAILED,
        };
        Self::new(code, error.to_string())
    }
}

/// Reads and checks a manifest.
///
/// # Errors
/// `PLUGIN_ABI_UNSUPPORTED` for another ABI, `PLUGIN_LOAD_FAILED` for a manifest that is not one.
pub fn parse_manifest(text: &str) -> Result<PluginManifest, PluginError> {
    PluginManifest::from_json(text).map_err(PluginError::from)
}

/// What the caller allows a plugin. A program that calls [`load`] is its own operator: it decides
/// here what the `alpha2-host` operator decides with its `serve` flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Native code runs with the process's rights; refused unless allowed.
    pub allow_native: bool,
    /// The wasm artifact even where a native one is allowed.
    pub force_wasm: bool,
    /// The most linear memory a wasm module may have, in bytes; at most [`MAX_MEMORY_BYTES`].
    pub memory_bytes: u64,
    /// The longest one wasm call may run.
    pub call_deadline: Duration,
    /// Where files the caller supplies for resource slots may lie.
    pub resource_root: ResourceRoot,
    /// Where a native artifact runs.
    pub isolation: Isolation,
}

/// Where a native artifact runs; wasm modules always run in this process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Isolation {
    /// In this process: a crash of the plugin ends it, and a call runs to its end.
    #[default]
    InProcess,
    /// In a worker process per run, started with this command: a crash or a hang fails the run,
    /// and cancel and the call deadline kill the worker. A program that writes to a dead worker's
    /// pipe needs `SIGPIPE` ignored, as Rust programs have it.
    Child(WorkerCommand),
}

/// The command that runs [`worker::main`], whose standard input and output become the pipes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCommand {
    /// The program, usually this one.
    pub program: PathBuf,
    /// Its arguments, such as the subcommand that calls [`worker::main`].
    pub args: Vec<std::ffi::OsString>,
    /// Variables set in its environment.
    pub envs: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// How video frames reach the worker.
    pub frames: FramePath,
}

/// How an isolated plugin's video frames reach its worker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FramePath {
    /// Through memory the host shares with the worker, read-only there; a run whose memory
    /// cannot be made or mapped falls back to the pipe, with one diagnostic.
    #[default]
    Shared,
    /// Through the worker's pipe, copied by the kernel.
    Pipe,
}

/// Where files supplied for resource slots may lie, links resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ResourceRoot {
    /// Anywhere the caller names.
    Any,
    /// Nowhere: a load that fills a slot is refused.
    #[default]
    None,
    /// Under this directory.
    Dir(PathBuf),
}

/// Linear memory a wasm plugin may grow to when the caller sets no cap.
pub const DEFAULT_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
/// The most memory a wasm plugin may have, the span its `i32` offsets address; a larger cap is
/// taken as this.
pub const MAX_MEMORY_BYTES: u64 = 1 << 31;
/// How long one wasm call may run when the caller sets no deadline.
pub const DEFAULT_CALL_DEADLINE: Duration = Duration::from_secs(300);

impl Default for Policy {
    fn default() -> Self {
        Self {
            allow_native: false,
            force_wasm: false,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            call_deadline: DEFAULT_CALL_DEADLINE,
            resource_root: ResourceRoot::None,
            isolation: Isolation::InProcess,
        }
    }
}

/// The Rust target triple this build runs on, as manifests name native artifacts.
pub const TARGET: &str = env!("ALPHA2_HOST_CORE_TARGET");

/// Picks the native artifact for `target` when the policy allows native code, else the wasm one.
///
/// # Errors
/// `PLUGIN_NATIVE_REFUSED` when only a native artifact runs here and native code is not allowed,
/// `PLUGIN_NO_ARTIFACT`, listing the offered targets, when none runs here.
pub fn select<'m>(
    manifest: &'m PluginManifest,
    policy: &Policy,
    target: &str,
) -> Result<&'m PluginArtifact, PluginError> {
    let native = manifest.artifact(target, ArtifactKind::Cdylib);
    let wasm = manifest.artifact(WASM_TARGET, ArtifactKind::Wasm);
    match (native, wasm) {
        (Some(native), _) if policy.allow_native && !policy.force_wasm => Ok(native),
        (_, Some(wasm)) => Ok(wasm),
        (Some(_), None) if policy.force_wasm => Err(PluginError::new(
            codes::NO_ARTIFACT,
            format!(
                "{} has no wasm artifact; it offers {target} only",
                manifest.id
            ),
        )),
        (Some(_), None) => Err(PluginError::new(
            codes::NATIVE_REFUSED,
            format!(
                "{} runs here only as native code, which has the process's rights; allow native \
                 code for this plugin to run it",
                manifest.id
            ),
        )),
        (None, None) => {
            let offered = manifest
                .artifacts
                .iter()
                .map(|artifact| format!("{} ({:?})", artifact.target, artifact.kind).to_lowercase())
                .collect::<Vec<_>>();
            Err(PluginError::new(
                codes::NO_ARTIFACT,
                format!(
                    "{} has no artifact for {target} and no wasm artifact; it offers {}",
                    manifest.id,
                    offered.join(", ")
                ),
            ))
        }
    }
}

/// Hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// # Errors
/// `PLUGIN_HASH_MISMATCH` when `bytes` are not the `size` bytes with the pinned `sha256`. Only
/// for a `local` file does the message state the length or digest found.
pub fn verify(
    bytes: &[u8],
    sha256: &str,
    size: u64,
    what: &str,
    local: bool,
) -> Result<(), PluginError> {
    let length_differs = bytes.len() as u64 != size;
    if !length_differs && sha256_hex(bytes) == sha256 {
        return Ok(());
    }
    let message = if !local {
        format!("{what} does not match the length and SHA-256 the manifest pins")
    } else if length_differs {
        format!("{what} is {} bytes; the manifest pins {size}", bytes.len())
    } else {
        format!(
            "{what} has SHA-256 {}; the manifest pins {sha256}",
            sha256_hex(bytes)
        )
    };
    Err(PluginError::new(codes::HASH_MISMATCH, message))
}

/// A file a [`Fetch`] supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// Where its bytes are.
    pub path: PathBuf,
    /// A local file the caller named; false for a download, whose content a mismatch message
    /// must not describe.
    pub local: bool,
}

/// Where a plugin's files come from.
pub trait Fetch {
    /// The file a manifest's `url` names, a path under the manifest's directory or an `https:`
    /// URL. The loader checks its bytes against `sha256` and `bytes` itself.
    ///
    /// # Errors
    /// `PLUGIN_FETCH_FAILED`, or `PLUGIN_HASH_MISMATCH` when the fetcher checks too.
    fn fetch(&mut self, url: &str, sha256: &str, bytes: u64) -> Result<Fetched, PluginError>;

    /// The file to load for the native artifact read from `verified`, whose checked `bytes` have
    /// the pinned `sha256`. In process it is called only when this process has not loaded that
    /// artifact yet, and the library then stays loaded until the process exits; with
    /// [`Isolation::Child`] it is called for every run. A host loads its own copy of the bytes
    /// here, so the file it checked cannot be replaced before the load.
    ///
    /// # Errors
    /// `PLUGIN_LOAD_FAILED` when it cannot be prepared.
    fn prepare_native(
        &mut self,
        verified: &Path,
        bytes: &[u8],
        sha256: &str,
    ) -> Result<PathBuf, PluginError> {
        let _ = (bytes, sha256);
        Ok(verified.to_path_buf())
    }
}

/// Checks the file `fetch` gives for a pinned resource without keeping its bytes.
fn check_pinned(
    fetch: &mut dyn Fetch,
    url: &str,
    sha256: &str,
    size: u64,
) -> Result<resources::PinnedFile, PluginError> {
    use std::io::Read;
    let Fetched { path, local } = fetch.fetch(url, sha256, size)?;
    let failed = |error: std::io::Error| {
        PluginError::new(codes::FETCH_FAILED, format!("{}: {error}", path.display()))
    };
    let mut file = std::fs::File::open(&path)
        .map_err(failed)?
        .take(size.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(failed)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    let found = hex(&hasher.finalize());
    if total != size || found != sha256 {
        let message = if !local {
            format!("{url} does not match the length and SHA-256 the manifest pins")
        } else if total > size {
            format!("{url} is longer than the {size} bytes the manifest pins")
        } else if total != size {
            format!("{url} is {total} bytes; the manifest pins {size}")
        } else {
            format!("{url} has SHA-256 {found}; the manifest pins {sha256}")
        };
        return Err(PluginError::new(codes::HASH_MISMATCH, message));
    }
    Ok(resources::PinnedFile {
        path,
        sha256: sha256.to_owned(),
        bytes: size,
        local,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn read_verified(
    fetch: &mut dyn Fetch,
    url: &str,
    sha256: &str,
    size: u64,
) -> Result<(PathBuf, Vec<u8>), PluginError> {
    use std::io::Read;
    let Fetched { path, local } = fetch.fetch(url, sha256, size)?;
    let failed = |error: std::io::Error| {
        PluginError::new(codes::FETCH_FAILED, format!("{}: {error}", path.display()))
    };
    let file = std::fs::File::open(&path).map_err(failed)?;
    let length = file.metadata().map_err(failed)?.len();
    let mut bytes = Vec::with_capacity(usize::try_from(length.min(size)).unwrap_or(0));
    file.take(size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    if bytes.len() as u64 > size {
        let message = if local {
            format!("{url} is longer than the {size} bytes the manifest pins")
        } else {
            format!("{url} does not match the length and SHA-256 the manifest pins")
        };
        return Err(PluginError::new(codes::HASH_MISMATCH, message));
    }
    verify(&bytes, sha256, size, url, local)?;
    Ok((path, bytes))
}

/// The longest plugin log line written; the rest is cut.
const MAX_LOG_BYTES: usize = 2048;
/// Log text one run may write; later lines are dropped after one notice.
const LOG_BUDGET_BYTES: usize = 64 * 1024;
/// The longest `fail` message kept; the rest is cut.
const MAX_FAIL_BYTES: usize = 4096;

/// The first `max` of `bytes` as UTF-8 text, without a character the cut splits.
fn bounded_text(bytes: &[u8], max: usize) -> String {
    let mut kept = &bytes[..bytes.len().min(max)];
    if kept.len() < bytes.len()
        && let Err(error) = std::str::from_utf8(kept)
        && error.error_len().is_none()
    {
        kept = &kept[..error.valid_up_to()];
    }
    String::from_utf8_lossy(kept).into_owned()
}

/// What is left of a run's log budget.
#[derive(Debug)]
struct LogBudget {
    remaining: usize,
    exhausted: bool,
}

impl Default for LogBudget {
    fn default() -> Self {
        Self {
            remaining: LOG_BUDGET_BYTES,
            exhausted: false,
        }
    }
}

impl LogBudget {
    /// The line to write for a plugin's `message`, cut to [`MAX_LOG_BYTES`]; once the budget is
    /// spent, one notice and then nothing.
    fn line(&mut self, message: &[u8]) -> Option<String> {
        if self.exhausted {
            return None;
        }
        let text = bounded_text(message, MAX_LOG_BYTES);
        if text.len() > self.remaining {
            self.exhausted = true;
            return Some(format!(
                "the plugin logged more than {LOG_BUDGET_BYTES} bytes; its further lines are dropped"
            ));
        }
        self.remaining -= text.len();
        Some(text)
    }

    /// Writes a plugin's diagnostic to standard error, where host diagnostics go; a closed stream
    /// drops it.
    fn log(&mut self, message: &[u8]) {
        if let Some(line) = self.line(message) {
            alpha2_os::diagnostic!("plugin: {line}");
        }
    }
}

/// What one plugin call emitted, or why it failed.
type Call = Result<Vec<Record>, ExtractorError>;

/// How the loader calls a plugin, whichever transport it came through.
trait Transport {
    fn describe(&mut self) -> Result<String, PluginError>;
    fn begin(&mut self, context: &[u8]) -> Call;
    fn pass(&mut self, pass: u32, direction: u32) -> Call;
    fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> Call;
    fn frame_buffer(&mut self, _len: usize) -> Option<&mut [u8]> {
        None
    }
    fn audio(&mut self, block: &AudioInput) -> Call;
    fn end(&mut self) -> Call;
    /// The supplied files the plugin read so far, sorted by name.
    fn supplied(&self) -> Vec<SuppliedFile>;
}

/// The most items a declared output takes in one call.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputCap {
    name: String,
    items: usize,
}

fn output_caps(descriptor: &Descriptor) -> Vec<OutputCap> {
    descriptor
        .outputs
        .iter()
        .map(|output| OutputCap {
            name: output.name.clone(),
            items: output.max_items(),
        })
        .collect()
}

/// Bounds on one emitted record: the widest sample and the longest text any output takes.
const RECORD_LIMITS: Limits = Limits {
    max_values: MAX_DATA_CHANNELS * 4,
    max_text_bytes: MAX_DATA_LABEL_BYTES,
};

/// What a call's emit and fail callbacks left, turned into the call's outcome.
#[derive(Debug, Default)]
struct Collected {
    records: Vec<Record>,
    /// Records each output took in this call, by output index.
    counts: Vec<usize>,
    /// Why an emit of this call was refused; later emits of the call are refused at once.
    refusal: Option<String>,
    failure: Option<String>,
}

impl Collected {
    fn outcome(&mut self, status: i32) -> Call {
        let collected = std::mem::take(self);
        if let Some(reason) = collected.refusal {
            return Err(ExtractorError::with_code("INVALID_EMIT", reason));
        }
        match status {
            status::OK => Ok(collected.records),
            status::FAILED => {
                Err(ExtractorError::new(collected.failure.unwrap_or_else(
                    || "the plugin failed without saying why".to_owned(),
                )))
            }
            status::TRAPPED => Err(ExtractorError::with_code(
                codes::TRAPPED,
                "the plugin panicked; it caught the panic at its boundary",
            )),
            other => Err(ExtractorError::with_code(
                codes::TRAPPED,
                format!("the plugin returned the unknown status {other}"),
            )),
        }
    }

    /// Takes a batch whole, or refuses it and keeps nothing of it.
    fn emit(&mut self, batch: &[u8], caps: &[OutputCap]) -> i32 {
        if self.refusal.is_some() {
            return status::FAILED;
        }
        let start = self.records.len();
        self.counts.resize(caps.len(), 0);
        for record in records(batch, RECORD_LIMITS) {
            let refusal = match record {
                Err(error) => Some(error.to_string()),
                Ok(record) => {
                    let output = usize::from(record.output());
                    match (caps.get(output), self.counts.get_mut(output)) {
                        (Some(cap), Some(count)) if *count < cap.items => {
                            *count += 1;
                            self.records.push(record);
                            None
                        }
                        (Some(cap), _) => Some(format!(
                            "the plugin emitted more than {} items to output {} in one call",
                            cap.items, cap.name
                        )),
                        _ => Some(format!(
                            "the plugin emitted to output {output}, which it does not declare"
                        )),
                    }
                }
            };
            if let Some(reason) = refusal {
                self.records.truncate(start);
                self.refusal = Some(reason);
                return status::FAILED;
            }
        }
        status::OK
    }
}

/// A loaded plugin, ready to run as an extractor.
pub struct PluginExtractor {
    descriptor: Descriptor,
    params: serde_json::Map<String, serde_json::Value>,
    slots_context: serde_json::Value,
    plugin: MadeByPlugin,
    transport: Box<dyn Transport>,
    native: bool,
    isolated: bool,
}

impl fmt::Debug for PluginExtractor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginExtractor")
            .field("id", &self.descriptor.id)
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}

/// What a caller loads: a checked manifest and what one run of it takes.
#[derive(Clone, Copy, Debug)]
pub struct LoadRequest<'a> {
    /// The manifest, as [`parse_manifest`] read it.
    pub manifest: &'a PluginManifest,
    /// SHA-256 of the manifest's bytes, which the caller checked; the data records it lowercase.
    pub manifest_sha256: &'a str,
    /// The run's parameters.
    pub params: &'a serde_json::Map<String, serde_json::Value>,
    /// The file or directory the caller names for each resource slot it fills, by slot name.
    pub supplied: &'a BTreeMap<String, PathBuf>,
}

/// Loads the artifact `policy` picks from the request's manifest, fetching it and every pinned
/// resource through `fetch`. Slots are resolved before any download; their files are read only
/// when the plugin asks for them. Once `cancel` is
/// set, the load stops before the next file, and a wasm call of the plugin it returns stops within
/// about 50 ms; a module compile and a native call run to their end.
///
/// # Errors
/// A [`PluginError`] for the first check that fails, [`CANCELLED`] once `cancel` is set.
pub fn load(
    request: LoadRequest<'_>,
    policy: &Policy,
    fetch: &mut dyn Fetch,
    cancel: &CancelToken,
) -> Result<PluginExtractor, PluginError> {
    let LoadRequest {
        manifest,
        manifest_sha256,
        params,
        supplied,
    } = request;
    let artifact = select(manifest, policy, TARGET)?;
    let stopped = || {
        if cancel.is_cancelled() {
            Err(PluginError::new(CANCELLED, "the plugin load was cancelled"))
        } else {
            Ok(())
        }
    };
    let mut resources = resources::ResourceSet::resolve(
        manifest,
        supplied,
        &policy.resource_root,
        artifact.kind == ArtifactKind::Wasm,
    )?;
    for resource in &manifest.resources {
        stopped()?;
        let Some(pinned) = resource.pinned() else {
            continue;
        };
        let file = check_pinned(fetch, &pinned.url, &pinned.sha256, pinned.bytes)?;
        resources.add_pinned(&resource.name, file);
    }
    let slots_context = resources.context();
    stopped()?;
    let (path, bytes) = read_verified(fetch, &artifact.url, &artifact.sha256, artifact.bytes)?;
    let caps = output_caps(&manifest.descriptor);
    let native = artifact.kind == ArtifactKind::Cdylib;
    let isolated = native && matches!(policy.isolation, Isolation::Child(_));
    let mut transport = transport(
        manifest,
        artifact,
        (&path, &bytes),
        resources,
        caps,
        (policy, fetch, cancel),
    )?;
    let described: Descriptor = serde_json::from_str(&transport.describe()?).map_err(|error| {
        PluginError::new(
            codes::DESCRIPTOR_MISMATCH,
            format!("the artifact's descriptor does not parse: {error}"),
        )
    })?;
    if described != manifest.descriptor {
        return Err(PluginError::new(
            codes::DESCRIPTOR_MISMATCH,
            format!(
                "the artifact {} describes another extractor than the manifest",
                artifact.url
            ),
        ));
    }
    Ok(PluginExtractor {
        descriptor: described,
        params: params.clone(),
        slots_context,
        plugin: MadeByPlugin {
            version: manifest.version.clone(),
            artifact_sha256: artifact.sha256.clone(),
            manifest_sha256: manifest_sha256.to_ascii_lowercase(),
            supplied: Vec::new(),
        },
        transport,
        native,
        isolated,
    })
}

/// The transport that runs `artifact`, whose checked file is `path` holding `bytes`.
fn transport(
    manifest: &PluginManifest,
    artifact: &PluginArtifact,
    (path, bytes): (&Path, &[u8]),
    resources: resources::ResourceSet,
    caps: Vec<OutputCap>,
    (policy, fetch, cancel): (&Policy, &mut dyn Fetch, &CancelToken),
) -> Result<Box<dyn Transport>, PluginError> {
    Ok(match artifact.kind {
        ArtifactKind::Wasm => Box::new(wasm::WasmTransport::new(
            &manifest.id,
            bytes,
            &artifact.sha256,
            resources::ResourceReader::new(resources, cancel.clone()),
            caps,
            policy,
            cancel.clone(),
        )?),
        ArtifactKind::Cdylib => match &policy.isolation {
            Isolation::InProcess => {
                Box::new(native::NativeTransport::new(native::NativeInstance::new(
                    &artifact.sha256,
                    || fetch.prepare_native(path, bytes, &artifact.sha256),
                    resources::ResourceReader::new(resources, cancel.clone()),
                    caps,
                    None,
                )?))
            }
            Isolation::Child(command) => {
                let library =
                    std::path::absolute(fetch.prepare_native(path, bytes, &artifact.sha256)?)
                        .map_err(|error| {
                            PluginError::new(
                                codes::LOAD_FAILED,
                                format!("the library path: {error}"),
                            )
                        })?;
                if cancel.is_cancelled() {
                    return Err(PluginError::new(CANCELLED, "the plugin load was cancelled"));
                }
                let mut worker =
                    process::ProcessTransport::spawn(command, cancel, policy.call_deadline)?;
                worker.load(
                    &wire::Load {
                        library,
                        sha256: artifact.sha256.clone(),
                        check: !cfg!(target_os = "macos"),
                        caps: caps
                            .iter()
                            .map(|cap| (cap.name.clone(), cap.items))
                            .collect(),
                        resources,
                    },
                    caps,
                )?;
                Box::new(worker)
            }
        },
        ArtifactKind::Staticlib => {
            return Err(PluginError::new(
                codes::NO_ARTIFACT,
                "a static library is linked at build time, not loaded",
            ));
        }
    })
}

impl PluginExtractor {
    /// Whether the plugin is a native library.
    #[must_use]
    pub const fn is_native(&self) -> bool {
        self.native
    }

    /// Whether its calls end at the policy's call deadline: a wasm module's, and an isolated
    /// native library's; a native call in this process runs to its end.
    #[must_use]
    pub const fn calls_are_bounded(&self) -> bool {
        !self.native || self.isolated
    }

    /// What `made_by.plugin` records for data this plugin makes, with the supplied files read
    /// so far; complete once the run has ended.
    #[must_use]
    pub fn made_by(&self) -> MadeByPlugin {
        MadeByPlugin {
            supplied: self.transport.supplied(),
            ..self.plugin.clone()
        }
    }

    fn deliver(&self, records: Vec<Record>, out: &mut Emitter) -> Result<(), ExtractorError> {
        for record in records {
            let name = |output: u16| {
                self.descriptor
                    .outputs
                    .get(usize::from(output))
                    .map(|spec| spec.name.as_str())
                    .ok_or_else(|| {
                        ExtractorError::with_code(
                            "INVALID_EMIT",
                            format!(
                                "the plugin emitted to output {output}, which it does not declare"
                            ),
                        )
                    })
            };
            match record {
                Record::Sample {
                    output,
                    time,
                    values,
                } => out.sample(name(output)?, MediaTime(time), &values),
                Record::Word {
                    output,
                    start,
                    end,
                    text,
                    speaker,
                } => out.word(
                    name(output)?,
                    DataWord {
                        start: MediaTime(start),
                        end: MediaTime(end),
                        text,
                        speaker,
                    },
                ),
                Record::Interval {
                    output,
                    start,
                    end,
                    label,
                    score,
                } => out.interval(
                    name(output)?,
                    DataInterval {
                        start: MediaTime(start),
                        end: MediaTime(end),
                        label,
                        score,
                    },
                ),
            }
        }
        Ok(())
    }
}

/// Emits are taken only from frames, blocks and `end`.
fn nothing_emitted(records: &[Record], step: &str) -> Result<(), ExtractorError> {
    if records.is_empty() {
        Ok(())
    } else {
        Err(ExtractorError::with_code(
            "INVALID_EMIT",
            format!("the plugin emitted during {step}, which takes no emits"),
        ))
    }
}

impl Extractor for PluginExtractor {
    fn describe(&self) -> &Descriptor {
        &self.descriptor
    }

    fn begin(&mut self, context: &RunContext) -> Result<(), ExtractorError> {
        let context = serde_json::json!({
            "range": context.range,
            "passes": context.passes,
            "from": context.from,
            "video": context.video.map(|(size, format)| {
                serde_json::json!({ "size": size, "format": format })
            }),
            "audio": context.audio.map(|(rate, channels, block_frames)| serde_json::json!({
                "rate": rate, "channels": channels, "block_frames": block_frames
            })),
            "frame_rate": context.frame_rate,
            "params": self.params,
            "supplied": self.slots_context,
        });
        let records = self.transport.begin(context.to_string().as_bytes())?;
        nothing_emitted(&records, "begin")
    }

    fn pass(&mut self, pass: &PassInfo) -> Result<(), ExtractorError> {
        let way = match pass.direction {
            alpha2_engine::extract::Direction::Forward => direction::FORWARD,
            alpha2_engine::extract::Direction::Backward => direction::BACKWARD,
        };
        let index = u32::try_from(pass.pass).unwrap_or(u32::MAX);
        let records = self.transport.pass(index, way)?;
        nothing_emitted(&records, "a pass start")
    }

    fn video(&mut self, frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let format = match frame.format {
            VideoFormat::Rgba8 => video_format::RGBA8,
            VideoFormat::Luma8 => video_format::LUMA8,
        };
        let records = self.transport.video(
            frame.time.ticks(),
            frame.width,
            frame.height,
            format,
            &frame.bytes,
        )?;
        self.deliver(records, out)
    }

    fn frame_buffer(&mut self, len: usize) -> Option<&mut [u8]> {
        self.transport.frame_buffer(len)
    }

    fn audio(&mut self, block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let records = self.transport.audio(block)?;
        self.deliver(records, out)
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError> {
        let records = self.transport.end()?;
        self.deliver(records, out)
    }
}
