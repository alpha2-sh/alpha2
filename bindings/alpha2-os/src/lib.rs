//! The operating-system calls the native host makes that the standard library
//! does not offer at this workspace's minimum Rust version, behind safe
//! functions: an advisory lock on a file, a rename that never replaces its
//! target, a handler for a request to terminate the process, the process's
//! resident memory, the work the calling thread has done, and a diagnostic
//! line on standard error that a closed stream does not turn into a panic.
//!
//! The rest of the workspace forbids `unsafe`; this crate, like
//! `alpha2-ffmpeg-bridge`, is where the few calls that need it are kept and
//! reviewed.

use std::fs::File;
use std::io;
use std::path::Path;

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

/// Renames `from` to `to` unless `to` exists, in one step: a file that
/// appears at `to` at any moment, up to the rename itself, is never replaced,
/// and the rename fails with [`io::ErrorKind::AlreadyExists`].
///
/// `std::fs::rename` replaces whatever is at `to`. This is
/// `renameat2(RENAME_NOREPLACE)` on Linux, `renamex_np(RENAME_EXCL)` on
/// macOS, and `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on Windows. On
/// Unix, a file system or kernel that refuses the flag (the call answers
/// `EINVAL`, `ENOSYS` or `ENOTSUP`) gets a hard link to `to`, which fails the
/// same way when `to` exists, and then the removal of `from`.
///
/// # Errors
/// [`io::ErrorKind::AlreadyExists`] when `to` exists, else what the operating
/// system reports.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    platform::rename_no_replace(from, to)
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
            .args(["--exact", "tests::lock_probe_child", "--test-threads=1"])
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
    fn lock_probe_child() {
        let Some(path) = std::env::var_os(LOCK_PROBE) else {
            return;
        };
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
        let symlinked = std::os::unix::fs::symlink(&directory, &linked);
        #[cfg(windows)]
        let symlinked = std::os::windows::fs::symlink_dir(&directory, &linked);
        // Windows creates symbolic links only in developer mode or as an administrator.
        if symlinked.is_ok() {
            assert!(same_file_at(&file, &linked.join("file")).expect("through the link"));
            assert!(!same_file_at(&copy, &linked.join("file")).expect("another file"));
            #[cfg(unix)]
            fs::remove_file(&linked).expect("cleanup");
            #[cfg(windows)]
            fs::remove_dir(&linked).expect("cleanup");
        }
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
    fn a_rename_moves_a_file_to_a_free_name_and_never_replaces_one() {
        let (from, to) = (scratch("rename-from"), scratch("rename-to"));
        fs::write(&from, b"export").expect("from");
        fs::write(&to, b"someone else's").expect("to");
        let refused = rename_no_replace(&from, &to).expect_err("an existing file is not replaced");
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).expect("to"), b"someone else's");
        assert_eq!(fs::read(&from).expect("from"), b"export");
        fs::remove_file(&to).expect("free the name");
        rename_no_replace(&from, &to).expect("rename to a free name");
        assert_eq!(fs::read(&to).expect("to"), b"export");
        assert!(!from.exists());
        let _ = fs::remove_file(&to);
    }
}
