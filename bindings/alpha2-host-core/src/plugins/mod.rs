//! Loads extractor plugins. The trust policy picks a manifest's artifact, which is checked against
//! its hash and loaded as an [`Extractor`] the session drives like any other. wasm modules run in
//! wasmtime, sandboxed, with capped memory and bounded calls. Native libraries run in the process,
//! and only for plugins the caller allows.
//!
//! The caller fetches: [`load`] asks a [`Fetch`] for each file by URL and hash.

mod native;
#[cfg(test)]
mod silence_tests;
#[cfg(test)]
mod tests;
mod wasm;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alpha2_engine::extract::{
    ArtifactKind, AudioInput, Descriptor, Emitter, Extractor, ExtractorError, ManifestError,
    PassInfo, PluginArtifact, PluginManifest, RunContext, VideoFormat, VideoInput, WASM_TARGET,
};
use alpha2_model::{DataInterval, DataWord, MAX_DATA_CHANNELS, MAX_DATA_LABEL_BYTES, MadeByPlugin};
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
}

/// Every plugin error code.
pub const PLUGIN_CODES: [&str; 10] = [
    codes::FETCH_FAILED,
    codes::HASH_MISMATCH,
    codes::ABI_UNSUPPORTED,
    codes::NO_ARTIFACT,
    codes::NATIVE_REFUSED,
    codes::DESCRIPTOR_MISMATCH,
    codes::UNSUPPORTED_FEATURE,
    codes::LOAD_FAILED,
    codes::TRAPPED,
    codes::LIMIT,
];

/// Why a plugin did not load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginError {
    /// One of [`codes`].
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl PluginError {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
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

/// What the caller allows a plugin.
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
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            use fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
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

    /// The file to load for the native artifact at `verified`, whose bytes the loader has checked;
    /// macOS hosts sign a copy here.
    ///
    /// # Errors
    /// `PLUGIN_LOAD_FAILED` when it cannot be prepared.
    fn prepare_native(&mut self, verified: &Path) -> Result<PathBuf, PluginError> {
        Ok(verified.to_path_buf())
    }
}

fn read_verified(
    fetch: &mut dyn Fetch,
    url: &str,
    sha256: &str,
    size: u64,
) -> Result<(PathBuf, Vec<u8>), PluginError> {
    let Fetched { path, local } = fetch.fetch(url, sha256, size)?;
    let bytes = std::fs::read(&path).map_err(|error| {
        PluginError::new(codes::FETCH_FAILED, format!("{}: {error}", path.display()))
    })?;
    verify(&bytes, sha256, size, url, local)?;
    Ok((path, bytes))
}

/// The longest plugin log line written; the rest is cut.
const MAX_LOG_BYTES: usize = 2048;

/// Writes a plugin's diagnostic to standard error, where host diagnostics go.
fn log_line(message: &str) {
    let mut end = message.len().min(MAX_LOG_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    eprintln!("plugin: {}", &message[..end]);
}

/// What one plugin call emitted, or why it failed.
type Call = Result<Vec<Record>, ExtractorError>;

/// How the loader calls a plugin, whichever transport it came through.
trait Transport {
    fn describe(&mut self) -> Result<String, PluginError>;
    fn begin(&mut self, context: &[u8]) -> Call;
    fn pass(&mut self, pass: u32, direction: u32) -> Call;
    fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> Call;
    fn audio(&mut self, block: &AudioInput) -> Call;
    fn end(&mut self) -> Call;
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
    plugin: MadeByPlugin,
    transport: Box<dyn Transport>,
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

/// Loads the artifact `policy` picks from `manifest`, fetching it and every resource through
/// `fetch`, to run with `params`.
///
/// # Errors
/// A [`PluginError`] for the first check that fails.
pub fn load(
    manifest: &PluginManifest,
    params: serde_json::Map<String, serde_json::Value>,
    policy: &Policy,
    fetch: &mut dyn Fetch,
) -> Result<PluginExtractor, PluginError> {
    let artifact = select(manifest, policy, TARGET)?;
    let mut resources = HashMap::new();
    for resource in &manifest.resources {
        let (_, bytes) = read_verified(fetch, &resource.url, &resource.sha256, resource.bytes)?;
        resources.insert(resource.name.clone(), bytes);
    }
    let (path, bytes) = read_verified(fetch, &artifact.url, &artifact.sha256, artifact.bytes)?;
    let caps = output_caps(&manifest.descriptor);
    let mut transport: Box<dyn Transport> = match artifact.kind {
        ArtifactKind::Wasm => Box::new(wasm::WasmTransport::new(
            &manifest.id,
            &bytes,
            &artifact.sha256,
            resources,
            caps,
            policy,
        )?),
        ArtifactKind::Cdylib => Box::new(native::NativeTransport::new(
            &fetch.prepare_native(&path)?,
            resources,
            caps,
        )?),
        ArtifactKind::Staticlib => {
            return Err(PluginError::new(
                codes::NO_ARTIFACT,
                "a static library is linked at build time, not loaded",
            ));
        }
    };
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
        params,
        plugin: MadeByPlugin {
            version: manifest.version.clone(),
            artifact_sha256: artifact.sha256.clone(),
        },
        transport,
    })
}

impl PluginExtractor {
    /// What `made_by.plugin` records for data this plugin makes.
    #[must_use]
    pub fn made_by(&self) -> &MadeByPlugin {
        &self.plugin
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

    fn audio(&mut self, block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let records = self.transport.audio(block)?;
        self.deliver(records, out)
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError> {
        let records = self.transport.end()?;
        self.deliver(records, out)
    }
}
