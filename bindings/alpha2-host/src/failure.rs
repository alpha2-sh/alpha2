//! What an export was doing when it failed, and what the failure is evidence
//! of, for the protocol error's `failure`.
//!
//! Errors cross this host's worker threads as strings, so the step a failure
//! came from travels as a tag ([`crate::tag`]) at the front of the message,
//! the way [`crate::protocol::gpu_timeout_tag`] does, and the first marker a message
//! received is the one kept. The main loop's own errors take the step
//! [`FailureContext`] was in. Neither adds a GPU wait or grows with the export.

use std::cell::{Cell, RefCell};

use alpha2_gpu::GpuDiagnosticSnapshot;
use serde_json::{Map, Value, json};

/// A step of an export, in pipeline order. Steps overlap in time, so this
/// names the step an event belongs to, not a global state.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ExportPhase {
    /// Before the first frame: conform, project checks, opening the output.
    #[default]
    Prepare,
    Decode,
    Upload,
    Render,
    Readback,
    Encode,
    /// Flushing the encoders and writing the container's tail.
    Mux,
    Cleanup,
}

impl ExportPhase {
    pub const ALL: [Self; 8] = [
        Self::Prepare,
        Self::Decode,
        Self::Upload,
        Self::Render,
        Self::Readback,
        Self::Encode,
        Self::Mux,
        Self::Cleanup,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Decode => "decode",
            Self::Upload => "upload",
            Self::Render => "render",
            Self::Readback => "readback",
            Self::Encode => "encode",
            Self::Mux => "mux",
            Self::Cleanup => "cleanup",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|phase| phase.as_str() == value)
    }
}

crate::tag::tags! {
    /// How a phase tag opens.
    fn phase_tag_open = "phase=";
    /// How an evidence tag opens.
    fn evidence_tag_open = "evidence=";
}

/// `message`, marked as coming from `phase` (and `frame`, when one applies),
/// unless it already carries a phase: the first step to fail is the cause.
pub fn tag_phase(phase: ExportPhase, frame: Option<u64>, message: String) -> String {
    let open = phase_tag_open();
    if message.contains(open) {
        return message;
    }
    match frame {
        Some(frame) => format!("{open}{} frame={frame}] {message}", phase.as_str()),
        None => format!("{open}{}] {message}", phase.as_str()),
    }
}

/// The phase and frame `message` was first marked with.
pub fn phase_tag(message: &str) -> Option<(ExportPhase, Option<u64>)> {
    let start = message.find(phase_tag_open())?;
    let end = crate::tag::end(message, start)?;
    let body = &message[start + phase_tag_open().len()..end - 2];
    let mut fields = body.split(' ');
    let phase = fields.next().and_then(ExportPhase::parse)?;
    let frame = fields
        .find_map(|field| field.strip_prefix("frame="))
        .and_then(|frame| frame.parse().ok());
    Some((phase, frame))
}

/// The failure classes a message can carry as typed evidence: the ones no
/// protocol code names.
/// `HARDWARE_STALL` is not a class: it says a stalled decoder or encoder
/// held a hardware session, which is what makes the stall's `resource` the
/// GPU ([`hardware_stall`]).
const EVIDENCE_CLASSES: [&str; 4] = [
    "OUT_OF_MEMORY",
    "ALLOCATION_FAILED",
    "VALIDATION_FAILED",
    "HARDWARE_STALL",
];

/// A stall's `message`, carrying `HARDWARE_STALL` evidence when the stuck
/// decoder or encoder held a hardware session (`hardware`). The message
/// says so in words too, for whoever reads it; only the evidence decides.
pub fn hardware_stall(hardware: bool, message: String) -> String {
    if hardware {
        evidence_tag("HARDWARE_STALL", message)
    } else {
        message
    }
}

