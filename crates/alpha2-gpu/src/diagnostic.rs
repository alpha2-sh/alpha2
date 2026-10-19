use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

/// What class of error the driver reported.
///
/// WebGPU's three error filters. Only [`Self::OutOfMemory`] is evidence that
/// an allocation failed; the others are never read as memory pressure.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GpuErrorKind {
    /// An allocation the driver was asked for could not be made.
    OutOfMemory,
    /// Invalid use of the API: a bug or bad data, not a resource shortage.
    Validation,
    /// Any other failure the implementation reports, including limits it hit.
    Internal,
}

impl GpuErrorKind {
    /// The kind `error` belongs to.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub const fn of(error: &wgpu::Error) -> Self {
        match error {
            wgpu::Error::OutOfMemory { .. } => Self::OutOfMemory,
            wgpu::Error::Validation { .. } => Self::Validation,
            wgpu::Error::Internal { .. } => Self::Internal,
        }
    }

    /// Stable lowercase name for logs and protocols.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OutOfMemory => "out-of-memory",
            Self::Validation => "validation",
            Self::Internal => "internal",
        }
    }
}

/// A device-level GPU event that no call return value reports.
///
/// These arrive asynchronously from the driver, so an embedding application
/// observes them through [`GpuContext::set_diagnostic_handler`] rather than
/// from the call that triggered them.
///
/// [`GpuContext::set_diagnostic_handler`]: crate::GpuContext::set_diagnostic_handler
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GpuDiagnostic {
    /// The driver reported an error that no pending call returned.
    UncapturedError {
        /// Which of WebGPU's error classes it was.
        kind: GpuErrorKind,
        /// Driver-supplied description.
        message: String,
    },
    /// The device was lost and can no longer serve work.
    DeviceLost {
        /// Loss reason reported by `wgpu`, or the browser event that lost it.
        reason: String,
        /// Driver-supplied description.
        message: String,
    },
}

impl GpuDiagnostic {
    /// The diagnostic for an uncaptured `wgpu` error, keeping its class.
    ///
    /// `wgpu` spells a validation or internal error's sources out in its
    /// description, but writes an out-of-memory error as the bare words "Out
    /// of Memory"; its sources name the call, the resource label, and the
    /// driver's cause, so they are appended, each after `": "`. A browser's
    /// report has only `wgpu`'s `<WebGPU Error>` placeholder there, which
    /// names nothing and is left out.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn uncaptured(error: &wgpu::Error) -> Self {
        let mut message = error.to_string();
        if matches!(error, wgpu::Error::OutOfMemory { .. }) {
            let mut source = std::error::Error::source(error);
            while let Some(cause) = source {
                let text = cause.to_string();
                if !text.is_empty() && text != "<WebGPU Error>" {
                    message.push_str(": ");
                    message.push_str(&text);
                }
                source = cause.source();
            }
        }
        Self::UncapturedError {
            kind: GpuErrorKind::of(error),
            message,
        }
    }

    /// The export failure class this event is evidence of:
    /// `OUT_OF_MEMORY`, `VALIDATION_FAILED`, `DEVICE_LOST`, or `UNKNOWN` for
    /// an internal error, which names no cause.
    #[must_use]
    pub const fn failure_code(&self) -> &'static str {
        match self {
            Self::UncapturedError {
                kind: GpuErrorKind::OutOfMemory,
                ..
            } => "OUT_OF_MEMORY",
            Self::UncapturedError {
                kind: GpuErrorKind::Validation,
                ..
            } => "VALIDATION_FAILED",
            Self::UncapturedError {
                kind: GpuErrorKind::Internal,
                ..
            } => "UNKNOWN",
            Self::DeviceLost { .. } => "DEVICE_LOST",
        }
    }

    /// Stable lowercase event name: `uncaptured-error` or `device-lost`.
    #[must_use]
    pub const fn event(&self) -> &'static str {
        match self {
            Self::UncapturedError { .. } => "uncaptured-error",
            Self::DeviceLost { .. } => "device-lost",
        }
    }

    fn truncated(self) -> Self {
        match self {
            Self::UncapturedError { kind, message } => Self::UncapturedError {
                kind,
                message: truncate(message),
            },
            Self::DeviceLost { reason, message } => Self::DeviceLost {
                reason: truncate(reason),
                message: truncate(message),
            },
        }
    }
}

