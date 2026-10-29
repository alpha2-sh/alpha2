//! The output lock (an open-file-description lock on one unreachable byte on
//! Linux, `flock` elsewhere), file identity by device and inode, the
//! no-replace rename, `SIGTERM`/`SIGINT` through a self-pipe, and resident
//! memory.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::{ProcessMemory, Termination};

/// The byte the Linux lock covers: past any offset a signed 64-bit file
/// position, `FFmpeg`'s included, can write (as on Windows).
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_pointer_width = "64"
))]
const LOCK_OFFSET: libc::off_t = i64::MAX;

/// On Linux, an open-file-description lock (`F_OFD_SETLK`) on the one byte at
/// [`LOCK_OFFSET`]; `flock` where the kernel or file system does not take
/// that (`EINVAL`).
///
/// Not `flock`: on a CIFS mount (default `brl`) the client sends `flock` to
/// the server as a mandatory byte-range lock over the whole file, so the
/// server refuses every write through another handle, `FFmpeg`'s and the WAV
/// writer's, with `EACCES`. Not a traditional `F_SETLK` lock either: that
/// belongs to the process, and goes the moment `FFmpeg` closes any handle of
/// the file. An open-file-description lock belongs to this handle, as `flock`
/// does, and covers a byte no write reaches.
///
/// Only 64-bit targets: where `off_t` is 32 bits the byte would be at 2 GiB,
/// within reach of a large export, so those keep `flock`.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_pointer_width = "64"
))]
pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    match lock_unreachable_byte(file) {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => flock_exclusive(file),
        locked => locked,
    }
}

/// `flock`: macOS has no open-file-description lock, and its SMB client is
/// not the Linux one.
#[cfg(not(all(
    any(target_os = "linux", target_os = "android"),
    target_pointer_width = "64"
)))]
pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    flock_exclusive(file)
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_pointer_width = "64"
))]
fn lock_unreachable_byte(file: &File) -> io::Result<bool> {
    // SAFETY: an all-zero `flock` is valid, and `l_pid` must be zero for an open-file-description
    // lock; the range is set below.
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    // Both constants are small (1 and 0) and fit the struct's `short` fields.
    #[allow(clippy::cast_possible_truncation)]
    {
        range.l_type = libc::F_WRLCK as libc::c_short;
        range.l_whence = libc::SEEK_SET as libc::c_short;
    }
    range.l_start = LOCK_OFFSET;
    range.l_len = 1;
    loop {
        // SAFETY: `fcntl` takes any descriptor and reports a bad one as an error; `file` keeps this
        // one open for the call, and `range` is a valid `flock` that outlives it.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &raw const range) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // POSIX lets a refused lock answer either.
            Some(libc::EAGAIN | libc::EACCES) => return Ok(false),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

fn flock_exclusive(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: `flock` takes any descriptor and reports a bad one as an error; `file` keeps this
        // one open for the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::WouldBlock => return Ok(false),
            io::ErrorKind::Interrupted => {}
            _ => return Err(error),
        }
    }
}

/// Device and inode, and only while the held file still has a name: on a
/// Linux CIFS mount a removed file that is still open keeps its name (the SMB
/// server deletes it when its last handle closes), and `path` still reports
/// that file, with no links, until then.
pub(crate) fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    let held = file.metadata()?;
    if held.nlink() == 0 {
        return Ok(false);
    }
    match fs::metadata(path) {
        Ok(named) => Ok(named.dev() == held.dev() && named.ino() == held.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn same_file_at(a: &Path, b: &Path) -> io::Result<bool> {
    let identity = |path: &Path| match fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    Ok(match (identity(a)?, identity(b)?) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    })
}

pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_exclusive(from, to) {
        Err(error) if flag_refused(&error) => {}
        renamed => return renamed,
    }
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

/// The platform's no-replace rename; `ENOTSUP` where it has none.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both are NUL-terminated paths that outlive the call; `AT_FDCWD` resolves relative
    // ones as `rename` does.
    let renamed = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if renamed == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both are NUL-terminated paths that outlive the call.
    let renamed = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if renamed == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
fn rename_exclusive(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// Whether a rename failed because the file system or kernel does not take
/// its no-replace flag, rather than for the files' sake.
fn flag_refused(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
    )
}

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// The pipe's write end, which the signal handler writes the signal's number
/// to; `-1` until a handler is installed.
static WAKE: AtomicI32 = AtomicI32::new(-1);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Runs on whatever thread the signal interrupted, so it does only what is
/// async-signal-safe: one `write` to the pipe, with `errno` kept for the code
/// it interrupted.
extern "C" fn on_signal(signal: libc::c_int) {
    let saved = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let fd = WAKE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = u8::try_from(signal).unwrap_or(u8::MAX);
        // SAFETY: `fd` is the pipe's write end, open for the life of the process, and `byte` is one
        // readable byte on this stack.
        let _ = unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
    }
    // SAFETY: the location is this thread's `errno`, valid for the thread's life.
    unsafe { *errno_location() = saved };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid on the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid on the calling thread.
    unsafe { libc::__error() }
}

