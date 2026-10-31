//! A collector for the validation messages `wgpu-hal` sends only to the `log` crate, so a test
//! run can report them or fail on them.
//!
//! `wgpu-hal` writes Vulkan debug-utils messages, GL debug output and the D3D12 debug layer's
//! messages through `log`, each call inside `catch_unwind`: a logger that panics is ignored, and
//! with no logger installed every message is dropped. [`install`] makes this module the process's
//! logger. It keeps the records of `wgpu_hal` targets at `Error` and `Warn` and ignores the rest;
//! each kept record is also written to standard error, so a test's output shows it.
//!
//! The D3D12 debug layer writes its messages to a debugger's output, which `wgpu-hal` reads only
//! when one is attached, and stores them in the device's `ID3D12InfoQueue` either way. Once the
//! collector is installed, every DX12 device a [`crate::GpuContext`] adopts is watched: its
//! stored errors, corruptions and warnings are read into the collector at each checkpoint, and
//! the records of the debugger route are dropped so nothing is counted twice. A watched device
//! is kept until the process exits.
//!
//! A test reads what arrived during its turn with a [`Checkpoint`], or a [`TestTurn`] that wraps
//! the guard serializing GPU tests and reports or fails when the turn ends, as
//! `ALPHA2_VALIDATION_LOG` says (see [`ValidationMode`]). Errors a test causes on purpose go
//! through [`expect_errors`], which hands them back instead of reporting them.

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
    /// The name of the thread that logged it, when it has one: a test's name for a message
    /// logged on the test's thread, a driver's thread otherwise.
    pub thread: Option<String>,
}

/// What a [`TestTurn`] does with the records of its turn, from `ALPHA2_VALIDATION_LOG`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValidationMode {
    /// `off`: nothing is installed or collected.
    Off,
    /// `report` (the default): records are written to standard error, and appended to
    /// `ALPHA2_VALIDATION_REPORT` when it names a file.
    Report,
    /// `fail`: as `report`, then an `Error` record fails the turn.
    Fail,
}

/// The mode `ALPHA2_VALIDATION_LOG` asks for, read once per process: `off`, `report` or `fail`,
/// `report` when unset or empty.
///
/// # Panics
/// On any other value, so a misspelt mode never runs a lane blind.
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

/// Makes the collector the process's logger and lowers the `log` level to `Warn`. Calling it
/// again does nothing. Does nothing on wasm, where `wgpu-hal` does not run.
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

/// Installs the collector unless [`mode`] is [`ValidationMode::Off`]: for a test suite's device
/// initializer, before its first device, so the device's D3D12 queue is watched.
///
/// # Panics
/// When another logger was installed first ([`InstallError::AlreadySet`]): a run that asked for
/// validation must not go on blind.
pub fn install_unless_off() {
    if mode() != ValidationMode::Off {
        install().unwrap_or_else(|error| panic!("{error}"));
    }
}

/// Whether [`install`] made the collector the process's logger.
fn installed() -> bool {
    SINK.installed.load(Ordering::Acquire) == 1
}

/// The records of one stretch of a test's work: those logged between [`Checkpoint::begin`] and
/// [`Checkpoint::finish`], and those logged before the begin that no checkpoint took, labelled
/// with the name of the checkpoint that found them.
#[derive(Debug)]
pub struct Checkpoint {
    name: String,
    earlier: Vec<ValidationRecord>,
}