impl fmt::Display for GpuDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UncapturedError { kind, message } => {
                write!(
                    formatter,
                    "uncaptured GPU {} error: {message}",
                    kind.as_str()
                )
            }
            Self::DeviceLost { reason, message } => {
                write!(formatter, "GPU device lost ({reason}): {message}")
            }
        }
    }
}

/// Longest message a [`GpuDiagnosticLog`] keeps, in bytes. A validation
/// error can quote a whole shader; the log only needs enough to identify it.
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1_024;

/// What a cut message ends with.
const ELLIPSIS: &str = "...";

/// `message` cut to [`MAX_DIAGNOSTIC_MESSAGE_BYTES`] on a char boundary,
/// with an ellipsis where it was cut, in a buffer of its own length: the log
/// keeps what it returns, so it must not keep the driver's whole buffer too.
fn truncate(mut message: String) -> String {
    if message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        message.shrink_to_fit();
        return message;
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut kept = String::with_capacity(end + ELLIPSIS.len());
    kept.push_str(&message[..end]);
    kept.push_str(ELLIPSIS);
    kept
}

/// The most recent [`GpuDiagnostic`]s, in arrival order, in fixed memory:
/// at most `capacity` events, each text at most
/// [`MAX_DIAGNOSTIC_MESSAGE_BYTES`] and an ellipsis.
///
/// A failure report reads it to connect an error a call returned (a readback
/// that failed to map carries no cause: `wgpu::BufferAsyncError` is a unit)
/// with what the device reported around it. Its size never grows
/// with the events, because it is consulted exactly when memory may be short.
#[derive(Debug)]
pub struct GpuDiagnosticLog {
    state: Mutex<LogState>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct LogState {
    entries: VecDeque<GpuDiagnostic>,
    dropped: u64,
    device_lost: bool,
    out_of_memory: bool,
}

/// What a [`GpuDiagnosticLog`] held when it was read.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GpuDiagnosticSnapshot {
    /// The kept events, oldest first.
    pub entries: Vec<GpuDiagnostic>,
    /// Events that arrived and were pushed out by later ones.
    pub dropped: u64,
    /// Whether any event, kept or dropped, was a device loss.
    pub device_lost: bool,
    /// Whether any event since the log was last cleared, kept or dropped,
    /// was an out-of-memory report.
    ///
    /// An allocation that ran out leaves an invalid resource behind, and
    /// every later use of it is reported as a validation error, so the
    /// report that names the cause is the first to be pushed out.
    pub out_of_memory: bool,
}

impl GpuDiagnosticSnapshot {
    /// The failure class the strongest evidence here names, if any: a device
    /// loss first, since everything after it fails too; then an out-of-memory
    /// report, kept or dropped; then a validation error. Internal errors
    /// name nothing.
    #[must_use]
    pub fn failure_code(&self) -> Option<&'static str> {
        if self.device_lost {
            return Some("DEVICE_LOST");
        }
        if self.out_of_memory {
            return Some("OUT_OF_MEMORY");
        }
        self.entries
            .iter()
            .any(|entry| entry.failure_code() == "VALIDATION_FAILED")
            .then_some("VALIDATION_FAILED")
    }
}

impl GpuDiagnosticLog {
    /// Default number of events a log keeps.
    pub const DEFAULT_CAPACITY: usize = 16;

    /// An empty log keeping at most `capacity` events (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(LogState::default()),
            capacity: capacity.max(1),
        }
    }

    /// Records `diagnostic`, pushing out the oldest event when full.
    pub fn push(&self, diagnostic: GpuDiagnostic) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.device_lost |= matches!(diagnostic, GpuDiagnostic::DeviceLost { .. });
        state.out_of_memory |= diagnostic.failure_code() == "OUT_OF_MEMORY";
        if state.entries.len() == self.capacity {
            state.entries.pop_front();
            state.dropped += 1;
        }
        state.entries.push_back(diagnostic.truncated());
    }

    /// A copy of what the log holds now.
    #[must_use]
    pub fn snapshot(&self) -> GpuDiagnosticSnapshot {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        GpuDiagnosticSnapshot {
            entries: state.entries.iter().cloned().collect(),
            dropped: state.dropped,
            device_lost: state.device_lost,
            out_of_memory: state.out_of_memory,
        }
    }

    /// Forgets the events, so the next snapshot covers only what follows,
    /// out-of-memory reports included. A device loss is kept: a lost device
    /// stays lost.
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.clear();
        state.dropped = 0;
        state.out_of_memory = false;
    }

    /// Whether a device loss was ever recorded.
    #[must_use]
    pub fn device_lost(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .device_lost
    }
}

