//! The operating-system calls the native host makes that the standard library
//! does not offer at this workspace's minimum Rust version, behind safe
//! functions: an advisory lock on a file, publishing a finished file without
//! replacing another (through a rename that never replaces its target, with
//! retries while another program holds a file and a sync of its directory), a
//! handler for a request to terminate the process, the process's
//! resident memory, the work the calling thread has done, and a diagnostic
//! line on standard error that a closed stream does not turn into a panic.
//!
//! The rest of the workspace forbids `unsafe`; this crate, like
//! `alpha2-ffmpeg-bridge`, is where the few calls that need it are kept and
//! reviewed.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

/// Takes an exclusive advisory lock on `file` without waiting: `Ok(true)`
/// when this handle now holds it, `Ok(false)` when another open handle (in
/// this process or another) does.
///
/// The lock belongs to this handle and goes with it: closing the file, or the
/// process ending in any way, crash and `SIGKILL` included, releases it. Other
/// handles of the file can still read and write all of it, so a library that
/// opens the same path itself (as `FFmpeg` does) is not affected.
///
/// It never covers a byte a write can reach where a lock would refuse other
/// handles' reads and writes of what it covers:
/// - On Windows it is `LockFileEx` on one byte at offset `i64::MAX`, because a
///   Windows byte-range lock is mandatory.
/// - On Linux it is an open-file-description lock (`fcntl(F_OFD_SETLK)`) on
///   that same byte. A Linux CIFS mount (with the default `brl`) turns a lock
///   into a mandatory byte-range lock on the server, and `flock` covers the
///   whole file there, so `FFmpeg`'s writes through its own handle would fail
///   with `EACCES`. A traditional `fcntl` lock would not do either: it belongs
///   to the process, and closing any handle of the file (as `FFmpeg` does)
///   releases it. A kernel or file system that does not take the
///   open-file-description lock (`EINVAL`) gets `flock`.
/// - On macOS and other Unix it is `flock`.
///
/// The lock is a write lock: on Linux `file` must be open for writing.
///
/// # Errors
/// When the operating system refuses the lock for a reason other than another
/// holder, for example on a file system without locks.
pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    platform::try_lock_exclusive(file)
}

/// Whether `path` names the file `file` is open on now: `false` when the path
/// no longer exists, names another file (one removed and created again), or
/// names a removed file that some handle keeps open (`file`'s own included):
/// Windows and an SMB share keep such a name until the last handle closes.
///
/// # Errors
/// When the file's identity cannot be read.
pub fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    platform::same_file(file, path)
}

/// Whether the paths `a` and `b` name one file: the same device and inode on
/// Unix, the same volume serial and file index on Windows. Hard links and
/// paths through symbolic links to one file are the same file; two copies are
/// not. `false` when either path names nothing.
///
/// # Errors
/// When either path's identity cannot be read for a reason other than
/// nothing being there.
pub fn same_file_at(a: &Path, b: &Path) -> io::Result<bool> {
    platform::same_file_at(a, b)
}

/// How [`publish_no_replace`] or [`publish_replace`] moved a finished file
/// into place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishMethod {
    /// One rename that refuses an existing target: `renameat2(RENAME_NOREPLACE)`
    /// on Linux, `renamex_np(RENAME_EXCL)` on macOS, `MoveFileExW` without
    /// `MOVEFILE_REPLACE_EXISTING` on Windows.
    RenameNoReplace,
    /// The file system refused the no-replace flag: an empty file was created
    /// at the target with `O_EXCL`, then renamed over.
    PlaceholderRename,
    /// A rename that replaces whatever is at the target.
    RenameReplace,
}

impl PublishMethod {
    /// The method's name in reports: `rename-no-replace`,
    /// `placeholder-rename` or `rename-replace`.
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
    /// How many times a step was tried again after a sharing, lock or access
    /// error ([`is_retryable_publish_error`]).
    pub retries: u32,
}