/// `message`, carrying `class` as typed evidence of its failure class,
/// unless it already carries some: the first is the cause's. The place that
/// failed knows it (a GPU call's status), and errors cross this host's
/// threads as strings, so it travels in them the way a code tag does.
pub fn evidence_tag(class: &'static str, message: String) -> String {
    debug_assert!(EVIDENCE_CLASSES.contains(&class), "{class} is not evidence");
    if message.contains(evidence_tag_open()) {
        return message;
    }
    format!("{}{class}] {message}", evidence_tag_open())
}

/// The first evidence `message` was tagged with, and `message` without any
/// evidence tag.
pub fn take_evidence_tag(mut message: String) -> (Option<&'static str>, String) {
    let mut evidence = None;
    while let Some(start) = message.find(evidence_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        let name = &message[start + evidence_tag_open().len()..end - 2];
        if evidence.is_none() {
            evidence = EVIDENCE_CLASSES
                .iter()
                .copied()
                .find(|known| *known == name);
        }
        message.replace_range(start..end, "");
    }
    (evidence, message)
}

/// `message` without any phase marker.
pub fn strip_phase_tags(mut message: String) -> String {
    while let Some(start) = message.find(phase_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        message.replace_range(start..end, "");
    }
    message
}

/// What the running export is doing, and the facts a failure report needs.
///
/// Updated from the main loop with plain stores, so recording it costs
/// nothing a frame would notice.
#[derive(Default)]
pub struct FailureContext {
    phase: Cell<ExportPhase>,
    frame: Cell<Option<u64>>,
    facts: RefCell<Map<String, Value>>,
    /// The running export's provider sources, whose diagnostics its report
    /// carries.
    providers: RefCell<Option<std::sync::Arc<crate::providers::ProviderMedia>>>,
    /// The running export's decode policy and what its decoders reported.
    decoding: RefCell<std::sync::Arc<crate::recovery::ExportDecoding>>,
}

impl std::fmt::Debug for FailureContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FailureContext")
            .field("phase", &self.phase)
            .field("frame", &self.frame)
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

impl FailureContext {
    /// Starts a new export's record.
    pub fn begin(&self) {
        self.phase.set(ExportPhase::Prepare);
        self.frame.set(None);
        self.facts.borrow_mut().clear();
        self.providers.borrow_mut().take();
        *self.decoding.borrow_mut() = std::sync::Arc::default();
    }

    /// How this export decodes damaged input: every decoder it opens takes
    /// this, and what they report is part of its result and failure report.
    pub fn begin_decoding(&self, decoding: crate::recovery::ExportDecoding) {
        *self.decoding.borrow_mut() = std::sync::Arc::new(decoding);
    }

    /// The running export's decode policy and report, for a decoder it opens.
    pub fn decoding(&self) -> std::sync::Arc<crate::recovery::ExportDecoding> {
        std::sync::Arc::clone(&self.decoding.borrow())
    }

    /// What the export's decoders reported, per source: `[]` when nothing.
    pub fn decode_diagnostics(&self) -> Value {
        self.decoding.borrow().diagnostics_json()
    }

    /// The provider sources this export draws from now: their diagnostics
    /// are part of its result and of its failure report.
    pub fn watch_providers(&self, providers: std::sync::Arc<crate::providers::ProviderMedia>) {
        *self.providers.borrow_mut() = Some(providers);
    }

    /// What the export's provider sources reported, and how many distinct
    /// reports were only counted: none before any was opened.
    pub fn provider_diagnostics(&self) -> (Value, u64) {
        self.providers
            .borrow()
            .as_ref()
            .map_or_else(|| (json!([]), 0), |providers| providers.diagnostics_json())
    }

    /// The main loop moved on to `phase`, for `frame` when one applies.
    pub fn enter(&self, phase: ExportPhase, frame: Option<u64>) {
        self.phase.set(phase);
        self.frame.set(frame);
    }

    /// Records a fact about this export for its failure report, such as the
    /// output path or encoder it chose. A later note of the same key wins.
    pub fn note(&self, key: &str, value: Value) {
        self.facts.borrow_mut().insert(key.to_owned(), value);
    }

    /// The phase the main loop is in, and the frame when one applies.
    pub fn position(&self) -> (ExportPhase, Option<u64>) {
        (self.phase.get(), self.frame.get())
    }