impl Default for GpuDiagnosticLog {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

/// Shared handler invoked for every [`GpuDiagnostic`].
///
/// The driver may call it from an arbitrary thread, so it must not block.
pub type GpuDiagnosticHandler = Arc<dyn Fn(GpuDiagnostic) + Send + Sync + 'static>;

/// Interior-mutable slot the device callbacks forward through.
///
/// Devices take their callbacks once at creation, so the slot is what lets a
/// caller install or replace a handler on an already-created context.
pub(crate) struct DiagnosticHook {
    handler: Mutex<Option<GpuDiagnosticHandler>>,
    strict: bool,
}

/// Whether an uncaptured error with no handler installed panics, as wgpu's own default does,
/// instead of being dropped: in debug builds, so every test that creates a context fails on the
/// call that made the error, and wherever `ALPHA2_GPU_STRICT=1`.
fn strict_by_default() -> bool {
    cfg!(debug_assertions)
        || std::env::var_os("ALPHA2_GPU_STRICT").is_some_and(|value| value == "1")
}

impl DiagnosticHook {
    pub(crate) fn new() -> Self {
        Self::with_strict(strict_by_default())
    }

    pub(crate) fn with_strict(strict: bool) -> Self {
        Self {
            handler: Mutex::new(None),
            strict,
        }
    }

    pub(crate) fn set(&self, handler: Option<GpuDiagnosticHandler>) {
        *self.handler.lock().unwrap_or_else(PoisonError::into_inner) = handler;
    }