/// How long a publish keeps trying a step that another program's open handle
/// refused, and what it does between tries.
#[derive(Clone, Copy)]
pub struct RetryPolicy<'a> {
    /// The longest time, from the first try, after which a refused step is
    /// not tried again. Zero tries once.
    pub budget: Duration,
    /// Called before each wait: `false` stops trying (the caller was
    /// cancelled) and the last error is returned.
    pub on_wait: &'a dyn Fn() -> bool,
}

/// [`RetryPolicy::NONE`]'s `on_wait`.
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

/// The first wait between two tries of a refused step; each later wait
/// doubles, up to [`RETRY_MAX_DELAY`].
const RETRY_FIRST_DELAY: Duration = Duration::from_millis(50);
/// The longest wait between two tries of a refused step.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// Runs `step` until it succeeds, fails with an error
/// [`is_retryable_publish_error`] does not accept, or `retry` says to stop:
/// its budget has run out or its `on_wait` answers `false`. Waits 50 ms
/// before the first retry, doubling to 1 s. Returns the step's value and how
/// many times it was tried again.
///
/// # Errors
/// The step's last error.
pub fn with_publish_retries<T>(
    retry: &RetryPolicy<'_>,
    step: impl FnMut() -> io::Result<T>,
) -> io::Result<(T, u32)> {
    retrying(retry, is_retryable_publish_error, step)
}

/// [`with_publish_retries`] with the errors to retry chosen by `retryable`.
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

/// Whether a publish step failed because another program holds one of the
/// files open in a way that refuses it, which usually ends within seconds
/// (an antivirus scanner, a search indexer, a media player): on Windows
/// `ERROR_ACCESS_DENIED` (5), `ERROR_SHARING_VIOLATION` (32) and
/// `ERROR_LOCK_VIOLATION` (33). Never on Unix, where an open handle does not
/// stop a rename or a removal.
///
/// `ERROR_ACCESS_DENIED` is also what a real permission error answers; a
/// publish retrying one costs at most its budget.
#[must_use]
pub fn is_retryable_publish_error(error: &io::Error) -> bool {
    platform::is_retryable_publish_error(error)
}

/// Moves the finished file `from` to `to` unless something is at `to`, and
/// never replaces a file there.
///
/// It tries the platform's no-replace rename first
/// ([`PublishMethod::RenameNoReplace`]), with verbatim (`\\?\`) paths on
/// Windows so a path longer than `MAX_PATH` works. A file system that refuses
/// the flag (`EINVAL`, `ENOSYS`, `ENOTSUP` on Unix, as exFAT on macOS and FUSE
/// servers such as rclone do; `ERROR_NOT_SUPPORTED` or
/// `ERROR_INVALID_FUNCTION` on Windows) gets a placeholder instead
/// ([`PublishMethod::PlaceholderRename`]): an empty file created at `to` with
/// `O_EXCL`, which fails when `to` exists, then a plain rename over it.
///
/// The placeholder leaves two windows the one-step rename does not have, on
/// those file systems only. A process that removes the empty placeholder and
/// creates its own file at `to` before the rename loses that file. A crash
/// between the placeholder and the rename leaves an empty file at `to`. No
/// window leaves a second name of the published file: there is no hard link.
///
/// Each try that fails with [`is_retryable_publish_error`] is tried again as
/// `retry` allows ([`with_publish_retries`]).
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

/// [`publish_no_replace`], with the platform's no-replace rename
/// `rename_exclusive` (a test stands in a file system that refuses the flag).
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

/// Creates an empty file at `to` with `O_EXCL`, closes it, then renames
/// `from` over it. A rename that fails removes the placeholder, unless `to`
/// no longer names it.
///
/// The placeholder is closed before the rename: a FUSE server that writes a
/// file back when its write handle closes (rclone without a cache) would
/// otherwise write the empty placeholder back under its name after the
/// rename, over the published file or after its removal.
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

/// Moves the finished file `from` to `to`, replacing a file there
/// (`std::fs::rename`, which replaces on Windows too). Each try that fails
/// with [`is_retryable_publish_error`] is tried again as `retry` allows.
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

