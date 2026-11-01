//! Collects the validation messages `wgpu-hal` sends only to the `log` crate, so tests can report
//! or fail on them.
//!
//! `wgpu-hal` logs Vulkan, GL and D3D12 debug messages inside `catch_unwind`, so a panicking
//! logger is ignored and without a logger they are dropped. [`install`] makes this module the
//! logger; it keeps `wgpu_hal` records at `Error` and `Warn` and echoes them to standard error.
//!
//! The D3D12 debug layer reaches `wgpu-hal` only with a debugger attached, but always stores
//! messages in the device's `ID3D12InfoQueue`. Once installed, every adopted DX12 device's queue
//! is read at each checkpoint and the debugger-route records are dropped to avoid duplicates.
//! Watched devices are kept until process exit.
//!
//! Tests read their records with a [`Checkpoint`] or a [`TestTurn`], which reports or fails per
//! `ALPHA2_VALIDATION_LOG` ([`ValidationMode`]). Intentional errors go through [`expect_errors`].

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// One message a `wgpu_hal` target logged at `Error` or `Warn`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationRecord {
    /// The level it was logged at.
    pub level: log::Level,
    /// The module that logged it, such as `wgpu_hal::vulkan::instance`.
    pub target: String,
    /// The message.
    pub message: String,
    /// The logging thread's name (the test name on a test thread).
    pub thread: Option<String>,
}

/// What a [`TestTurn`] does with the records of its turn, from `ALPHA2_VALIDATION_LOG`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValidationMode {
    /// `off`: nothing is installed or collected.
    Off,
    /// `report` (the default): records go to standard error and to `ALPHA2_VALIDATION_REPORT`
    /// when set.
    Report,
    /// `fail`: as `report`, then an `Error` record fails the turn.
    Fail,
}

/// The `ALPHA2_VALIDATION_LOG` mode, read once: `off`, `report` or `fail`, default `report`.
///
/// # Panics
/// On any other value, so a misspelt mode never runs blind.
#[must_use]
pub fn mode() -> ValidationMode {
    static MODE: OnceLock<ValidationMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("ALPHA2_VALIDATION_LOG")
            .unwrap_or_default()
            .as_str()
        {
            "" | "report" => ValidationMode::Report,
            "off" => ValidationMode::Off,
            "fail" => ValidationMode::Fail,
            other => panic!("ALPHA2_VALIDATION_LOG must be off, report or fail, not {other:?}"),
        }
    })
}

/// Why [`install`] could not make the collector the process's logger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallError {
    /// Another logger was installed first, so `wgpu-hal`'s messages go there.
    AlreadySet,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadySet => formatter.write_str(
                "another logger was installed before the validation collector, so validation \
                 messages would not be collected",
            ),
        }
    }
}

impl std::error::Error for InstallError {}

/// Makes the collector the process logger at `Warn` level. Idempotent; a no-op on wasm.
///
/// # Errors
/// [`InstallError::AlreadySet`] when another logger was installed first.
pub fn install() -> Result<(), InstallError> {
    static INSTALLED: OnceLock<Result<(), InstallError>> = OnceLock::new();
    if cfg!(target_arch = "wasm32") {
        return Ok(());
    }
    *INSTALLED.get_or_init(|| {
        log::set_logger(&SINK).map_err(|_| InstallError::AlreadySet)?;
        log::set_max_level(log::LevelFilter::Warn);
        SINK.installed.store(1, Ordering::Release);
        Ok(())
    })
}

/// Installs the collector unless [`mode`] is [`ValidationMode::Off`]. Call before the first
/// device so its D3D12 queue is watched.
///
/// # Panics
/// When another logger was installed first ([`InstallError::AlreadySet`]).
pub fn install_unless_off() {
    if mode() != ValidationMode::Off {
        install().unwrap_or_else(|error| panic!("{error}"));
    }
}

fn installed() -> bool {
    SINK.installed.load(Ordering::Acquire) == 1
}

/// Records logged between [`Checkpoint::begin`] and [`Checkpoint::finish`], plus earlier
/// unclaimed records labelled with this checkpoint's name.
#[derive(Debug)]
pub struct Checkpoint {
    name: String,
    earlier: Vec<ValidationRecord>,
}