impl Checkpoint {
    /// Begins a checkpoint called `name` (a test's name, say). Installs the collector when the
    /// mode is not [`ValidationMode::Off`].
    ///
    /// # Panics
    /// When another logger was installed first ([`InstallError::AlreadySet`]): a run that asked
    /// for validation must not go on blind.
    #[must_use]
    pub fn begin(name: &str) -> Self {
        install_unless_off();
        let earlier = take_records()
            .into_iter()
            .map(|mut record| {
                // Logged outside any checkpoint: a driver's late callback from the test before,
                // or a test that does not take the GPU turn.
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

    /// Ends the checkpoint and returns its records: those found at the begin, then those logged
    /// since, oldest first.
    #[must_use]
    pub fn finish(mut self) -> Vec<ValidationRecord> {
        let mut records = std::mem::take(&mut self.earlier);
        records.extend(take_records());
        records
    }
}

/// Runs `test`, which causes validation errors on purpose, and returns its result with the
/// records logged while it ran, which no checkpoint then reports. The caller holds the GPU turn,
/// so no other test's records arrive meanwhile.
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
    /// A file each record is appended to as one JSON line `{"test", "level", "target",
    /// "message"}`, in both reporting modes.
    pub report: Option<PathBuf>,
}

impl TurnSettings {
    /// [`mode`], and the report file `ALPHA2_VALIDATION_REPORT` names when it is set and not
    /// empty.
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

/// A test's GPU turn: `guard` (what serializes the GPU tests of a suite) together with a
/// [`Checkpoint`] named after the test, which reports or fails when the turn ends, before the
/// guard is released. Holding one is holding the guard.
pub struct TestTurn<G> {
    checkpoint: Option<Checkpoint>,
    settings: TurnSettings,
    _guard: G,
}

impl<G> TestTurn<G> {
    /// A turn holding `guard`, with the settings the environment asks for and the name libtest
    /// gives the current thread (the test's).
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

/// Appends one JSON line per record to `path`; a file that cannot be written is said on
/// standard error rather than failing the test.
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

/// The process's collector.
struct Sink {
    /// Each kept record with its sequence number, oldest first.
    records: Mutex<Vec<(u64, ValidationRecord)>>,
    /// The next record's sequence number.
    next: AtomicU64,
    /// How many [`expect_errors`] calls are running: their records are marked as expected on
    /// standard error.
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

    /// Keeps `record` and writes it to standard error. A line `wgpu-hal` logs after a Vulkan
    /// message to name its queues, command buffers or objects (indented with a tab) joins the
    /// record of that message.
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
        // The D3D12 debug layer's messages are read from the watched devices' queues instead.
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

/// Writes `line` to standard error as `eprintln!` does, so libtest shows it with the test that
/// wrote it, without failing when standard error is closed: `eprintln!` panics then, and a test
/// whose parent closed the stream (to see that nothing else panics on it) must not fail on a line
/// nobody reads.
fn note(line: std::fmt::Arguments<'_>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| eprintln!("{line}")));
}

/// The target of the records `wgpu-hal` makes from the D3D12 debug layer's debugger output.
const DEBUGGER_ROUTE: &str = "wgpu_hal::auxil::dxgi::exception";

/// Takes every record the collector holds, reading the watched devices' queues first.
fn take_records() -> Vec<ValidationRecord> {
    drain_watched();
    SINK.records().drain(..).map(|(_, record)| record).collect()
}

/// When the collector is installed (a test run that collects validation messages): writes
/// `gpu tests on <summary>` to standard error, so a run's log names every device it opened, and
/// starts watching `device`'s D3D12 debug-layer queue when it is a DX12 device with the debug
/// layer on. Called for every device a context adopts, with the summary of its capabilities.
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

/// Whether a D3D12 queue is watched.
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

    /// A watched queue and how many of its messages were read: the count includes the messages
    /// the queue discarded at its limit, so it only grows.
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
        // D3D12 makes one device per adapter: a second context on it shares the queue.
        if watched.iter().any(|known| known.queue == queue) {
            return;
        }
        // Messages stored before the device was watched belong to no test.
        let read = total(&queue);
        watched.push(Watched { queue, read });
    }

    /// Every message the queue has taken: those stored and those its limit discarded.
    fn total(queue: &ID3D12InfoQueue) -> u64 {
        // SAFETY: two counter reads on a live queue.
        unsafe { queue.GetNumStoredMessages() + queue.GetNumMessagesDiscardedByMessageCountLimit() }
    }

    pub(super) fn drain() {
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        for known in watched.iter_mut() {
            let total = total(&known.queue);
            // A test that cleared the queue itself: the count starts again.
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

    /// The stored message at `index` as a record, when it is an error, a corruption or a
    /// warning.
    fn message(queue: &ID3D12InfoQueue, index: u64) -> Option<ValidationRecord> {
        let mut length = 0;
        // SAFETY: a live queue; asked for the length only.
        unsafe { queue.GetMessage(index, None, &raw mut length) }.ok()?;
        if length == 0 {
            return None;
        }
        // Aligned for the structure, which the description follows in the same buffer.
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
