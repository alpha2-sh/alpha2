//! Unix implementations of the crate's calls.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::{ProcessMemory, Termination};

/// Past any offset a signed 64-bit file position can write.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_pointer_width = "64"
))]
const LOCK_OFFSET: libc::off_t = i64::MAX;

/// An open-file-description lock on the byte at [`LOCK_OFFSET`], falling back to `flock` on
/// `EINVAL`. On CIFS, `flock` becomes a mandatory whole-file lock that makes other handles' writes
/// fail with `EACCES`, and a classic `F_SETLK` lock is dropped when any handle closes.
///
/// 64-bit only: with a 32-bit `off_t` the byte would sit at 2 GiB, within reach of an export.
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

/// macOS has no open-file-description lock, and its SMB client does not need one.
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
    // SAFETY: an all-zero `flock` is valid, and OFD locks require `l_pid` to be zero.
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    {
        range.l_type = libc::F_WRLCK as libc::c_short;
        range.l_whence = libc::SEEK_SET as libc::c_short;
    }
    range.l_start = LOCK_OFFSET;
    range.l_len = 1;
    loop {
        // SAFETY: `file` keeps the descriptor open and `range` outlives the call.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &raw const range) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // POSIX allows either for a refused lock.
            Some(libc::EAGAIN | libc::EACCES) => return Ok(false),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

fn flock_exclusive(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: `file` keeps the descriptor open for the call.
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

/// Compares device and inode, but only while the held file has links: on CIFS a removed but
/// open file keeps its name until the last handle closes.
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

/// Device and inode.
pub(crate) type FileIdentity = (u64, u64);

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(crate) fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn same_file_at(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(match (path_identity(a)?, path_identity(b)?) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    })
}

/// From `<linux/fs.h>`; the same on every architecture.
#[cfg(any(target_os = "linux", target_os = "android"))]
const RENAME_NOREPLACE: libc::c_uint = 1;

/// The platform's no-replace rename; `ENOTSUP` where it has none.
///
/// Linux uses the raw `renameat2` syscall because musl and old glibc lack a wrapper. Kernels
/// before 3.15 return `ENOSYS`, which [`flag_refused`] accepts.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated and outlive the call; the arguments match the syscall.
    let renamed = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if renamed == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both are NUL-terminated paths that outlive the call.
    let renamed = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if renamed == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub(crate) fn rename_exclusive(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// Whether a rename failed because the file system or kernel does not support the no-replace
/// flag.
pub(crate) fn flag_refused(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        code == libc::EINVAL
            || code == libc::ENOSYS
            || code == libc::ENOTSUP
            || code == libc::EOPNOTSUPP
    })
}

/// Open handles never block a rename or removal on Unix.
pub(crate) fn is_retryable_publish_error(_error: &io::Error) -> bool {
    false
}

pub(crate) fn sync_directory(dir: &Path) -> io::Result<()> {
    match File::open(dir).and_then(|directory| directory.sync_all()) {
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                code == libc::EINVAL
                    || code == libc::ENOTSUP
                    || code == libc::EOPNOTSUPP
                    || code == libc::EBADF
            }) =>
        {
            Ok(())
        }
        synced => synced,
    }
}

pub(crate) fn name_limit(dir: &Path) -> crate::NameLimit {
    let queried = c_path(dir).ok().map(|dir| {
        // SAFETY: `dir` is NUL-terminated and outlives the call.
        unsafe { libc::pathconf(dir.as_ptr(), libc::_PC_NAME_MAX) }
    });
    crate::NameLimit {
        max: queried
            .and_then(|max| usize::try_from(max).ok())
            .filter(|max| *max > 0)
            .unwrap_or(255),
        unit: crate::NameUnit::Bytes,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn quarantined(path: &Path) -> bool {
    let Ok(path) = c_path(path) else {
        return false;
    };
    // SAFETY: both strings are NUL-terminated and outlive the call; a null buffer queries the size.
    let size = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    size >= 0
}

fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Write end of the self-pipe the signal handler writes the signal number to; `-1` until set.
static WAKE: AtomicI32 = AtomicI32::new(-1);
static HANDLER: OnceLock<Box<dyn Fn(Termination) + Send + Sync>> = OnceLock::new();

pub(crate) fn termination_handler() -> Option<&'static (dyn Fn(Termination) + Send + Sync)> {
    HANDLER.get().map(|handler| &**handler)
}

/// Async-signal-safe: one `write` to the pipe, preserving `errno` for the interrupted code.
extern "C" fn on_signal(signal: libc::c_int) {
    let saved = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let fd = WAKE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = u8::try_from(signal).unwrap_or(u8::MAX);
        // SAFETY: `fd` stays open for the process's life and `byte` is on this stack.
        let _ = unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
    }
    // SAFETY: this thread's `errno` location is valid for the thread's life.
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
        libc::SIGHUP => ("SIGHUP", libc::SIGHUP),
        _ => ("SIGTERM", libc::SIGTERM),
    };
    Termination {
        name,
        exit_code: u8::try_from(128 + number).unwrap_or(u8::MAX),
        closing: false,
    }
}

