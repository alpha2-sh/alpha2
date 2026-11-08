//! What an export was doing when it failed, and what the failure is evidence of.
//!
//! Errors cross worker threads as strings, so the failing step travels as a tag
//! ([`crate::tag`]) and the first one a message received is kept. Neither adds a GPU wait or
//! grows with the export.

use std::cell::{Cell, RefCell};

use alpha2_gpu::GpuDiagnosticSnapshot;
use serde_json::{Map, Value, json};

/// Steps overlap in time, so this names the step an event belongs to, not a global state.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ExportPhase {
    /// Conform, project checks, opening the output.
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
    fn phase_tag_open = "phase=";
    fn evidence_tag_open = "evidence=";
}

/// Keeps an existing phase tag, since the first step to fail is the cause.
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

/// Failure classes no protocol code names. `HARDWARE_STALL` is not a class; it makes a stall's
/// `resource` the GPU ([`hardware_stall`]).
const EVIDENCE_CLASSES: [&str; 4] = [
    "OUT_OF_MEMORY",
    "ALLOCATION_FAILED",
    "VALIDATION_FAILED",
    "HARDWARE_STALL",
];

/// Only the evidence decides; the words are for whoever reads the message.
pub fn hardware_stall(hardware: bool, message: String) -> String {
    if hardware {
        evidence_tag("HARDWARE_STALL", message)
    } else {
        message
    }
}

/// Keeps existing evidence, since the first is the cause's.
pub fn evidence_tag(class: &'static str, message: String) -> String {
    debug_assert!(EVIDENCE_CLASSES.contains(&class), "{class} is not evidence");
    if message.contains(evidence_tag_open()) {
        return message;
    }
    format!("{}{class}] {message}", evidence_tag_open())
}

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

pub fn strip_phase_tags(mut message: String) -> String {
    while let Some(start) = message.find(phase_tag_open()) {
        let Some(end) = crate::tag::end(&message, start) else {
            break;
        };
        message.replace_range(start..end, "");
    }
    message
}

/// Updated from the main loop with plain stores, so recording costs nothing a frame would notice.
#[derive(Default)]
pub struct FailureContext {
    phase: Cell<ExportPhase>,
    frame: Cell<Option<u64>>,
    facts: RefCell<Map<String, Value>>,
    providers: RefCell<Option<std::sync::Arc<alpha2_host_core::providers::ProviderMedia>>>,
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
    pub fn begin(&self) {
        self.phase.set(ExportPhase::Prepare);
        self.frame.set(None);
        self.facts.borrow_mut().clear();
        self.providers.borrow_mut().take();
        *self.decoding.borrow_mut() = std::sync::Arc::default();
    }

    pub fn begin_decoding(&self, decoding: crate::recovery::ExportDecoding) {
        *self.decoding.borrow_mut() = std::sync::Arc::new(decoding);
    }

    pub fn decoding(&self) -> std::sync::Arc<crate::recovery::ExportDecoding> {
        std::sync::Arc::clone(&self.decoding.borrow())
    }

    pub fn decode_diagnostics(&self) -> Value {
        self.decoding.borrow().diagnostics_json()
    }

    pub fn watch_providers(
        &self,
        providers: std::sync::Arc<alpha2_host_core::providers::ProviderMedia>,
    ) {
        *self.providers.borrow_mut() = Some(providers);
    }

    /// Returns the reports and how many distinct reports were only counted.
    pub fn provider_diagnostics(&self) -> (Value, u64) {
        self.providers
            .borrow()
            .as_ref()
            .map_or_else(|| (json!([]), 0), |providers| providers.diagnostics_json())
    }

    pub fn enter(&self, phase: ExportPhase, frame: Option<u64>) {
        self.phase.set(phase);
        self.frame.set(frame);
    }

    /// A later note of the same key wins.
    pub fn note(&self, key: &str, value: Value) {
        self.facts.borrow_mut().insert(key.to_owned(), value);
    }

