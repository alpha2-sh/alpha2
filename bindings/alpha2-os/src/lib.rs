//! Safe wrappers for the operating-system calls the native host needs that the standard library
//! does not offer at the minimum Rust version: file locks, no-replace publishing, termination
//! handling, parent and child lifetime, clocks, memory, and the OS version.
//!
//! The rest of the workspace forbids `unsafe`; this crate and `alpha2-ffmpeg-bridge` hold the few
//! calls that need it.
//!
//! In iOS and Android apps the system owns the process, so the termination, parent-exit and
//! child-lifetime calls have no use there; files, clocks, memory and the version do.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

mod lifetime;
mod memory;
mod os_version;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

pub use lifetime::{
    KillOnCloseJob, claim_stdout, exit_without_handlers, stop_std_handle_inheritance, watch_parent,
};
pub use memory::{MemoryCapacity, memory_capacity};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use memory::{cgroup_v2_usage, memory_capacity_at};
pub use os_version::os_version;

/// Takes an exclusive advisory lock on `file` without waiting. Returns `Ok(false)` when another
/// open handle, in this or another process, holds it.
///
/// The lock belongs to this handle and is released when it closes or the process dies. Other
/// handles can still read and write the whole file, so `FFmpeg` opening the same path is not
/// affected. Where locks are mandatory, the lock covers only a byte no write can reach:
/// - Windows: `LockFileEx` on one byte at offset `i64::MAX`.
/// - Linux: an open-file-description lock (`F_OFD_SETLK`) on that byte. On CIFS mounts `flock`
///   becomes a mandatory whole-file lock that would fail `FFmpeg`'s writes, and a classic `fcntl`
///   lock is dropped when any handle of the file closes. Falls back to `flock` on `EINVAL`.
/// - macOS and other Unix: `flock`.
///
/// On Linux `file` must be open for writing.
///
/// # Errors
/// When the OS refuses the lock for a reason other than another holder, for example on a file
/// system without locks.
pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    platform::try_lock_exclusive(file)
}

/// Whether `path` still names the file `file` is open on. `false` when the path is gone, names a
/// recreated file, or names a removed file kept visible by an open handle (Windows and SMB keep
/// such names until the last handle closes).
///
/// # Errors
/// When the file's identity cannot be read.
pub fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    platform::same_file(file, path)
}

/// Whether `a` and `b` name one file (same device and inode, or volume serial and file index on
/// Windows). Hard links and symlinks count as the same file. `false` when either path is missing.
///
/// # Errors
/// When either path's identity cannot be read for a reason other than it being missing.
pub fn same_file_at(a: &Path, b: &Path) -> io::Result<bool> {
    platform::same_file_at(a, b)
}

/// How [`publish_no_replace`] or [`publish_replace`] moved a file into place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishMethod {
    /// One rename that refuses an existing target: `renameat2(RENAME_NOREPLACE)`
    /// on Linux, `renamex_np(RENAME_EXCL)` on macOS, `MoveFileExW` without
    /// `MOVEFILE_REPLACE_EXISTING` on Windows.
    RenameNoReplace,
    /// The file system refused the no-replace flag, so an empty `O_EXCL` placeholder was created
    /// at the target and renamed over.
    PlaceholderRename,
    /// A rename that replaces whatever is at the target.
    RenameReplace,
}

impl PublishMethod {
    /// The name used in reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RenameNoReplace => "rename-no-replace",
            Self::PlaceholderRename => "placeholder-rename",
            Self::RenameReplace => "rename-replace",
        }
    }
}

/// A finished file moved into place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Published {
    /// How it was moved.
    pub method: PublishMethod,
    /// Retries after errors accepted by [`is_retryable_publish_error`].
    pub retries: u32,
}

/// How long a publish retries a step refused by another program's open handle.
#[derive(Clone, Copy)]
pub struct RetryPolicy<'a> {
    /// Time from the first try after which no retry starts. Zero tries once.
    pub budget: Duration,
    /// Called before each wait; `false` stops and returns the last error.
    pub on_wait: &'a dyn Fn() -> bool,
}

fn keep_waiting() -> bool {
    true
}

impl RetryPolicy<'static> {
    /// One try, no retries.
    pub const NONE: Self = Self {
        budget: Duration::ZERO,
        on_wait: &keep_waiting,
    };
}

impl std::fmt::Debug for RetryPolicy<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetryPolicy")
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

// Each later wait doubles, up to `RETRY_MAX_DELAY`.
const RETRY_FIRST_DELAY: Duration = Duration::from_millis(50);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// Runs `step` until it succeeds, fails with a non-retryable error, or `retry` stops it. Waits
/// 50 ms before the first retry, doubling up to 1 s. Returns the value and the retry count.
///
/// # Errors
/// The step's last error.
pub fn with_publish_retries<T>(
    retry: &RetryPolicy<'_>,
    step: impl FnMut() -> io::Result<T>,
) -> io::Result<(T, u32)> {
    retrying(retry, is_retryable_publish_error, step)
}