pub(crate) fn on_termination(handler: Box<dyn Fn(Termination) + Send + Sync>) -> io::Result<()> {
    if HANDLER.set(handler).is_err() {
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
        // SAFETY: `fd` was just opened by `pipe`. Children must not inherit either end.
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
                    Ok(1) => {
                        if let Some(handler) = HANDLER.get() {
                            handler(termination(signal[0]));
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    _ => return,
                }
            }
        })?;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: an all-zero `sigaction` is valid; the fields are set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SA_RESETHAND lets a second signal of a kind end the process normally.
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
    let peak = kib("VmHWM:");
    ProcessMemory {
        resident_bytes: kib("VmRSS:"),
        peak_resident_bytes: peak,
        footprint_bytes: kib("RssAnon:")
            .zip(kib("RssShmem:"))
            .map(|(anonymous, shared)| anonymous.saturating_add(shared)),
        peak_footprint_bytes: peak,
    }
}

/// `proc_pid_rusage` for footprint, and `ru_maxrss`, which macOS counts in bytes.
#[cfg(target_os = "macos")]
pub(crate) fn process_memory() -> ProcessMemory {
    // SAFETY: an all-zero `rusage_info_v4` is valid; the call fills it.
    let mut usage: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` matches the requested flavor and outlives the call.
    let resident = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
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
        footprint_bytes: resident.then_some(usage.ri_phys_footprint),
        // The kernel updates the lifetime maximum lazily, so it can lag the current footprint.
        peak_footprint_bytes: resident.then_some(
            usage
                .ri_lifetime_max_phys_footprint
                .max(usage.ri_phys_footprint),
        ),
    }
}

/// Available memory is free plus inactive plus purgeable pages. macOS has no readable
/// per-process limit, so the limit is the physical total.
#[cfg(target_os = "macos")]
pub(crate) fn memory_capacity() -> crate::MemoryCapacity {
    unsafe extern "C" {
        // The `libc` binding is deprecated.
        fn mach_host_self() -> libc::mach_port_t;
    }
    let mut physical = 0_u64;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: the name is NUL-terminated and `hw.memsize` is a `u64`, as `size` says.
    let read = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            std::ptr::addr_of_mut!(physical).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    let physical = read.then_some(physical);
    // SAFETY: an all-zero `vm_statistics64` is valid; the call fills at most `count` integers.
    let mut statistics: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the statistics buffer holds `count` integers and outlives the call.
    let answered = unsafe {
        libc::host_statistics64(
            mach_host_self(),
            libc::HOST_VM_INFO64,
            std::ptr::addr_of_mut!(statistics).cast(),
            &raw mut count,
        )
    } == libc::KERN_SUCCESS;
    // SAFETY: `sysconf` reads a constant of the running system.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok();
    let available = answered.then_some(()).and(page).map(|page| {
        (u64::from(statistics.free_count)
            + u64::from(statistics.inactive_count)
            + u64::from(statistics.purgeable_count))
        .saturating_mul(page)
    });
    crate::MemoryCapacity {
        physical_bytes: physical,
        available_bytes: available,
        limit_bytes: physical,
        limit_source: "physical",
        used_in_limit_bytes: None,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub(crate) fn process_memory() -> ProcessMemory {
    ProcessMemory::default()
}

#[cfg(target_os = "macos")]
pub(crate) fn prefer_fast_cores() {
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    /// From `<sys/qos.h>`.
    const USER_INTERACTIVE: u32 = 0x21;
    // SAFETY: constant arguments; only the calling thread's scheduling class changes.
    let _ = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
}

pub(crate) fn process_cpu_time() -> Option<std::time::Duration> {
    // SAFETY: an all-zero `rusage` is valid; the call fills it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` outlives the call, which writes only it.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
        return None;
    }
    let time = |value: libc::timeval| {
        Some(
            std::time::Duration::from_secs(u64::try_from(value.tv_sec).ok()?)
                + std::time::Duration::from_micros(u64::try_from(value.tv_usec).ok()?),
        )
    };
    Some(time(usage.ru_utime)? + time(usage.ru_stime)?)
}

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