    pub fn position(&self) -> (ExportPhase, Option<u64>) {
        (self.phase.get(), self.frame.get())
    }

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailureClass {
    pub code: &'static str,
    /// `gpu`, `cpu`, or `unknown`: a region is named only on typed evidence.
    pub resource: &'static str,
}

/// Classifies a failure from what is known for certain: the protocol code, typed device
/// diagnostics, `native` evidence ([`evidence_tag`]), and the host's own markers.
///
/// A lost device the device or a GPU call reported wins. A readback's missing canary reads as a
/// lost device only when nothing typed says otherwise, because the canary is also missing when the
/// GPU rejected the submission and the device is fine.
///
/// Error text is read only for markers emitted verbatim (the readback's own message, C runtime
/// `ENOMEM`/`ENOSPC` strings); a readback failure is never memory exhaustion unless the device
/// reported one.
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
        // Every session is held by other exports; it frees up when they end.
        "ENCODER_BUSY" => return class("RESOURCE_BUSY", "gpu"),
        // Never started: the host was retiring, and another host runs it.
        "HOST_RETIRING" => return class("HOST_EXITED", "unknown"),
        // The export's own submission stopped the GPU; another attempt on the GPU stops it again.
        "GPU_DEVICE_HUNG" => return class("DEVICE_HUNG", "gpu"),
        // With a hardware session open the stall waits on the GPU, otherwise most likely on I/O.
        // The stall's evidence says which, never its words.
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
        // Another export is writing the same output; retrying meets it again until that one ends.
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

/// `ENOSPC` and `EDQUOT` where the platform's C runtime has them.
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

/// How `std::io` reports a full disk, `(os error N)`: `ERROR_HANDLE_DISK_FULL` and
/// `ERROR_DISK_FULL`.
#[cfg(windows)]
const STORAGE_OS_ERRORS: &[i64] = &[39, 112];
#[cfg(not(windows))]
const STORAGE_OS_ERRORS: &[i64] = STORAGE_ERRNOS;

/// Reads `FFmpeg`'s `AVERROR(-28)` (as `rsmpeg` writes it) or `std::io`'s `(os error 28)`.
fn numbers_after<'a>(message: &'a str, prefix: &'a str) -> impl Iterator<Item = i64> + 'a {
    message.match_indices(prefix).filter_map(move |(start, _)| {
        let rest = &message[start + prefix.len()..];
        rest[..rest.find(')')?].trim().parse::<i64>().ok()
    })
}

/// Matches the errno whatever the C runtime's words, or the words in glibc's
/// (`Disk quota exceeded`) and macOS's (`Disc quota exceeded`) spellings.
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

pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

fn readback_stage(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once("GPU texture readback failed: ")?;
    Some(
        rest.split(|c: char| !(c.is_ascii_lowercase() || c == '-'))
            .next()?,
    )
    .filter(|stage| !stage.is_empty())
}

/// A `RESOURCE_BUSY` failure also carries `retryAfterMs`.
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

/// For a start that failed before any device exists; only the build is known.
pub fn startup_failure_json(host_code: &str, message: &str, native: Option<&str>) -> Value {
    let mut diagnostics = Map::new();
    diagnostics.insert(
        "environment".to_owned(),
        json!({
            "hostVersion": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "osVersion": alpha2_os::os_version(),
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

/// `null` fields where the OS does not report a value.
pub fn process_memory_json() -> Value {
    let memory = alpha2_os::process_memory();
    json!({
        "residentBytes": memory.resident_bytes,
        "peakResidentBytes": memory.peak_resident_bytes,
        "footprintBytes": memory.footprint_bytes,
        "peakFootprintBytes": memory.peak_footprint_bytes,
        "capacity": memory_capacity_json(),
    })
}

pub fn memory_capacity_json() -> Value {
    let capacity = crate::memory::machine_capacity();
    json!({
        "physicalBytes": capacity.physical_bytes,
        "availableBytes": capacity.available_bytes,
        "limitBytes": capacity.limit_bytes,
        "limitSource": capacity.limit_source,
        "usedInLimitBytes": capacity.used_in_limit_bytes,
    })
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
