//! `LockFileEx` on one unreachable byte, file identity by volume and file
//! index, `MoveFileExW` that does not replace, the console control handler,
//! and the working set.

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, MOVEFILE_WRITE_THROUGH,
    MoveFileExW,
};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::core::BOOL;

use crate::{ProcessMemory, Termination};

/// The byte the lock covers: past any offset a signed 64-bit file position,
/// `FFmpeg`'s included, can write.
const LOCK_OFFSET: u64 = i64::MAX as u64;

pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: an all-zero `OVERLAPPED` is valid; only the offset is set.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = (LOCK_OFFSET & 0xFFFF_FFFF) as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (LOCK_OFFSET >> 32) as u32;
    // SAFETY: the handle is open for the call and `overlapped` outlives it; with
    // `LOCKFILE_FAIL_IMMEDIATELY` on a synchronous handle the call completes before it returns.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION.cast_signed()) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn identity(file: &File) -> io::Result<(u32, u32, u32)> {
    // SAFETY: an all-zero `BY_HANDLE_FILE_INFORMATION` is valid, and is overwritten.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the call and `info` is writable.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

pub(crate) fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    let named = match File::open(path) {
        Ok(named) => named,
        // A file being deleted refuses new opens until its last handle closes.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    Ok(identity(file)? == identity(&named)?)
}

pub(crate) fn same_file_at(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    // Backup semantics opens a directory too; no access is asked for, so a
    // file another program holds without sharing still opens.
    let open = |path: &Path| match std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => Ok(Some(identity(&file)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    Ok(match (open(a)?, open(b)?) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    })
}

pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both are NUL-terminated wide paths that outlive the call. Without
    // `MOVEFILE_REPLACE_EXISTING` the move fails with `ERROR_ALREADY_EXISTS` when `to` exists.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

static HANDLER: OnceLock<Box<dyn Fn(Termination) + Send + Sync>> = OnceLock::new();
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Runs on a thread the system starts for the event.
unsafe extern "system" fn on_console_event(event: u32) -> BOOL {
    let (name, closing) = match event {
        CTRL_C_EVENT => ("CTRL_C_EVENT", false),
        CTRL_BREAK_EVENT => ("CTRL_BREAK_EVENT", false),
        CTRL_CLOSE_EVENT => ("CTRL_CLOSE_EVENT", true),
        CTRL_LOGOFF_EVENT => ("CTRL_LOGOFF_EVENT", true),
        CTRL_SHUTDOWN_EVENT => ("CTRL_SHUTDOWN_EVENT", true),
        _ => return 0,
    };
    // A second interrupt is left to the default handler, which ends the process.
    if !closing && INTERRUPTED.swap(true, Ordering::AcqRel) {
        return 0;
    }
    if let Some(handler) = HANDLER.get() {
        handler(Termination {
            name,
            exit_code: if closing { 143 } else { 130 },
            closing,
        });
    }
    1
}

pub(crate) fn on_termination(handler: Box<dyn Fn(Termination) + Send + Sync>) -> io::Result<()> {
    if HANDLER.set(handler).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a termination handler is already installed",
        ));
    }
    // SAFETY: the routine has the signature the system calls and lives for the process.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The working set and its peak, from `K32GetProcessMemoryInfo`.
pub(crate) fn process_memory() -> ProcessMemory {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: an all-zero `PROCESS_MEMORY_COUNTERS` is valid; the call fills it.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(u32::MAX);
    // SAFETY: the pseudo-handle of the current process needs no closing; `counters` is `size` bytes
    // and outlives the call.
    let read =
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, size) } != 0;
    let bytes = |value: usize| read.then(|| u64::try_from(value).ok()).flatten();
    ProcessMemory {
        resident_bytes: bytes(counters.WorkingSetSize),
        peak_resident_bytes: bytes(counters.PeakWorkingSetSize),
    }
}

/// CPU cycles the calling thread has run.
pub(crate) fn thread_cpu_work() -> Option<u64> {
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let mut cycles = 0_u64;
    // SAFETY: the pseudo-handle of the current thread needs no closing; `cycles` outlives the
    // call, which writes only it.
    let read = unsafe { QueryThreadCycleTime(GetCurrentThread(), &raw mut cycles) } != 0;
    read.then_some(cycles)
}