fn retrying<T>(
    retry: &RetryPolicy<'_>,
    retryable: impl Fn(&io::Error) -> bool,
    mut step: impl FnMut() -> io::Result<T>,
) -> io::Result<(T, u32)> {
    let started = Instant::now();
    let mut delay = RETRY_FIRST_DELAY;
    let mut retries = 0_u32;
    loop {
        match step() {
            Ok(value) => return Ok((value, retries)),
            Err(error) => {
                let elapsed = started.elapsed();
                if !retryable(&error) || elapsed >= retry.budget || !(retry.on_wait)() {
                    return Err(error);
                }
                std::thread::sleep(delay.min(retry.budget.saturating_sub(elapsed)));
                retries = retries.saturating_add(1);
                delay = (delay * 2).min(RETRY_MAX_DELAY);
            }
        }
    }
}

/// Whether a publish step failed because another program (antivirus, indexer, media player)
/// briefly holds the file open: `ERROR_ACCESS_DENIED`, `ERROR_SHARING_VIOLATION` or
/// `ERROR_LOCK_VIOLATION` on Windows. Always `false` on Unix, where open handles do not block a
/// rename.
///
/// A real permission error also reports `ERROR_ACCESS_DENIED`; retrying it costs at most the
/// budget.
#[must_use]
pub fn is_retryable_publish_error(error: &io::Error) -> bool {
    platform::is_retryable_publish_error(error)
}

/// Moves the finished file `from` to `to`, never replacing an existing file at `to`.
///
/// Uses the platform's no-replace rename, with verbatim (`\\?\`) paths on Windows to allow
/// paths over `MAX_PATH`. File systems that refuse the flag (exFAT on macOS, FUSE servers such as
/// rclone) get [`PublishMethod::PlaceholderRename`] instead. That path has two races the one-step
/// rename lacks: a process that replaces the empty placeholder before the rename loses its file,
/// and a crash between the two steps leaves an empty file at `to`.
///
/// Retryable errors are retried as `retry` allows.
///
/// # Errors
/// [`io::ErrorKind::AlreadyExists`] when `to` exists, else what the operating
/// system reports for the last try.
pub fn publish_no_replace(
    from: &Path,
    to: &Path,
    retry: &RetryPolicy<'_>,
) -> io::Result<Published> {
    publish_no_replace_with(from, to, retry, &platform::rename_exclusive)
}

/// [`publish_no_replace`] with an injectable rename, so tests can refuse the flag.
fn publish_no_replace_with(
    from: &Path,
    to: &Path,
    retry: &RetryPolicy<'_>,
    rename_exclusive: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<Published> {
    let (method, retries) = with_publish_retries(retry, || match rename_exclusive(from, to) {
        Ok(()) => Ok(PublishMethod::RenameNoReplace),
        Err(error) if platform::flag_refused(&error) => {
            publish_through_placeholder(from, to).map(|()| PublishMethod::PlaceholderRename)
        }
        Err(error) => Err(error),
    })?;
    Ok(Published { method, retries })
}

/// Creates an empty `O_EXCL` placeholder at `to`, then renames `from` over it. A failed rename
/// removes the placeholder if `to` still names it.
///
/// The placeholder must close before the rename: a FUSE server that writes back on close (rclone
/// without a cache) would otherwise write the empty file over the published one.
fn publish_through_placeholder(from: &Path, to: &Path) -> io::Result<()> {
    let placeholder = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let identity = platform::file_identity(&placeholder);
    drop(placeholder);
    let renamed = std::fs::rename(from, to);
    if renamed.is_err()
        && let Ok(identity) = identity
        && platform::path_identity(to).ok().flatten() == Some(identity)
    {
        let _ = std::fs::remove_file(to);
    }
    renamed
}

/// Moves the finished file `from` to `to`, replacing any file there. Retryable errors are
/// retried as `retry` allows.
///
/// # Errors
/// What the operating system reports for the last try.
pub fn publish_replace(from: &Path, to: &Path, retry: &RetryPolicy<'_>) -> io::Result<Published> {
    let ((), retries) = with_publish_retries(retry, || std::fs::rename(from, to))?;
    Ok(Published {
        method: PublishMethod::RenameReplace,
        retries,
    })
}

/// Makes renames into `dir` survive a power loss by `fsync`ing it on Unix. File systems that
/// cannot sync a directory are not an error. No-op on Windows, where the rename writes through.
///
/// # Errors
/// When the directory cannot be opened or the sync fails.
pub fn sync_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        platform::sync_directory(dir)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// What a [`NameLimit`] counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameUnit {
    /// Bytes of the name's UTF-8 encoding (Unix).
    Bytes,
    /// UTF-16 code units (Windows).
    Utf16,
}

/// The longest file name a directory takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NameLimit {
    /// Maximum name length in `unit`.
    pub max: usize,
    /// What `max` counts.
    pub unit: NameUnit,
}

impl NameLimit {
    /// The length of `name` in this limit's unit.
    #[must_use]
    pub fn length(self, name: &str) -> usize {
        match self.unit {
            NameUnit::Bytes => name.len(),
            NameUnit::Utf16 => name.encode_utf16().count(),
        }
    }
}

