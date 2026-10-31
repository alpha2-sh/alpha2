//! `LockFileEx` on one unreachable byte, file identity by volume serial and
//! 128-bit file id, `MoveFileExW` that does not replace on verbatim paths, the
//! console control handler, and the working set.

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION,
    ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FileIdInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, MOVEFILE_WRITE_THROUGH, MoveFileExW,
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

/// A file's identity: its volume's 64-bit serial number and its 128-bit file
/// id. The 64-bit file index `BY_HANDLE_FILE_INFORMATION` carries is not
/// unique on `ReFS` (a Dev Drive), so it is read only from a file system that
/// refuses the `FileIdInfo` class.
pub(crate) fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    match file_id_info(file) {
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                code == ERROR_INVALID_PARAMETER.cast_signed()
                    || code == ERROR_INVALID_FUNCTION.cast_signed()
            }) =>
        {
            file_index(file)
        }
        identity => identity,
    }
}

/// `GetFileInformationByHandleEx(FileIdInfo)`.
pub(crate) fn file_id_info(file: &File) -> io::Result<(u64, [u8; 16])> {
    // SAFETY: an all-zero `FILE_ID_INFO` is valid, and is overwritten.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX);
    // SAFETY: the handle is open for the call, and `info` is a writable `FILE_ID_INFO` of `size`
    // bytes, the structure the class asks for.
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut info).cast(),
            size,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// The 32-bit volume serial and 64-bit file index of
/// `GetFileInformationByHandle`, widened to [`identity`]'s form.
fn file_index(file: &File) -> io::Result<(u64, [u8; 16])> {
    // SAFETY: an all-zero `BY_HANDLE_FILE_INFORMATION` is valid, and is overwritten.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the call and `info` is writable.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index.to_le_bytes()))
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

/// A file's identity: [`identity`].
pub(crate) type FileIdentity = (u64, [u8; 16]);

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    identity(file)
}

/// The identity of the file `path` names; `None` when nothing is there.
pub(crate) fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::os::windows::fs::OpenOptionsExt;
    // Backup semantics opens a directory too; no access is asked for, so a
    // file another program holds without sharing still opens.
    match std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => Ok(Some(identity(&file)?)),
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

/// `path` as a NUL-terminated verbatim wide path, which no Win32 call
/// limits to `MAX_PATH` or reinterprets: kept as it is when it already starts
/// with `\\?\`; otherwise made absolute (`GetFullPathNameW`, which resolves
/// `.` and `..` against the current directory and turns `/` into `\`), then
/// prefixed with `\\?\UNC\` for a `\\server\share` path, `\\?\` for any other.
/// A device path (`\\.\`) is kept as `GetFullPathNameW` returns it.
fn verbatim(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const DEVICE: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16];
    const UNC: [u16; 2] = [b'\\' as u16, b'\\' as u16];
    let given: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut wide = if given.starts_with(&VERBATIM) {
        given
    } else {
        let absolute: Vec<u16> = std::path::absolute(path)?
            .as_os_str()
            .encode_wide()
            .collect();
        if absolute.starts_with(&VERBATIM) || absolute.starts_with(&DEVICE) {
            absolute
        } else if let Some(share) = absolute.strip_prefix(&UNC) {
            "\\\\?\\UNC\\"
                .encode_utf16()
                .chain(share.iter().copied())
                .collect()
        } else {
            VERBATIM.iter().copied().chain(absolute).collect()
        }
    };
    wide.push(0);
    Ok(wide)
}

pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (verbatim(from)?, verbatim(to)?);
    // SAFETY: both are NUL-terminated wide paths that outlive the call. Without
    // `MOVEFILE_REPLACE_EXISTING` the move fails with `ERROR_ALREADY_EXISTS` (or
    // `ERROR_FILE_EXISTS`) when `to` exists, which `io::ErrorKind::AlreadyExists` covers.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether a rename failed because the file system does not take a
/// no-replace move, rather than for the files' sake.
pub(crate) fn flag_refused(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        code == ERROR_NOT_SUPPORTED.cast_signed() || code == ERROR_INVALID_FUNCTION.cast_signed()
    })
}