fn termination(signal: u8) -> Termination {
    let (name, number) = match i32::from(signal) {
        libc::SIGINT => ("SIGINT", libc::SIGINT),
        _ => ("SIGTERM", libc::SIGTERM),
    };
    Termination {
        name,
        exit_code: u8::try_from(128 + number).unwrap_or(u8::MAX),
        closing: false,
    }
}

pub(crate) fn on_termination(handler: Box<dyn Fn(Termination) + Send + Sync>) -> io::Result<()> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a termination handler is already installed",
        ));
    }
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: `fd` was just opened by `pipe`. A child the process starts must not inherit
        // either end.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: the read end is open and owned by nothing else; the `File` takes it over.
    let mut wake = unsafe { File::from_raw_fd(fds[0]) };
    WAKE.store(fds[1], Ordering::Release);
    std::thread::Builder::new()
        .name("alpha2-termination".to_owned())
        .spawn(move || {
            let mut signal = [0_u8];
            loop {
                match wake.read(&mut signal) {
                    Ok(1) => handler(termination(signal[0])),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    _ => return,
                }
            }
        })?;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct, whose fields are set
        // below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // Restarting keeps a read the signal interrupts from failing; resetting makes the second
        // signal of a kind end the process as it would have.
        action.sa_flags = libc::SA_RESTART | libc::SA_RESETHAND;
        // SAFETY: `action` is a valid, initialised struct and the handler is async-signal-safe.
        let installed = unsafe {
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(signal, &raw const action, std::ptr::null_mut())
        };
        if installed != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `VmRSS` and `VmHWM` of `/proc/self/status`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn process_memory() -> ProcessMemory {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kib = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|value| value.saturating_mul(1_024))
    };
    ProcessMemory {
        resident_bytes: kib("VmRSS:"),
        peak_resident_bytes: kib("VmHWM:"),
    }
}

/// The task's resident size (`proc_pid_rusage`) and `ru_maxrss`, which
/// macOS counts in bytes.
#[cfg(target_os = "macos")]
pub(crate) fn process_memory() -> ProcessMemory {
    // SAFETY: an all-zero `rusage_info_v2` is valid; the call fills it.
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a `rusage_info_v2`, the flavor asked for, and outlives the call; the
    // buffer argument is that struct's address passed as `rusage_info_t*`, as the C prototype takes
    // it.
    let resident = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            std::ptr::addr_of_mut!(usage).cast(),
        )
    } == 0;
    // SAFETY: an all-zero `rusage` is valid; the call fills it.
    let mut own: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `own` outlives the call.
    let peak = unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut own) } == 0;
    ProcessMemory {
        resident_bytes: resident.then_some(usage.ri_resident_size),
        peak_resident_bytes: peak.then(|| u64::try_from(own.ru_maxrss).ok()).flatten(),
    }
}

/// Neither figure is read on this Unix.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub(crate) fn process_memory() -> ProcessMemory {
    ProcessMemory::default()
}

/// CPU nanoseconds the calling thread has run.
pub(crate) fn thread_cpu_work() -> Option<u64> {
    // SAFETY: an all-zero `timespec` is valid; the call fills it.
    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `now` outlives the call, which writes only it.
    let read = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut now) } == 0;
    let seconds = u64::try_from(now.tv_sec).ok()?;
    let nanoseconds = u64::try_from(now.tv_nsec).ok()?;
    read.then(|| {
        seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(nanoseconds)
    })
}