/// The longest file name `dir` accepts: `pathconf(_PC_NAME_MAX)` on Unix (255 bytes if
/// unreported), 255 UTF-16 units on Windows.
#[must_use]
pub fn name_limit(dir: &Path) -> NameLimit {
    platform::name_limit(dir)
}

/// A request to end the process: an interrupt or termination signal on Unix,
/// a console control event on Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Termination {
    name: &'static str,
    exit_code: u8,
    closing: bool,
}

impl Termination {
    /// Creates a termination; platform handlers and tests use this.
    #[must_use]
    pub const fn new(name: &'static str, exit_code: u8, closing: bool) -> Self {
        Self {
            name,
            exit_code,
            closing,
        }
    }

    /// What asked, such as `SIGTERM` or `CTRL_C_EVENT`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// The exit status to report: `128` plus the signal number, as a shell does. On Windows an
    /// interrupt or break maps to `130` and a close, logoff or shutdown to `143`.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        self.exit_code
    }

    /// Whether the OS kills the process once the handler returns (Windows console close, logoff,
    /// shutdown), so cleanup must finish before returning.
    #[must_use]
    pub const fn closing(self) -> bool {
        self.closing
    }
}

/// Calls `handler` on its own thread when the process is asked to terminate, instead of ending
/// at once. [`watch_parent`] calls the same handler.
///
/// On Unix it handles `SIGTERM`, `SIGINT` and `SIGHUP`; a second signal of a kind ends the process
/// normally. On Windows it handles console control events; a second interrupt or break calls
/// [`exit_without_handlers`]. Close, logoff and shutdown events only reach processes that have not
/// loaded `user32.dll`. `TerminateProcess` and `SIGKILL` cannot be handled.
///
/// # Errors
/// When a handler is already installed, or the operating system refuses one.
pub fn on_termination(handler: impl Fn(Termination) + Send + Sync + 'static) -> io::Result<()> {
    platform::on_termination(Box::new(handler))
}

/// A point on a clock that stops while the system sleeps, so stall checks ignore suspend.
///
/// `std::time::Instant` already stops during sleep on Linux and macOS; on Windows this reads
/// `QueryUnbiasedInterruptTimePrecise` (100 ns units).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct AwakeInstant(#[cfg(windows)] u64, #[cfg(not(windows))] Instant);

impl AwakeInstant {
    /// The current instant.
    #[must_use]
    pub fn now() -> Self {
        #[cfg(windows)]
        {
            Self(platform::unbiased_interrupt_time())
        }
        #[cfg(not(windows))]
        {
            Self(Instant::now())
        }
    }

    /// Awake time since `earlier`, or zero when `earlier` is later.
    #[must_use]
    pub fn duration_since(self, earlier: Self) -> Duration {
        #[cfg(windows)]
        {
            Duration::from_nanos(self.0.saturating_sub(earlier.0).saturating_mul(100))
        }
        #[cfg(not(windows))]
        {
            self.0.saturating_duration_since(earlier.0)
        }
    }

    /// The awake time since this one.
    #[must_use]
    pub fn elapsed(self) -> Duration {
        Self::now().duration_since(self)
    }

    /// The instant `duration` earlier, if the clock reaches back that far.
    #[must_use]
    pub fn checked_sub(self, duration: Duration) -> Option<Self> {
        #[cfg(windows)]
        {
            let units = u64::try_from(duration.as_nanos() / 100).ok()?;
            self.0.checked_sub(units).map(Self)
        }
        #[cfg(not(windows))]
        {
            self.0.checked_sub(duration).map(Self)
        }
    }
}