    /// The noted facts, plus `providerDiagnostics` and `providerDiagnosticsOmitted` when any
    /// provider source reported something, and `decodeDiagnostics` when any decoder did.
    pub fn facts(&self) -> Map<String, Value> {
        let mut facts = self.facts.borrow().clone();
        let decode = self.decode_diagnostics();
        if decode.as_array().is_some_and(|list| !list.is_empty()) {
            facts.insert("decodeDiagnostics".to_owned(), decode);
        }
        let (diagnostics, omitted) = self.provider_diagnostics();
        if diagnostics.as_array().is_some_and(|list| !list.is_empty()) {
            facts.insert("providerDiagnostics".to_owned(), diagnostics);
            facts.insert("providerDiagnosticsOmitted".to_owned(), json!(omitted));
        }
        facts
    }
}

/// A failure's class, and the memory region its evidence names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailureClass {
    pub code: &'static str,
    /// `gpu`, `cpu`, or `unknown`: a region is named only on typed evidence.
    pub resource: &'static str,
}

/// Classifies a failure from what is known for certain: the protocol code,
/// typed device diagnostics, the evidence the place that failed tagged
/// (`native`, from [`evidence_tag`]: a GPU call's own out-of-memory status,
/// or the error a frame's own scope caught when the GPU rejected its work,
/// which the device's diagnostics never see), and markers this host's own
/// errors carry.
///
/// A lost device the device itself reported, or a GPU call's status named,
/// is the class whatever else is known. A readback's missing canary also
/// reads as a lost device, but only when nothing typed says otherwise: the
/// canary is missing too when the GPU rejected the submission for an error
/// the device reported, and the device is fine then.
///
/// Error text is read only for markers the host or its libraries emit
/// verbatim (the readback's own message, `ENOMEM`/`ENOSPC` strings from the
/// C runtime); a readback failure is never read as memory exhaustion unless
/// the device itself reported one.
pub fn classify_failure(
    host_code: &str,
    message: &str,
    native: Option<&str>,
    phase: ExportPhase,
    gpu: &GpuDiagnosticSnapshot,
) -> FailureClass {
    let lower = message.to_ascii_lowercase();
    let class = |code, resource| FailureClass { code, resource };
    let evidence = gpu.failure_code();
    match host_code {
        "CANCELLED" => return class("CANCELLED", "unknown"),
        "RESOURCE_LIMIT" => return class("RESOURCE_LIMIT", "unknown"),
        "GPU_TIMEOUT" => return class("GPU_TIMEOUT", "gpu"),
        // Every session of the hardware encoder is held by other exports; it frees up when they
        // end.
        "ENCODER_BUSY" => return class("RESOURCE_BUSY", "gpu"),
        // The export's own submission ran long enough to stop the GPU; another attempt on the GPU
        // stops it again.
        "GPU_DEVICE_HUNG" => return class("DEVICE_HUNG", "gpu"),
        // Blocked inside FFmpeg: with a hardware session open it is waiting on the GPU, otherwise
        // most likely on its input or output. The stall's evidence says which, never its words.
        "DECODE_STALLED" | "ENCODE_STALLED" => {
            return class(
                "GPU_TIMEOUT",
                if native == Some("HARDWARE_STALL") {
                    "gpu"
                } else {
                    "unknown"
                },
            );
        }
        "UNSUPPORTED_PROTOCOL"
        | "INVALID_INPUT"
        | "INVALID_REQUEST"
        | "CONFORM_FAILED"
        | "MISSING_INPUT"
        | "INCOMPLETE_COMPOSITION"
        | "EFFECT_ERROR"
        | "COPY_INELIGIBLE"
        | "GPU_OUTPUT_UNAVAILABLE"
        | "HARDWARE_DECODE_UNAVAILABLE"
        // Another export is writing the same output; this request meets it again until that one
        // ends.
        | "OUTPUT_IN_USE" => return class("INVALID_INPUT", "unknown"),
        _ => {}
    }
    let missing_canary = message.contains(alpha2_render::GPU_WORK_DROPPED_MESSAGE);
    if evidence == Some("DEVICE_LOST") || (host_code == "GPU_DEVICE_LOST" && !missing_canary) {
        return class("DEVICE_LOST", "gpu");
    }
    match native {
        Some("OUT_OF_MEMORY") => return class("OUT_OF_MEMORY", "gpu"),
        Some("ALLOCATION_FAILED") => return class("ALLOCATION_FAILED", "cpu"),
        Some("VALIDATION_FAILED") => return class("VALIDATION_FAILED", "gpu"),
        _ => {}
    }
    let gpu_phase = matches!(
        phase,
        ExportPhase::Upload | ExportPhase::Render | ExportPhase::Readback
    );
    if evidence == Some("OUT_OF_MEMORY")
        && (gpu_phase || missing_canary || lower.contains("readback"))
    {
        return class("OUT_OF_MEMORY", "gpu");
    }
    if evidence == Some("VALIDATION_FAILED") && missing_canary {
        return class("VALIDATION_FAILED", "gpu");
    }
    if host_code == "GPU_DEVICE_LOST" {
        return class("DEVICE_LOST", "gpu");
    }
    if lower.contains("gpu texture readback failed") || lower.contains("gpu readback") {
        return class("READBACK_FAILED", "unknown");
    }
    if evidence == Some("VALIDATION_FAILED") && gpu_phase {
        return class("VALIDATION_FAILED", "gpu");
    }
    if lower.contains("cannot allocate memory") || lower.contains("memory allocation failed") {
        return class("ALLOCATION_FAILED", "cpu");
    }
    if storage_exhausted(message) {
        return class("STORAGE_QUOTA_EXCEEDED", "unknown");
    }
    if host_code == "IO_ERROR"
        || host_code == "OUTPUT_PUBLISH_FAILED"
        || (matches!(phase, ExportPhase::Mux) && (lower.contains("i/o") || lower.contains("write")))
    {
        return class("OUTPUT_IO_FAILED", "unknown");
    }
    class("UNKNOWN", "unknown")
}