pub(crate) fn is_retryable_publish_error(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        code == ERROR_ACCESS_DENIED.cast_signed()
            || code == ERROR_SHARING_VIOLATION.cast_signed()
            || code == ERROR_LOCK_VIOLATION.cast_signed()
    })
}

/// NTFS, `ReFS` and exFAT all take 255 UTF-16 units.
pub(crate) fn name_limit(_dir: &Path) -> crate::NameLimit {
    crate::NameLimit {
        max: 255,
        unit: crate::NameUnit::Utf16,
    }
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
    // A second interrupt ends the process at once. Not through the default handler, whose
    // `ExitProcess` runs the DLLs' detach code, which can wait on a thread inside a driver.
    if !closing && INTERRUPTED.swap(true, Ordering::AcqRel) {
        crate::exit_without_handlers(130);
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

/// The handler [`on_termination`] installed, if any.
pub(crate) fn termination_handler() -> Option<&'static (dyn Fn(Termination) + Send + Sync)> {
    HANDLER.get().map(|handler| &**handler)
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

/// Opts the calling thread out of execution-speed power throttling (`EcoQoS`),
/// under which Windows may run it on efficiency cores at a lower clock; the
/// result is ignored.
pub(crate) fn prefer_fast_cores() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadInformation, THREAD_POWER_THROTTLING_CURRENT_VERSION,
        THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
        ThreadPowerThrottling,
    };
    // Controlled, and off: the thread is never throttled.
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: the pseudo-handle of the current thread needs no closing; `state` is the structure
    // this information class reads, of the size passed, and outlives the call.
    let _ = unsafe {
        SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw const state).cast(),
            u32::try_from(std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>()).unwrap_or(0),
        )
    };
}

/// Kernel plus user CPU time of the process, from `GetProcessTimes`.
pub(crate) fn process_cpu_time() -> Option<std::time::Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    // SAFETY: the pseudo-handle of the current process needs no closing; the four structures
    // outlive the call, which writes only them.
    let read = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    } != 0;
    // FILETIME durations count 100 ns intervals.
    let hundreds =
        |time: FILETIME| u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime);
    read.then(|| {
        std::time::Duration::from_nanos((hundreds(kernel) + hundreds(user)).saturating_mul(100))
    })
}

/// The unbiased interrupt time: 100 ns units since boot, not counting time
/// the system spent asleep or hibernating.
pub(crate) fn unbiased_interrupt_time() -> u64 {
    use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTimePrecise;
    let mut time = 0_u64;
    // SAFETY: `time` outlives the call, which writes only it.
    unsafe { QueryUnbiasedInterruptTimePrecise(&raw mut time) };
    time
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

/// The file version of the executable or DLL at `path`, from its version
/// resource, as `major.minor.build.revision`.
pub(crate) fn file_version(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated path; the handle argument may be null.
    let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0_u8; usize::try_from(size).ok()?];
    // SAFETY: `data` holds the `size` bytes the call writes.
    if unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let root: [u16; 2] = [u16::from(b'\\'), 0];
    let mut info: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut length = 0_u32;
    // SAFETY: `data` is the version resource just read; `info` points into it on success.
    if unsafe {
        VerQueryValueW(
            data.as_ptr().cast(),
            root.as_ptr(),
            &raw mut info,
            &raw mut length,
        )
    } == 0
        || info.is_null()
        || usize::try_from(length).ok()? < std::mem::size_of::<VS_FIXEDFILEINFO>()
    {
        return None;
    }
    // SAFETY: `info` points at a `VS_FIXEDFILEINFO` inside `data`, which is still alive; the
    // resource keeps it 4-byte aligned.
    let fixed = unsafe { info.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    Some(format!(
        "{}.{}.{}.{}",
        fixed.dwFileVersionMS >> 16,
        fixed.dwFileVersionMS & 0xffff,
        fixed.dwFileVersionLS >> 16,
        fixed.dwFileVersionLS & 0xffff
    ))
}