/// Whether `path` has the macOS `com.apple.quarantine` attribute, under which Gatekeeper refuses
/// to load an unnotarized library. `false` on other systems and for unreadable files.
#[must_use]
pub fn quarantined(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        platform::quarantined(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

/// The arguments as UTF-8 strings. Unlike `std::env::args`, a non-UTF-8 argument is an error,
/// not a panic.
///
/// # Errors
/// A message naming the first non-UTF-8 argument, lossily decoded.
pub fn utf8_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Vec<String>, String> {
    arguments
        .map(|argument| {
            argument.into_string().map_err(|argument| {
                format!(
                    "argument {:?} is not valid UTF-8",
                    argument.to_string_lossy()
                )
            })
        })
        .collect()
}

/// The process's memory, as the operating system counts it.
///
/// Resident memory includes shared pages. The footprint is what memory limits and the OOM killer
/// use: `RssAnon + RssShmem` on Linux (peak from `VmHWM`, which also counts file pages),
/// `ri_phys_footprint` on macOS, `PrivateUsage` on Windows (peak `PeakPagefileUsage`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessMemory {
    /// Resident bytes now (the working set on Windows).
    pub resident_bytes: Option<u64>,
    /// Peak resident bytes so far.
    pub peak_resident_bytes: Option<u64>,
    /// Footprint bytes now.
    pub footprint_bytes: Option<u64>,
    /// Peak footprint bytes so far.
    pub peak_footprint_bytes: Option<u64>,
}

/// The `major.minor.build.revision` from the Windows version resource of `path`; `None` elsewhere
/// or without one.
#[must_use]
pub fn file_version(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        platform::file_version(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// The process's memory now and at its peak; unavailable figures are `None`.
#[must_use]
pub fn process_memory() -> ProcessMemory {
    #[cfg(any(unix, windows))]
    {
        platform::process_memory()
    }
    #[cfg(not(any(unix, windows)))]
    {
        ProcessMemory::default()
    }
}

/// Handles the process holds open: file descriptors on Unix, kernel handles on Windows. `None`
/// where unavailable.
#[must_use]
pub fn open_handles() -> Option<u64> {
    #[cfg(any(unix, windows))]
    {
        platform::open_handles()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The soft limit on open file descriptors; `None` on Windows or where unlimited or unknown.
#[must_use]
pub fn open_file_limit() -> Option<u64> {
    #[cfg(unix)]
    {
        platform::open_file_limit()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The calling thread's CPU work so far, comparable only with other readings on the same thread:
/// CPU nanoseconds on Unix, cycles on Windows (`QueryThreadCycleTime`). Excludes time spent
/// waiting for a CPU. `None` where unavailable.
#[must_use]
pub fn thread_cpu_work() -> Option<u64> {
    #[cfg(any(unix, windows))]
    {
        platform::thread_cpu_work()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Asks the scheduler to keep the calling thread on the fastest cores, so [`thread_cpu_work`]
/// readings come from one kind of core: `QOS_CLASS_USER_INTERACTIVE` on Apple systems, no
/// `EcoQoS` throttling on Windows, nothing elsewhere. Best effort; failures are silent.
pub fn prefer_fast_cores() {
    #[cfg(any(target_vendor = "apple", windows))]
    platform::prefer_fast_cores();
}

/// Caps glibc's malloc arenas at `arenas` unless `MALLOC_ARENA_MAX` is set. Does nothing elsewhere.
///
/// Long-lived allocations spread over many per-thread arenas pin their pages, so a process that
/// starts threads per request keeps growing. The cap applies to arenas created after the call.
pub fn limit_heap_arenas(arenas: u16) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    platform::limit_heap_arenas(arenas);
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = arenas;
}

/// User plus system CPU time of all the process's threads so far; `None` where unavailable.
#[must_use]
pub fn process_cpu_time() -> Option<std::time::Duration> {
    #[cfg(any(unix, windows))]
    {
        platform::process_cpu_time()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Writes one line to standard error like `eprintln!`, but drops it instead of panicking when the
/// stream is closed, so a client closing the pipe cannot abort an export. Use [`diagnostic!`].
pub fn write_diagnostic(line: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(io::stderr().lock(), "{line}");
}

/// `eprintln!` that drops the line instead of panicking ([`write_diagnostic`]).
#[macro_export]
macro_rules! diagnostic {
    ($($argument:tt)*) => {
        $crate::write_diagnostic(::std::format_args!($($argument)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};

    #[test]
    fn the_awake_clock_advances_with_time_and_never_goes_back() {
        let started = AwakeInstant::now();
        std::thread::sleep(Duration::from_millis(200));
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(5),
            "{elapsed:?} for a 200 ms sleep"
        );
        let mut last = AwakeInstant::now();
        for _ in 0..10_000 {
            let now = AwakeInstant::now();
            assert!(now >= last, "{now:?} after {last:?}");
            last = now;
        }
        assert_eq!(
            started
                .checked_sub(Duration::from_millis(1))
                .map(|earlier| started.duration_since(earlier)),
            Some(Duration::from_millis(1))
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_claimed_stream_keeps_stray_writes_out() {
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let pipe = || {
            let mut ends = [0; 2];
            // SAFETY: `ends` holds the two descriptors `pipe` writes.
            assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
            // SAFETY: both descriptors are new and owned here.
            unsafe { (File::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
        };
        let stray = |fd: i32| {
            // SAFETY: the bytes are valid for the call; a write to a closed fd fails harmlessly.
            unsafe { libc::write(fd, b"stray".as_ptr().cast(), 5) }
        };
        for diagnostics_open in [true, false] {
            let (mut protocol_read, protocol_write) = pipe();
            let (mut diagnostics_read, diagnostics_write) = pipe();
            let diagnostics_fd = if diagnostics_open {
                diagnostics_write.as_raw_fd()
            } else {
                -1
            };
            let mut claimed =
                platform::claim_stream(protocol_write.as_raw_fd(), diagnostics_fd).expect("claim");
            stray(protocol_write.as_raw_fd());
            claimed.write_all(b"frame").expect("frame");
            drop((claimed, protocol_write, diagnostics_write));
            let mut protocol = String::new();
            protocol_read
                .read_to_string(&mut protocol)
                .expect("protocol");
            assert_eq!(protocol, "frame", "diagnostics open: {diagnostics_open}");
            let mut diagnostics = String::new();
            diagnostics_read
                .read_to_string(&mut diagnostics)
                .expect("diagnostics");
            let expected = if diagnostics_open { "stray" } else { "" };
            assert_eq!(diagnostics, expected);
        }
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn opened_files_count_as_open_handles() {
        let path = std::env::temp_dir().join(format!("alpha2-os-handles-{}", std::process::id()));
        fs::write(&path, b"x").expect("write");
        let before = open_handles().expect("open handles");
        // Other test threads open and close handles meanwhile, so a margin absorbs them.
        let opened = (0..64)
            .map(|_| File::open(&path).expect("open"))
            .collect::<Vec<_>>();
        let during = open_handles().expect("open handles");
        assert!(during >= before + 32, "{before} then {during}");
        drop(opened);
        fs::remove_file(&path).expect("remove");
        #[cfg(unix)]
        assert!(open_file_limit().is_none_or(|limit| limit >= during));
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    fn the_process_reports_its_resident_memory() {
        let memory = process_memory();
        let resident = memory.resident_bytes.expect("resident bytes");
        let peak = memory.peak_resident_bytes.expect("peak resident bytes");
        assert!(resident > 1 << 20, "{memory:?}");
        assert!(peak >= resident, "{memory:?}");
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn the_process_cpu_time_grows_with_its_work() {
        let started = process_cpu_time().expect("process CPU time");
        let mut state = 1_u64;
        let busy = std::time::Instant::now();
        while busy.elapsed() < std::time::Duration::from_millis(200) {
            state = std::hint::black_box(state.wrapping_mul(6_364_136_223_846_793_005));
        }
        let worked = process_cpu_time()
            .expect("process CPU time")
            .saturating_sub(started);
        // Windows CPU clocks tick in steps of up to 16 ms.
        assert!(
            worked >= std::time::Duration::from_millis(150),
            "{worked:?} of CPU time for 200 ms of work"
        );
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn the_thread_counts_the_work_it_does_and_not_the_time_it_sleeps() {
        let started = thread_cpu_work().expect("thread work");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let slept = thread_cpu_work().expect("thread work") - started;
        let mut state = 1_u64;
        let busy = std::time::Instant::now();
        while busy.elapsed() < std::time::Duration::from_millis(200) {
            state = std::hint::black_box(state.wrapping_mul(6_364_136_223_846_793_005));
        }
        let worked = thread_cpu_work().expect("thread work") - started - slept;
        assert!(worked > slept * 4, "slept {slept}, worked {worked}");
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("alpha2-os-{}-{name}", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }

    fn open(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("open")
    }

    /// Serializes lock-release tests with the test that spawns a child: between `fork` and `exec`
    /// the child holds copies of every descriptor, keeping their locks alive.
    pub(crate) static FORK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn one_handle_at_a_time_holds_the_lock_and_closing_it_releases_it() {
        let _fork = FORK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = scratch("lock");
        let first = open(&path);
        let second = open(&path);
        assert!(try_lock_exclusive(&first).expect("lock"));
        assert!(!try_lock_exclusive(&second).expect("second lock"));
        drop(first);
        assert!(try_lock_exclusive(&second).expect("lock after release"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn another_handle_writes_the_whole_of_a_locked_file() {
        use std::io::{Read, Write};
        let path = scratch("write");
        let lock = open(&path);
        assert!(try_lock_exclusive(&lock).expect("lock"));
        let mut writer = File::create(&path).expect("writer truncates and opens");
        writer
            .write_all(&[7_u8; 4096])
            .expect("write under the lock");
        drop(writer);
        let mut read = Vec::new();
        File::open(&path)
            .expect("reader")
            .read_to_end(&mut read)
            .expect("read under the lock");
        assert_eq!(read, vec![7_u8; 4096]);
        drop(lock);
        let _ = fs::remove_file(&path);
    }

    /// Names the file [`lock_probe_child`] tries to lock.
    const LOCK_PROBE: &str = "ALPHA2_OS_LOCK_PROBE";

    /// Whether a child process ([`lock_probe_child`]) can take the lock on `path`.
    fn another_process_takes_the_lock(path: &Path) -> bool {
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::lock_probe_child",
                "--ignored",
                "--test-threads=1",
            ])
            .env(LOCK_PROBE, path)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run the lock probe");
        match status.code() {
            Some(0) => true,
            Some(3) => false,
            _ => panic!("the lock probe failed: {status}"),
        }
    }

    /// Child side of [`another_process_takes_the_lock`]: exits `0` if it got the lock, `3` if not.
    #[test]
    #[ignore = "run by another_process_takes_the_lock"]
    fn lock_probe_child() {
        let path = std::env::var_os(LOCK_PROBE).expect("the parent names the file");
        let locked = try_lock_exclusive(&open(Path::new(&path))).expect("probe lock");
        std::process::exit(if locked { 0 } else { 3 });
    }

    /// `FFmpeg` opens and closes its own handles on the locked file; a per-process `fcntl` lock
    /// would be released by the first close.
    #[test]
    fn the_lock_stays_with_its_handle_when_another_handle_closes() {
        let _fork = FORK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = scratch("other-handle");
        let lock = open(&path);
        assert!(try_lock_exclusive(&lock).expect("lock"));
        drop(File::create(&path).expect("another handle opens and closes"));
        drop(open(&path));
        assert!(
            !another_process_takes_the_lock(&path),
            "closing another handle released the lock"
        );
        drop(lock);
        assert!(
            another_process_takes_the_lock(&path),
            "closing the handle that holds the lock releases it"
        );
        let _ = fs::remove_file(&path);
    }

    /// The Linux lock covers only the byte at `i64::MAX`, so CIFS still lets `FFmpeg` write the
    /// whole file.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_lock_covers_only_one_byte_no_write_reaches() {
        use std::os::fd::AsRawFd;
        let path = scratch("one-byte");
        let lock = open(&path);
        assert!(try_lock_exclusive(&lock).expect("lock"));
        let other = open(&path);
        let probe = |start: i64, len: i64| {
            // SAFETY: an all-zero `flock` is valid; the fields the query reads are set below.
            let mut query: libc::flock = unsafe { std::mem::zeroed() };
            query.l_type = libc::c_short::try_from(libc::F_WRLCK).expect("F_WRLCK");
            query.l_whence = libc::c_short::try_from(libc::SEEK_SET).expect("SEEK_SET");
            query.l_start = start;
            query.l_len = len;
            // SAFETY: `other` is open for the call and `query` outlives it.
            let asked =
                unsafe { libc::fcntl(other.as_raw_fd(), libc::F_OFD_GETLK, &raw mut query) };
            assert_eq!(asked, 0, "F_OFD_GETLK: {}", io::Error::last_os_error());
            query
        };
        let everything_below = probe(0, i64::MAX);
        assert_eq!(
            i32::from(everything_below.l_type),
            libc::F_UNLCK,
            "no byte a write can reach is locked"
        );
        let last = probe(0, 0);
        assert_eq!(
            i32::from(last.l_type),
            libc::F_WRLCK,
            "the lock is an OFD lock"
        );
        // The kernel reports a lock reaching the last offset with `l_len` 0 (to end of file).
        assert_eq!(last.l_start, i64::MAX);
        drop(lock);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_path_moved_away_and_created_again_is_another_file() {
        let (path, moved) = (scratch("identity"), scratch("identity-moved"));
        let file = open(&path);
        assert!(same_file(&file, &path).expect("same"));
        fs::rename(&path, &moved).expect("move away");
        assert!(!same_file(&file, &path).expect("missing"));
        drop(open(&path));
        assert!(!same_file(&file, &path).expect("replaced"));
        assert!(same_file(&file, &moved).expect("moved"));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&moved);
    }

    #[test]
    #[cfg_attr(
        target_os = "android",
        ignore = "SELinux denies hard links to apps and the shell"
    )]
    fn a_hard_link_and_a_path_through_a_linked_directory_are_the_same_file() {
        let directory = scratch("identity-paths");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("directory");
        let (file, copy, link) = (
            directory.join("file"),
            directory.join("copy"),
            directory.join("link"),
        );
        fs::write(&file, b"source").expect("file");
        fs::write(&copy, b"source").expect("copy");
        fs::hard_link(&file, &link).expect("hard link");
        assert!(same_file_at(&file, &file).expect("itself"));
        assert!(same_file_at(&file, &link).expect("hard link"));
        assert!(!same_file_at(&file, &copy).expect("copy"));
        assert!(!same_file_at(&file, &directory.join("missing")).expect("missing"));
        let linked = scratch("identity-paths-linked");
        let _ = fs::remove_file(&linked);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&directory, &linked).expect("symlink");
        // Windows symlinks need developer mode or admin; a junction needs no privilege.
        #[cfg(windows)]
        {
            let _fork = FORK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(&linked)
                .arg(&directory)
                .stdout(std::process::Stdio::null())
                .status()
                .expect("run mklink");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        assert!(same_file_at(&file, &linked.join("file")).expect("through the link"));
        assert!(!same_file_at(&copy, &linked.join("file")).expect("another file"));
        #[cfg(unix)]
        fs::remove_file(&linked).expect("cleanup");
        #[cfg(windows)]
        fs::remove_dir(&linked).expect("cleanup");
        fs::remove_dir_all(&directory).expect("cleanup");
    }

    #[test]
    fn a_removed_file_still_open_is_not_the_file_its_path_names() {
        // SMB keeps a removed file's name until its last handle closes.
        let path = scratch("identity-removed");
        let file = open(&path);
        fs::remove_file(&path).expect("remove");
        assert!(!same_file(&file, &path).expect("removed"));
        drop(file);
        let again = open(&path);
        assert!(same_file(&again, &path).expect("created again"));
        drop(again);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_publish_moves_a_file_to_a_free_name_and_never_replaces_one() {
        let (from, to) = (scratch("publish-from"), scratch("publish-to"));
        fs::write(&from, b"export").expect("from");
        fs::write(&to, b"someone else's").expect("to");
        let refused = publish_no_replace(&from, &to, &RetryPolicy::NONE)
            .expect_err("an existing file is not replaced");
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).expect("to"), b"someone else's");
        assert_eq!(fs::read(&from).expect("from"), b"export");
        fs::remove_file(&to).expect("free the name");
        // Expect the placeholder method on file systems that refuse the no-replace flag (exFAT).
        let (probe_from, probe_to) = (scratch("publish-probe-from"), scratch("publish-probe-to"));
        fs::write(&probe_from, b"probe").expect("probe");
        let method = match platform::rename_exclusive(&probe_from, &probe_to) {
            Err(error) if platform::flag_refused(&error) => PublishMethod::PlaceholderRename,
            answer => {
                answer.expect("the no-replace rename");
                PublishMethod::RenameNoReplace
            }
        };
        let _ = (fs::remove_file(&probe_from), fs::remove_file(&probe_to));
        let published =
            publish_no_replace(&from, &to, &RetryPolicy::NONE).expect("publish to a free name");
        assert_eq!(published, Published { method, retries: 0 });
        assert_eq!(fs::read(&to).expect("to"), b"export");
        assert!(!from.exists());
        let _ = fs::remove_file(&to);
    }

    #[test]
    fn a_replacing_publish_moves_a_file_over_another() {
        let (from, to) = (scratch("replace-from"), scratch("replace-to"));
        fs::write(&from, b"export").expect("from");
        fs::write(&to, b"old export").expect("to");
        let published = publish_replace(&from, &to, &RetryPolicy::NONE).expect("replace");
        assert_eq!(published.method, PublishMethod::RenameReplace);
        assert_eq!(fs::read(&to).expect("to"), b"export");
        assert!(!from.exists());
        let _ = fs::remove_file(&to);
    }

    /// The error a file system without a no-replace rename returns.
    fn flag_refused_error() -> io::Error {
        #[cfg(unix)]
        return io::Error::from_raw_os_error(libc::EINVAL);
        #[cfg(windows)]
        return io::Error::from_raw_os_error(50);
    }

    #[test]
    fn a_file_system_without_the_no_replace_flag_publishes_through_a_placeholder() {
        let (from, to) = (scratch("placeholder-from"), scratch("placeholder-to"));
        let _ = fs::remove_file(&to);
        fs::write(&from, b"export").expect("from");
        let no_flag = |_: &Path, _: &Path| Err(flag_refused_error());
        let published = publish_no_replace_with(&from, &to, &RetryPolicy::NONE, &no_flag)
            .expect("publish through a placeholder");
        assert_eq!(published.method, PublishMethod::PlaceholderRename);
        assert_eq!(fs::read(&to).expect("to"), b"export");
        assert!(!from.exists());
        fs::write(&from, b"second export").expect("from");
        let refused = publish_no_replace_with(&from, &to, &RetryPolicy::NONE, &no_flag)
            .expect_err("an existing file is not replaced");
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).expect("to"), b"export");
        assert_eq!(fs::read(&from).expect("from"), b"second export");
        let _ = fs::remove_file(&from);
        let _ = fs::remove_file(&to);
    }

    /// A second name for a published file would let an overwrite of either name truncate the
    /// other.
    #[test]
    #[cfg(unix)]
    fn a_publish_never_leaves_a_second_link_to_the_published_file() {
        use std::os::unix::fs::MetadataExt;
        let (from, to) = (scratch("one-link-from"), scratch("one-link-to"));
        let _ = fs::remove_file(&to);
        fs::write(&from, b"export").expect("from");
        let no_flag = |_: &Path, _: &Path| Err(flag_refused_error());
        publish_no_replace_with(&from, &to, &RetryPolicy::NONE, &no_flag).expect("publish");
        assert_eq!(fs::metadata(&to).expect("to").nlink(), 1);
        assert!(!from.exists());
        let _ = fs::remove_file(&to);
    }

    #[test]
    fn a_failure_another_handle_causes_is_tried_again_until_the_budget_ends() {
        let busy = || io::Error::from(io::ErrorKind::ResourceBusy);
        let retryable = |error: &io::Error| error.kind() == io::ErrorKind::ResourceBusy;
        let policy = RetryPolicy {
            budget: Duration::from_secs(10),
            on_wait: &|| true,
        };
        let mut tries = 0;
        let (value, retries) = retrying(&policy, retryable, || {
            tries += 1;
            if tries < 3 { Err(busy()) } else { Ok(tries) }
        })
        .expect("the third try succeeds");
        assert_eq!((value, retries), (3, 2));

        let short = RetryPolicy {
            budget: Duration::from_millis(200),
            on_wait: &|| true,
        };
        let started = Instant::now();
        let error = retrying(&short, retryable, || Err::<(), _>(busy())).expect_err("never");
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5),
            "{elapsed:?}"
        );

        let cancelled = RetryPolicy {
            budget: Duration::from_secs(10),
            on_wait: &|| false,
        };
        let mut tries = 0;
        assert!(
            retrying(&cancelled, retryable, || {
                tries += 1;
                Err::<(), _>(busy())
            })
            .is_err()
        );
        assert_eq!(tries, 1, "a cancelled wait stops trying");

        let mut tries = 0;
        assert!(
            retrying(&policy, retryable, || {
                tries += 1;
                Err::<(), _>(io::Error::from(io::ErrorKind::NotFound))
            })
            .is_err()
        );
        assert_eq!(tries, 1, "another error is not tried again");
    }

    #[test]
    #[cfg(unix)]
    fn a_directory_syncs() {
        sync_directory(&std::env::temp_dir()).expect("sync the temporary directory");
    }

    #[test]
    fn the_name_limit_of_the_temporary_directory_is_known() {
        let limit = name_limit(&std::env::temp_dir());
        assert!(limit.max >= 143, "{limit:?}");
        assert_eq!(limit.length("ab"), 2);
        assert_eq!(
            limit.length("\u{d55c}"),
            if limit.unit == NameUnit::Bytes { 3 } else { 1 }
        );
    }

    /// A directory whose paths exceed `MAX_PATH` once a file name is added.
    #[cfg(windows)]
    fn long_directory(label: &str) -> std::path::PathBuf {
        let root = scratch(label);
        let _ = fs::remove_dir_all(&root);
        let directory = root.join("a".repeat(120)).join("b".repeat(120));
        fs::create_dir_all(&directory).expect("long directory");
        directory
    }

    #[test]
    #[cfg(windows)]
    fn a_publish_reaches_a_path_longer_than_max_path() {
        let directory = long_directory("long-path");
        let (from, to) = (directory.join("from.mp4"), directory.join("to.mp4"));
        assert!(!to.as_os_str().to_string_lossy().starts_with(r"\\?\"));
        assert!(to.as_os_str().len() > 260);
        fs::write(&from, b"export").expect("from");
        let published = publish_no_replace(&from, &to, &RetryPolicy::NONE).expect("no replace");
        assert_eq!(published.method, PublishMethod::RenameNoReplace);
        fs::write(&from, b"second export").expect("from");
        publish_replace(&from, &to, &RetryPolicy::NONE).expect("replace");
        assert_eq!(fs::read(&to).expect("to"), b"second export");
        let _ = fs::remove_dir_all(scratch("long-path"));
    }

    /// Opens `path` with `FILE_SHARE_READ` only, as media players and scanners do.
    #[cfg(windows)]
    fn hold_without_delete_sharing(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(path)
            .expect("hold the file")
    }

    #[test]
    #[cfg(windows)]
    fn a_publish_waits_for_a_reader_without_delete_sharing() {
        let policy = RetryPolicy {
            budget: Duration::from_secs(10),
            on_wait: &|| true,
        };
        let (from, to) = (scratch("held-from"), scratch("held-to"));
        let _ = fs::remove_file(&to);
        fs::write(&from, b"export").expect("from");
        fs::write(&to, b"old export").expect("to");
        let holder = hold_without_delete_sharing(&to);
        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(holder);
        });
        let published = publish_replace(&from, &to, &policy).expect("replace once released");
        released.join().expect("holder");
        assert!(published.retries >= 1, "{published:?}");
        assert_eq!(fs::read(&to).expect("to"), b"export");
        fs::remove_file(&to).expect("free the name");

        fs::write(&from, b"export").expect("from");
        let holder = hold_without_delete_sharing(&from);
        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(holder);
        });
        let published = publish_no_replace(&from, &to, &policy).expect("publish once released");
        released.join().expect("holder");
        assert!(published.retries >= 1, "{published:?}");
        fs::remove_file(&to).expect("free the name");

        fs::write(&from, b"export").expect("from");
        let holder = hold_without_delete_sharing(&from);
        let short = RetryPolicy {
            budget: Duration::from_millis(500),
            on_wait: &|| true,
        };
        let error = publish_no_replace(&from, &to, &short).expect_err("never released");
        assert!(is_retryable_publish_error(&error), "{error}");
        drop(holder);
        assert!(from.exists(), "a refused publish keeps its source");
        let _ = fs::remove_file(&from);
    }

    #[test]
    #[cfg(windows)]
    fn two_names_of_one_file_have_one_identity_and_two_files_two() {
        let directory = scratch("file-id");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("directory");
        let (file, link, copy) = (
            directory.join("file"),
            directory.join("link"),
            directory.join("copy"),
        );
        fs::write(&file, b"source").expect("file");
        fs::write(&copy, b"source").expect("copy");
        fs::hard_link(&file, &link).expect("hard link");
        let id = |path: &Path| {
            platform::file_id_info(&File::open(path).expect("open")).expect("FileIdInfo")
        };
        assert_eq!(id(&file), id(&link));
        assert_ne!(id(&file), id(&copy));
        assert_eq!(
            platform::identity(&File::open(&file).expect("open")).expect("identity"),
            id(&file),
            "NTFS answers FileIdInfo, so the 64-bit index is not used"
        );
        fs::remove_dir_all(&directory).expect("cleanup");
    }
}