/// Makes the entries of directory `dir` durable: a file renamed into it
/// stays renamed after a power loss. `fsync` of the directory on Unix; a file
/// system that cannot sync a directory (`EINVAL`, `ENOTSUP`, `EBADF`) is not
/// an error. Nothing on Windows, where the no-replace rename writes through
/// and a directory cannot be synced.
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
    /// The most units a name may have.
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

/// The longest file name `dir` takes: `pathconf(_PC_NAME_MAX)` on Unix
/// (255 bytes when it does not say), 255 UTF-16 units on Windows.
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
    /// A termination as the platform reports it; also how a test describes
    /// one.
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

    /// The exit status a process that ends because of it reports: `128` plus
    /// the signal number, as a shell reports a process the signal killed. On
    /// Windows an interrupt or break reports `130` (as `SIGINT` would) and a
    /// closing console, logoff or shutdown `143` (as `SIGTERM` would).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        self.exit_code
    }

    /// Whether the operating system ends the process as soon as the handler
    /// returns (a Windows console closing, a logoff, a shutdown), so a handler
    /// that has cleanup to wait for must wait before it returns.
    #[must_use]
    pub const fn closing(self) -> bool {
        self.closing
    }
}

/// Calls `handler` on a thread of its own each time the process is asked to
/// terminate, instead of the process ending at once.
///
/// On Unix it handles `SIGTERM` and `SIGINT`. Each signal is handled once:
/// the second of a kind ends the process as it would have without this. On
/// Windows it handles the console control events; a second interrupt or
/// break ends the process. `TerminateProcess` (what Node's `kill()` does on
/// Windows) and `SIGKILL` cannot be handled, and still end it at once.
///
/// # Errors
/// When a handler is already installed, or the operating system refuses one.
pub fn on_termination(handler: impl Fn(Termination) + Send + Sync + 'static) -> io::Result<()> {
    platform::on_termination(Box::new(handler))
}

/// The process's resident memory, as the operating system counts it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessMemory {
    /// Bytes resident now: `VmRSS` on Linux, the task's resident size on
    /// macOS, the working set on Windows.
    pub resident_bytes: Option<u64>,
    /// The most bytes resident at once so far: `VmHWM` on Linux,
    /// `ru_maxrss` on macOS, the peak working set on Windows.
    pub peak_resident_bytes: Option<u64>,
}

/// The file version of the executable or DLL at `path` (`major.minor.build.revision`), from its
/// Windows version resource; `None` elsewhere or for a file without one.
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

/// The process's resident memory now and at its peak; a figure the
/// operating system does not report (or this build does not read on it) is
/// `None`.
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

/// The work the calling thread has done so far, as a count that only this
/// thread's other readings are comparable to: CPU nanoseconds on Unix
/// (`CLOCK_THREAD_CPUTIME_ID`), CPU cycles on Windows
/// (`QueryThreadCycleTime`). Time the thread spent waiting for a CPU, as on a
/// busy machine, is not in it, which is what a test that compares the work
/// of two input sizes needs. `None` where the operating system does not
/// report it (or this build does not read it there).
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

/// Asks the scheduler to run the calling thread on the fastest cores at full
/// speed, so two readings of [`thread_cpu_work`] are taken on one kind of
/// core: on macOS the `QOS_CLASS_USER_INTERACTIVE` class (a thread of a lower
/// class may move between performance and efficiency cores), on Windows out
/// of power throttling (`EcoQoS`). Nothing elsewhere.
///
/// Best effort: a refusal leaves the thread as it was, and nothing reports it.
pub fn prefer_fast_cores() {
    #[cfg(any(target_os = "macos", windows))]
    platform::prefer_fast_cores();
}

/// The CPU time the whole process has used so far, user and system together,
/// over all its threads: `getrusage(RUSAGE_SELF)` on Unix, `GetProcessTimes`
/// on Windows. `None` where the operating system does not report it (or this
/// build does not read it there).
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