impl Checkpoint {
    /// Begins a checkpoint called `name`, installing the collector unless the mode is
    /// [`ValidationMode::Off`].
    ///
    /// # Panics
    /// When another logger was installed first ([`InstallError::AlreadySet`]).
    #[must_use]
    pub fn begin(name: &str) -> Self {
        install_unless_off();
        let earlier = take_records()
            .into_iter()
            .map(|mut record| {
                // Logged outside any checkpoint, such as a late driver callback.
                record.message = format!("before {name}: {}", record.message);
                record
            })
            .collect();
        Self {
            name: name.to_owned(),
            earlier,
        }
    }

    /// The name the checkpoint was begun with.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ends the checkpoint and returns its records, oldest first.
    #[must_use]
    pub fn finish(mut self) -> Vec<ValidationRecord> {
        let mut records = std::mem::take(&mut self.earlier);
        records.extend(take_records());
        records
    }
}

/// Runs `test`, which causes validation errors on purpose, and returns the records it logged
/// instead of reporting them. The caller must hold the GPU turn.
pub fn expect_errors<R>(test: impl FnOnce() -> R) -> (R, Vec<ValidationRecord>) {
    drain_watched();
    let first = SINK.next.load(Ordering::Acquire);
    SINK.expecting.fetch_add(1, Ordering::AcqRel);
    let result = test();
    drain_watched();
    SINK.expecting.fetch_sub(1, Ordering::AcqRel);
    let mut records = SINK.records();
    let (expected, other): (Vec<_>, Vec<_>) = records
        .drain(..)
        .partition(|(sequence, _)| *sequence >= first);
    *records = other;
    (
        result,
        expected.into_iter().map(|(_, record)| record).collect(),
    )
}

/// What a [`TestTurn`] does when its turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnSettings {
    /// Whether records are reported, fail the turn, or are not collected.
    pub mode: ValidationMode,
    /// A file each record is appended to as a JSON line `{"test", "level", "target", "message"}`.
    pub report: Option<PathBuf>,
}