/// The `errno` values for a full device and an exhausted quota (`ENOSPC`,
/// `EDQUOT`) on the platform this host runs on, where its C runtime has them.
#[cfg(target_os = "linux")]
pub const STORAGE_ERRNOS: &[i64] = &[28, 122];
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub const STORAGE_ERRNOS: &[i64] = &[28, 69];
// The CRT has no EDQUOT; FFmpeg's own table, used there, names neither number by quota.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)))]
pub const STORAGE_ERRNOS: &[i64] = &[28];

/// The numbers `std::io` reports a full disk or quota as, `(os error N)`:
/// Win32 errors on Windows (`ERROR_HANDLE_DISK_FULL`, `ERROR_DISK_FULL`),
/// `errno` elsewhere.
#[cfg(windows)]
const STORAGE_OS_ERRORS: &[i64] = &[39, 112];
#[cfg(not(windows))]
const STORAGE_OS_ERRORS: &[i64] = STORAGE_ERRNOS;

/// The numbers `message` carries after `prefix` and before `)`: `FFmpeg`'s
/// `AVERROR(-28)`, as `rsmpeg` writes it, or `std::io`'s `(os error 28)`.
fn numbers_after<'a>(message: &'a str, prefix: &'a str) -> impl Iterator<Item = i64> + 'a {
    message.match_indices(prefix).filter_map(move |(start, _)| {
        let rest = &message[start + prefix.len()..];
        rest[..rest.find(')')?].trim().parse::<i64>().ok()
    })
}

/// Whether `message` reports a full device or an exhausted quota: by the
/// `errno` `FFmpeg` or the file system returned, whatever the C runtime's words
/// for it, or by those words in the spellings glibc (`Disk quota exceeded`)
/// and macOS (`Disc quota exceeded`) use.
fn storage_exhausted(message: &str) -> bool {
    let averror = numbers_after(message, "AVERROR(").any(|code| STORAGE_ERRNOS.contains(&-code));
    let os = numbers_after(message, "(os error ").any(|code| STORAGE_OS_ERRORS.contains(&code));
    let lower = message.to_ascii_lowercase();
    averror
        || os
        || lower.contains("no space left on device")
        || lower.contains("disk quota exceeded")
        || lower.contains("disc quota exceeded")
}