    /// Emits without holding the lock across the call, so a handler that
    /// re-enters the context cannot deadlock.
    ///
    /// With no handler installed a strict hook panics on an uncaptured error,
    /// on the thread whose call made it; device loss is still dropped, since
    /// dropping a device reports it too.
    ///
    /// Only the native device callbacks emit; the browser owns its own
    /// callbacks, as [`GpuContext::set_diagnostic_handler`] documents.
    ///
    /// [`GpuContext::set_diagnostic_handler`]: crate::GpuContext::set_diagnostic_handler
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn emit(&self, diagnostic: GpuDiagnostic) {
        let handler = self
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(handler) = handler {
            handler(diagnostic);
        } else if self.strict && matches!(diagnostic, GpuDiagnostic::UncapturedError { .. }) {
            panic!(
                "{diagnostic}\n(strict GPU diagnostics: no handler is installed; capture the error with an error scope or install one with GpuContext::set_diagnostic_handler)"
            );
        }
    }
}

impl fmt::Debug for DiagnosticHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let installed = self
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        formatter
            .debug_struct("DiagnosticHook")
            .field("installed", &installed)
            .field("strict", &self.strict)
            .finish()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn an_uninstalled_lenient_hook_drops_diagnostics() {
        let hook = DiagnosticHook::with_strict(false);
        hook.emit(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Internal,
            message: "ignored".to_owned(),
        });
    }

    #[test]
    #[should_panic(expected = "uncaptured GPU validation error: unseen")]
    fn an_uninstalled_strict_hook_panics_on_an_uncaptured_error() {
        DiagnosticHook::with_strict(true).emit(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Validation,
            message: "unseen".to_owned(),
        });
    }

    #[test]
    fn a_strict_hook_drops_device_loss_and_defers_to_a_handler() {
        let hook = DiagnosticHook::with_strict(true);
        hook.emit(GpuDiagnostic::DeviceLost {
            reason: "Destroyed".to_owned(),
            message: "device dropped".to_owned(),
        });
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        hook.set(Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::Release);
        })));
        hook.emit(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Validation,
            message: "handled".to_owned(),
        });
        assert_eq!(count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn the_installed_handler_receives_every_diagnostic() {
        let hook = DiagnosticHook::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        hook.set(Some(Arc::new(move |diagnostic: GpuDiagnostic| {
            recorded
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(diagnostic.to_string());
        })));
        hook.emit(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Validation,
            message: "validation".to_owned(),
        });
        hook.emit(GpuDiagnostic::DeviceLost {
            reason: "Destroyed".to_owned(),
            message: "device dropped".to_owned(),
        });
        assert_eq!(
            *seen.lock().unwrap_or_else(PoisonError::into_inner),
            vec![
                "uncaptured GPU validation error: validation".to_owned(),
                "GPU device lost (Destroyed): device dropped".to_owned(),
            ]
        );
    }

    #[test]
    fn clearing_the_handler_stops_delivery() {
        let hook = DiagnosticHook::with_strict(false);
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        hook.set(Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::Release);
        })));
        hook.emit(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Internal,
            message: "first".to_owned(),
        });
        hook.set(None);
        hook.emit(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Internal,
            message: "second".to_owned(),
        });
        assert_eq!(count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn every_wgpu_error_keeps_its_class() {
        // The class is what a failure report and the retry policy read; text says nothing.
        let source = || -> wgpu::ErrorSource { Box::new(std::io::Error::other("validation")) };
        for (error, kind, code) in [
            (
                wgpu::Error::OutOfMemory { source: source() },
                GpuErrorKind::OutOfMemory,
                "OUT_OF_MEMORY",
            ),
            (
                wgpu::Error::Validation {
                    source: source(),
                    description: "out of memory, says the text".to_owned(),
                },
                GpuErrorKind::Validation,
                "VALIDATION_FAILED",
            ),
            (
                wgpu::Error::Internal {
                    source: source(),
                    description: "out of memory".to_owned(),
                },
                GpuErrorKind::Internal,
                "UNKNOWN",
            ),
        ] {
            assert_eq!(GpuErrorKind::of(&error), kind);
            let diagnostic = GpuDiagnostic::uncaptured(&error);
            assert_eq!(diagnostic.failure_code(), code, "{diagnostic}");
            assert_eq!(diagnostic.event(), "uncaptured-error");
        }
    }

    #[test]
    fn an_out_of_memory_error_names_the_call_and_resource_that_ran_out() {
        // wgpu writes only "Out of Memory" for this class; the call, label and cause are in its
        // sources, shaped as wgpu-core's ContextError.
        #[derive(Debug)]
        struct Context(std::io::Error);
        impl fmt::Display for Context {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("In Device::create_texture, label = 'alpha2 intermediate'")
            }
        }
        impl std::error::Error for Context {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let error = wgpu::Error::OutOfMemory {
            source: Box::new(Context(std::io::Error::other("Not enough memory left."))),
        };
        assert_eq!(
            GpuDiagnostic::uncaptured(&error),
            GpuDiagnostic::UncapturedError {
                kind: GpuErrorKind::OutOfMemory,
                message: "Out of Memory: In Device::create_texture, label = 'alpha2 intermediate': Not enough memory left.".to_owned(),
            }
        );
        // A browser's report carries only wgpu's placeholder, which names nothing.
        let browser = wgpu::Error::OutOfMemory {
            source: Box::<dyn std::error::Error + Send + Sync>::from("<WebGPU Error>"),
        };
        assert_eq!(
            GpuDiagnostic::uncaptured(&browser).to_string(),
            "uncaptured GPU out-of-memory error: Out of Memory"
        );
        // A validation description already spells its sources out.
        let validation = wgpu::Error::Validation {
            source: Box::new(std::io::Error::other("inner")),
            description: "Validation Error\n\nCaused by:\n  inner".to_owned(),
        };
        let GpuDiagnostic::UncapturedError { message, .. } = GpuDiagnostic::uncaptured(&validation)
        else {
            panic!("an uncaptured error");
        };
        assert_eq!(message, "Validation Error\n\nCaused by:\n  inner");
    }

    fn oom(message: &str) -> GpuDiagnostic {
        GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::OutOfMemory,
            message: message.to_owned(),
        }
    }

    #[test]
    fn the_log_keeps_the_newest_events_in_fixed_space() {
        let log = GpuDiagnosticLog::new(2);
        log.push(oom("a"));
        log.push(oom("b"));
        log.push(oom("c"));
        let snapshot = log.snapshot();
        assert_eq!(snapshot.entries, vec![oom("b"), oom("c")]);
        assert_eq!(snapshot.dropped, 1);
    }

    fn kept_message(log: &GpuDiagnosticLog) -> String {
        let GpuDiagnostic::UncapturedError { message, .. } = &log.snapshot().entries[0] else {
            panic!("an uncaptured error");
        };
        message.clone()
    }

    #[test]
    fn the_log_truncates_long_messages_on_a_char_boundary() {
        // The cut falls inside a 3-byte character (naga's box drawing), so the walk back runs.
        for (input, kept) in [
            (
                format!("a{}", "é".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES)),
                format!("a{}", "é".repeat(511)),
            ),
            ("─".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES), "─".repeat(341)),
        ] {
            assert!(!input.is_char_boundary(MAX_DIAGNOSTIC_MESSAGE_BYTES));
            let log = GpuDiagnosticLog::new(1);
            log.push(oom(&input));
            // The longest prefix ending on a boundary at or before the limit, then the ellipsis.
            assert_eq!(kept_message(&log), format!("{kept}{ELLIPSIS}"));
        }
        let log = GpuDiagnosticLog::new(1);
        let exact = "x".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES);
        log.push(oom(&exact));
        assert_eq!(kept_message(&log), exact, "a message at the limit is whole");
    }

    #[test]
    fn the_log_keeps_no_more_memory_than_the_messages_it_keeps() {
        // A cut message must not keep the buffer it was cut from; read through the kept log, not a
        // snapshot's compact clone.
        let log = GpuDiagnosticLog::new(2);
        let mut quoted = String::with_capacity(64 * 1024);
        quoted.push_str(&"v".repeat(64 * 1024));
        log.push(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Validation,
            message: quoted,
        });
        let mut short = String::with_capacity(16 * 1024);
        short.push_str("short");
        log.push(GpuDiagnostic::DeviceLost {
            reason: short.clone(),
            message: short,
        });
        let state = log.state.lock().unwrap_or_else(PoisonError::into_inner);
        let held: Vec<usize> = state
            .entries
            .iter()
            .flat_map(|entry| match entry {
                GpuDiagnostic::UncapturedError { message, .. } => vec![message.capacity()],
                GpuDiagnostic::DeviceLost { reason, message } => {
                    vec![reason.capacity(), message.capacity()]
                }
            })
            .collect();
        assert_eq!(
            held,
            [MAX_DIAGNOSTIC_MESSAGE_BYTES + ELLIPSIS.len(), 5, 5],
            "each kept text owns its length and no more"
        );
    }

    #[test]
    fn only_typed_evidence_names_a_failure_class() {
        let log = GpuDiagnosticLog::default();
        log.push(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Internal,
            message: "out of memory, says the text".to_owned(),
        });
        assert_eq!(log.snapshot().failure_code(), None);
        log.push(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Validation,
            message: "bad layout".to_owned(),
        });
        assert_eq!(log.snapshot().failure_code(), Some("VALIDATION_FAILED"));
        log.push(oom("buffer"));
        assert_eq!(log.snapshot().failure_code(), Some("OUT_OF_MEMORY"));
        log.push(GpuDiagnostic::DeviceLost {
            reason: "Unknown".to_owned(),
            message: "driver reset".to_owned(),
        });
        assert_eq!(log.snapshot().failure_code(), Some("DEVICE_LOST"));
    }

    #[test]
    fn an_out_of_memory_report_survives_eviction_until_cleared() {
        // Later uses of the invalid resource push the report out with validation errors, yet the
        // out-of-memory report is still the cause.
        let log = GpuDiagnosticLog::default();
        log.push(oom("Device::create_texture"));
        for _ in 0..GpuDiagnosticLog::DEFAULT_CAPACITY {
            log.push(GpuDiagnostic::UncapturedError {
                kind: GpuErrorKind::Validation,
                message: "texture is invalid".to_owned(),
            });
        }
        let snapshot = log.snapshot();
        assert_eq!(snapshot.dropped, 1);
        assert!(!snapshot.entries.contains(&oom("Device::create_texture")));
        assert!(snapshot.out_of_memory);
        assert_eq!(snapshot.failure_code(), Some("OUT_OF_MEMORY"));
        // What follows a clear is the next span's evidence; memory may be back.
        log.clear();
        log.push(GpuDiagnostic::UncapturedError {
            kind: GpuErrorKind::Validation,
            message: "bad layout".to_owned(),
        });
        let snapshot = log.snapshot();
        assert!(!snapshot.out_of_memory);
        assert_eq!(snapshot.failure_code(), Some("VALIDATION_FAILED"));
    }

    #[test]
    fn a_device_loss_survives_clearing_and_eviction() {
        let log = GpuDiagnosticLog::new(1);
        log.push(GpuDiagnostic::DeviceLost {
            reason: "Unknown".to_owned(),
            message: String::new(),
        });
        log.push(oom("later"));
        log.clear();
        let snapshot = log.snapshot();
        assert_eq!(snapshot.entries, [] as [GpuDiagnostic; 0]);
        assert!(snapshot.device_lost);
        assert_eq!(snapshot.failure_code(), Some("DEVICE_LOST"));
    }
}