impl TurnSettings {
    /// [`mode`] and a non-empty `ALPHA2_VALIDATION_REPORT`.
    #[must_use]
    pub fn from_environment() -> Self {
        Self {
            mode: mode(),
            report: std::env::var_os("ALPHA2_VALIDATION_REPORT")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// A test's GPU turn: the suite's serializing `guard` plus a [`Checkpoint`] named after the test,
/// which reports or fails before the guard is released.
pub struct TestTurn<G> {
    checkpoint: Option<Checkpoint>,
    settings: TurnSettings,
    _guard: G,
}

impl<G> TestTurn<G> {
    /// A turn holding `guard`, with environment settings and the current thread's test name.
    ///
    /// # Panics
    /// As [`Checkpoint::begin`], and on a mode `ALPHA2_VALIDATION_LOG` does not name.
    #[must_use]
    pub fn new(guard: G) -> Self {
        Self::with_settings(guard, TurnSettings::from_environment())
    }

    /// A turn holding `guard` with `settings`, whatever the environment says.
    ///
    /// # Panics
    /// As [`Checkpoint::begin`].
    #[must_use]
    pub fn with_settings(guard: G, settings: TurnSettings) -> Self {
        let checkpoint = (settings.mode != ValidationMode::Off).then(|| {
            install().unwrap_or_else(|error| panic!("{error}"));
            let thread = std::thread::current();
            Checkpoint::begin(thread.name().unwrap_or("an unnamed thread"))
        });
        Self {
            checkpoint,
            settings,
            _guard: guard,
        }
    }
}

impl<G> Drop for TestTurn<G> {
    fn drop(&mut self) {
        let Some(checkpoint) = self.checkpoint.take() else {
            return;
        };
        let test = checkpoint.name().to_owned();
        let records = checkpoint.finish();
        if records.is_empty() {
            return;
        }
        if let Some(path) = &self.settings.report {
            append_report(path, &test, &records);
        }
        let errors = records
            .iter()
            .filter(|record| record.level == log::Level::Error)
            .count();
        if self.settings.mode == ValidationMode::Fail && errors > 0 && !std::thread::panicking() {
            let listed = records
                .iter()
                .map(|record| format!("  {} {}: {}", record.level, record.target, record.message))
                .collect::<Vec<_>>()
                .join("\n");
            panic!("{test}: {errors} validation error(s) during the test:\n{listed}");
        }
    }
}

/// Appends one JSON line per record to `path`; write failures are reported, not fatal.
fn append_report(path: &std::path::Path, test: &str, records: &[ValidationRecord]) {
    let mut lines = String::new();
    for record in records {
        let _ = writeln!(
            lines,
            "{{\"test\":{},\"level\":{},\"target\":{},\"message\":{}}}",
            json_string(test),
            json_string(record.level.as_str()),
            json_string(&record.target),
            json_string(&record.message),
        );
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(lines.as_bytes()));
    if let Err(error) = written {
        note(format_args!(
            "validation: could not append to {}: {error}",
            path.display()
        ));
    }
}

/// `text` as a JSON string literal.
fn json_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(quoted, "\\u{:04x}", u32::from(control));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

struct Sink {
    records: Mutex<Vec<(u64, ValidationRecord)>>,
    next: AtomicU64,
    /// Running [`expect_errors`] calls, whose records are marked as expected.
    expecting: AtomicUsize,
    /// 1 once [`install`] made this the logger.
    installed: AtomicUsize,
}

static SINK: Sink = Sink {
    records: Mutex::new(Vec::new()),
    next: AtomicU64::new(0),
    expecting: AtomicUsize::new(0),
    installed: AtomicUsize::new(0),
};

impl Sink {
    fn records(&self) -> MutexGuard<'_, Vec<(u64, ValidationRecord)>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps `record` and echoes it. Tab-indented continuation lines that `wgpu-hal` logs after a
    /// Vulkan message join that message's record.
    fn keep(&self, record: ValidationRecord) {
        if record.message.starts_with('\t') {
            let mut records = self.records();
            if let Some((_, last)) = records.last_mut()
                && last.target == record.target
                && last.thread == record.thread
            {
                last.message.push('\n');
                last.message.push_str(&record.message);
                return;
            }
        }
        let expected = if self.expecting.load(Ordering::Acquire) > 0 {
            " (expected)"
        } else {
            ""
        };
        note(format_args!(
            "validation{expected}: {} {}: {}",
            record.level, record.target, record.message
        ));
        let mut records = self.records();
        let sequence = self.next.fetch_add(1, Ordering::AcqRel);
        records.push((sequence, record));
    }
}

impl log::Log for Sink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn && metadata.target().starts_with("wgpu_hal")
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // D3D12 messages are read from the watched queues instead.
        if record.target().starts_with(DEBUGGER_ROUTE) && watching() {
            return;
        }
        self.keep(ValidationRecord {
            level: record.level(),
            target: record.target().to_owned(),
            message: record.args().to_string(),
            thread: std::thread::current().name().map(str::to_owned),
        });
    }

    fn flush(&self) {}
}

/// Like `eprintln!` (so libtest captures it) but does not panic when standard error is closed.
fn note(line: std::fmt::Arguments<'_>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| eprintln!("{line}")));
}

/// Target of `wgpu-hal` records from the D3D12 debugger output.
const DEBUGGER_ROUTE: &str = "wgpu_hal::auxil::dxgi::exception";

/// Takes every record the collector holds, reading the watched devices' queues first.
fn take_records() -> Vec<ValidationRecord> {
    drain_watched();
    SINK.records().drain(..).map(|(_, record)| record).collect()
}

/// When the collector is installed, logs `gpu tests on <summary>` and starts watching the
/// D3D12 debug queue of a DX12 `device`. Called for every adopted device.
pub(crate) fn watch_device(device: &wgpu::Device, summary: &str) {
    if !installed() {
        return;
    }
    note(format_args!("gpu tests on {summary}"));
    #[cfg(windows)]
    d3d12::watch(device);
    #[cfg(not(windows))]
    let _ = device;
}

fn watching() -> bool {
    #[cfg(windows)]
    {
        d3d12::watching()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Reads what the watched D3D12 queues stored since the last read into the collector.
fn drain_watched() {
    #[cfg(windows)]
    d3d12::drain();
}

/// The D3D12 debug layer's message queues.
#[cfg(windows)]
#[allow(unsafe_code)]
mod d3d12 {
    use std::sync::{Mutex, PoisonError};

    use windows::Win32::Graphics::Direct3D12::{
        D3D12_MESSAGE, D3D12_MESSAGE_SEVERITY_CORRUPTION, D3D12_MESSAGE_SEVERITY_ERROR,
        D3D12_MESSAGE_SEVERITY_WARNING, ID3D12InfoQueue,
    };
    use windows::core::Interface as _;

    use super::{SINK, ValidationRecord};

    /// The target of the records read from a queue.
    const TARGET: &str = "wgpu_hal::dx12::info_queue";

    /// A watched queue and its read count, which includes discarded messages so it only grows.
    struct Watched {
        queue: ID3D12InfoQueue,
        read: u64,
    }

    static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());

    pub(super) fn watching() -> bool {
        !WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    pub(super) fn watch(device: &wgpu::Device) {
        // SAFETY: the raw device is only queried for another interface, which takes a reference
        // of its own; nothing is destroyed through it.
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }) else {
            return;
        };
        // Without the debug layer the device has no info queue.
        let Ok(queue) = hal.raw_device().cast::<ID3D12InfoQueue>() else {
            return;
        };
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        // D3D12 has one device per adapter, so contexts may share a queue.
        if watched.iter().any(|known| known.queue == queue) {
            return;
        }
        // Messages stored before the device was watched belong to no test.
        let read = total(&queue);
        watched.push(Watched { queue, read });
    }

    /// Stored plus discarded message count.
    fn total(queue: &ID3D12InfoQueue) -> u64 {
        // SAFETY: two counter reads on a live queue.
        unsafe { queue.GetNumStoredMessages() + queue.GetNumMessagesDiscardedByMessageCountLimit() }
    }

    pub(super) fn drain() {
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        for known in watched.iter_mut() {
            let total = total(&known.queue);
            // The queue was cleared, so the count restarts.
            if total < known.read {
                known.read = 0;
            }
            // SAFETY: a counter read on a live queue.
            let discarded = unsafe { known.queue.GetNumMessagesDiscardedByMessageCountLimit() };
            for absolute in known.read.max(discarded)..total {
                if let Some(record) = message(&known.queue, absolute - discarded) {
                    SINK.keep(record);
                }
            }
            known.read = total;
        }
    }

    /// The stored message at `index` if it is an error, corruption or warning.
    fn message(queue: &ID3D12InfoQueue, index: u64) -> Option<ValidationRecord> {
        let mut length = 0;
        // SAFETY: a live queue; asked for the length only.
        unsafe { queue.GetMessage(index, None, &raw mut length) }.ok()?;
        if length == 0 {
            return None;
        }
        // Aligned for the structure; the description follows it in the same buffer.
        let mut storage = vec![0_u64; length.div_ceil(8)];
        let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
        // SAFETY: `storage` holds the `length` bytes the call writes.
        unsafe { queue.GetMessage(index, Some(message), &raw mut length) }.ok()?;
        // SAFETY: the call wrote a message, whose description points into `storage`.
        let message = unsafe { &*message };
        let level = match message.Severity {
            D3D12_MESSAGE_SEVERITY_CORRUPTION | D3D12_MESSAGE_SEVERITY_ERROR => log::Level::Error,
            D3D12_MESSAGE_SEVERITY_WARNING => log::Level::Warn,
            _ => return None,
        };
        // SAFETY: the description is `DescriptionByteLength` bytes inside `storage`.
        let text = unsafe {
            std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength)
        };
        Some(ValidationRecord {
            level,
            target: TARGET.to_owned(),
            message: String::from_utf8_lossy(text)
                .trim_end_matches('\0')
                .to_owned(),
            thread: None,
        })
    }
}