/// The message a panic carried, as `panic!` and the standard library's
/// checks give it.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// The readback step a message names, from the renderer's own wording.
fn readback_stage(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once("GPU texture readback failed: ")?;
    Some(
        rest.split(|c: char| !(c.is_ascii_lowercase() || c == '-'))
            .next()?,
    )
    .filter(|stage| !stage.is_empty())
}

/// The protocol's `failure` object for one failed export. A `RESOURCE_BUSY`
/// one says how long to wait before asking again, as `retryAfterMs`.
pub fn failure_json(
    host_code: &str,
    message: &str,
    native: Option<&str>,
    (phase, frame): (ExportPhase, Option<u64>),
    gpu: &GpuDiagnosticSnapshot,
    diagnostics: Map<String, Value>,
) -> Value {
    let class = classify_failure(host_code, message, native, phase, gpu);
    let mut failure = json!({
        "code": class.code,
        "origin": "native-host",
        "operation": "export",
        "backend": "native",
        "phase": phase.as_str(),
        "stage": readback_stage(message),
        "frameIndex": frame,
        "resource": class.resource,
        "hostCode": host_code,
        "diagnostics": Value::Object(diagnostics),
    });
    if class.code == "RESOURCE_BUSY" {
        failure["retryAfterMs"] = json!(crate::encoder_refusal::ENCODER_BUSY_RETRY_AFTER_MS);
    }
    failure
}

/// The `failure` of an error this host reports before it has a device: a
/// start that failed (`GPU_INIT_FAILED`, `HARDWARE_DECODE_UNAVAILABLE`, a
/// decode mode it cannot read). The client was waiting on an export, which
/// this ends before anything was prepared; only the build is known.
pub fn startup_failure_json(host_code: &str, message: &str, native: Option<&str>) -> Value {
    let mut diagnostics = Map::new();
    diagnostics.insert(
        "environment".to_owned(),
        json!({
            "hostVersion": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "features": {
                "hwdecode": cfg!(feature = "hwdecode"),
                "hwencode": cfg!(feature = "hwencode"),
                "extensions": cfg!(feature = "extensions"),
            },
        }),
    );
    failure_json(
        host_code,
        message,
        native,
        (ExportPhase::Prepare, None),
        &GpuDiagnosticSnapshot::default(),
        diagnostics,
    )
}

/// A diagnostic snapshot as JSON: the events, oldest first, how many were
/// dropped, whether the device was lost, and what they are evidence of.
pub fn gpu_json(snapshot: &GpuDiagnosticSnapshot) -> Value {
    json!({
        "events": snapshot.entries.iter().map(|entry| match entry {
            alpha2_gpu::GpuDiagnostic::UncapturedError { kind, message } => json!({
                "event": entry.event(),
                "kind": kind.as_str(),
                "code": entry.failure_code(),
                "message": message,
            }),
            alpha2_gpu::GpuDiagnostic::DeviceLost { reason, message } => json!({
                "event": entry.event(),
                "reason": reason,
                "code": entry.failure_code(),
                "message": message,
            }),
            _ => json!({ "event": entry.event(), "message": entry.to_string() }),
        }).collect::<Vec<_>>(),
        "dropped": snapshot.dropped,
        "deviceLost": snapshot.device_lost,
        "evidence": snapshot.failure_code(),
    })
}

/// The process's resident memory now and at its peak, as the OS reports
/// them ([`alpha2_os::process_memory`]: Linux, macOS, and Windows); `null`
/// fields where it does not.
pub fn process_memory_json() -> Value {
    let memory = alpha2_os::process_memory();
    json!({
        "residentBytes": memory.resident_bytes,
        "peakResidentBytes": memory.peak_resident_bytes,
    })
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