/// Writes one line to standard error, the way `eprintln!` does, and drops
/// it when the stream is closed or full instead of panicking.
///
/// A host's diagnostics are for whoever reads its standard error; a client
/// that stopped reading (a protocol client that closed the pipe, a timeout
/// that closed it first) must not end an export, or a worker thread, with a
/// panic over a line nobody will read. Use it through [`diagnostic!`].
pub fn write_diagnostic(line: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(io::stderr().lock(), "{line}");
}

/// `eprintln!` for the native host and its libraries: the same line on
/// standard error, dropped when the stream cannot take it
/// ([`write_diagnostic`]).
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
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn the_process_reports_its_resident_memory() {
        let memory = process_memory();
        let resident = memory.resident_bytes.expect("resident bytes");
        let peak = memory.peak_resident_bytes.expect("peak resident bytes");
        // A running test binary holds at least a megabyte, and never more than it ever held.
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
        // 200 ms of spinning on one thread; the clocks tick in at most 16 ms steps (Windows).
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

    /// Held by the tests that check a closed handle releases its lock, and by the one that
    /// starts a child process: a child between `fork` and `exec` holds a copy of every open
    /// descriptor, so a handle another test closes meanwhile keeps its lock until the `exec`
    /// (a window valgrind makes long enough to hit).
    static FORK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// Set in a child of [`the_lock_stays_with_its_handle_when_another_handle_closes`]: the
    /// file whose lock [`lock_probe_child`] tries to take.
    const LOCK_PROBE: &str = "ALPHA2_OS_LOCK_PROBE";

    /// Whether another process can take the lock on `path` now, asked of a child process (this
    /// test binary running [`lock_probe_child`]): another process's attempt is the one every
    /// platform's lock must refuse.
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

    /// Not a test of its own: in a child that [`another_process_takes_the_lock`] starts, tries
    /// the lock on the file it names and exits `0` when it got it, `3` when another handle holds
    /// it.
    #[test]
    #[ignore = "run by another_process_takes_the_lock"]
    fn lock_probe_child() {
        let path = std::env::var_os(LOCK_PROBE).expect("the parent names the file");
        let locked = try_lock_exclusive(&open(Path::new(&path))).expect("probe lock");
        std::process::exit(if locked { 0 } else { 3 });
    }

    /// `FFmpeg` opens and closes its own handles on the partial file while the export holds the
    /// lock; a lock that belonged to the process rather than the handle (a traditional POSIX
    /// `fcntl` lock) would go when the first of them closed.
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

    /// On Linux the lock is an open-file-description lock on the byte at `i64::MAX` and nothing
    /// below it, so a file system that enforces byte-range locks on other handles' writes (CIFS)
    /// still lets `FFmpeg` write the whole file. What another handle sees through
    /// `F_OFD_GETLK` is the lock itself.
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
        // The kernel reports a lock that reaches the last offset as running to the end (`l_len`
        // 0); with nothing below it locked, that is the one byte.
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
        // Windows creates symbolic links only in developer mode or as an administrator; a
        // directory junction needs no privilege and is resolved through the same reparse point.
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
        // An SMB share (a Linux CIFS mount) keeps a removed file's name, with no links, until its
        // last handle closes, and refuses to create the name again meanwhile.
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
        // The first rung where the file system has the no-replace rename (exFAT, for one, refuses
        // the flag), the placeholder rung where it does not.
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

    /// What a file system without a no-replace rename answers: `EINVAL` from exFAT on macOS and
    /// from FUSE servers, `ERROR_NOT_SUPPORTED` on Windows.
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

    /// The fallback this replaced linked the target to the source before removing the source; a
    /// second name of a published file lets a later overwrite of either name truncate the other.
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

    /// A temporary directory with two 120-character components below it: past `MAX_PATH` once
    /// a file name is added.
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

    /// Opens `path` the way a program that does not share deletion does (a media player, an
    /// antivirus scanner): read access, `FILE_SHARE_READ` only.
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
