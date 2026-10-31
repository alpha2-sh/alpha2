//! How long the process and the processes it starts live: an exit that runs
//! no exit handlers, the exit of the process that started this one as a
//! termination request, standard handles a child does not inherit unasked,
//! and a job that ends the children it holds when this process ends.

use std::io;

use crate::Termination;

/// Ends the process now with `code`, without running its exit handlers.
///
/// For a process with a thread stuck inside a hardware decoder, which is
/// inside the GPU driver: the driver's exit handlers wait for that thread, so
/// a normal exit never finishes. The caller flushes what it owes first;
/// nothing else is released.
///
/// On Windows the handlers are the DLLs' detach code. `std::process::exit`
/// and the CRT's `_exit` both end in `ExitProcess`, which stops the other
/// threads wherever they are and then runs that code, and a driver's detach
/// can wait on a lock a stopped thread held. `TerminateProcess` runs none.
pub fn exit_without_handlers(code: u8) -> ! {
    #[cfg(unix)]
    {
        // SAFETY: `_exit` accepts any status and never returns.
        unsafe { libc::_exit(libc::c_int::from(code)) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // SAFETY: `GetCurrentProcess` returns this process's pseudo-handle, which has every access
        // right, including the one to terminate it.
        if unsafe { TerminateProcess(GetCurrentProcess(), u32::from(code)) } != 0 {
            // Termination is under way and takes this thread with it.
            loop {
                std::thread::park();
            }
        }
        // Not expected for this process's own handle; the normal exit at least reports the code.
        std::process::exit(i32::from(code))
    }
    #[cfg(not(any(unix, windows)))]
    std::process::exit(i32::from(code))
}

/// Stops a process this one starts from inheriting its standard input,
/// output and error unasked (Windows; nothing elsewhere).
///
/// Windows hands a child every inheritable handle when the child's own
/// standard streams are redirected, so a child started with its output
/// discarded would still hold this process's pipes, and a client that waits
/// for the end of those pipes would wait for that child too. A child given
/// one of these streams on purpose (`Stdio::inherit`) still gets it: the
/// standard library duplicates it for the child.
pub fn stop_std_handle_inheritance() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: any standard handle id may be asked for; the handle is not closed here.
            let handle = unsafe { GetStdHandle(stream) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            // SAFETY: `handle` is open for the process's life; only its inherit flag changes. A
            // handle that refuses (a console pseudo-handle on old systems) is left as it is.
            let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// The termination [`watch_parent`] delivers: reported like `SIGTERM`.
const PARENT_EXITED: Termination = Termination::new("PARENT_EXITED", 143, false);

/// Calls the handler [`crate::on_termination`] installed, with a
/// `PARENT_EXITED` termination (exit status 143), when the process that
/// started this one exits; at once when it has exited already.
///
/// - Linux: a thread waits on a `pidfd_open` descriptor of the parent. A
///   kernel without it (before 5.3) gets `PR_SET_PDEATHSIG` with `SIGTERM`
///   instead, which arrives as a `SIGTERM` termination, and which a parent
///   that is one thread of a process also sends when that thread ends.
/// - macOS: a thread waits on a `kqueue` for the parent's `NOTE_EXIT`.
/// - Windows: a thread waits on a handle of the parent, found by a process
///   snapshot; a process with that id created after this one is another
///   process that reused the id, and means the parent has exited.
///
/// A parent that is process 1 (Unix) is not watched: this process was
/// started by `init`, or its parent had already exited.
///
/// # Errors
/// When no termination handler is installed, or the parent cannot be watched.
pub fn watch_parent() -> io::Result<()> {
    let handler = crate::platform::termination_handler().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no termination handler is installed",
        )
    })?;
    let Some(parent) = platform::Parent::open()? else {
        return Ok(());
    };
    std::thread::Builder::new()
        .name("alpha2-parent-watch".to_owned())
        .spawn(move || {
            if parent.wait() {
                handler(PARENT_EXITED);
            }
        })?;
    Ok(())
}

/// A job that ends every process assigned to it when the job is closed: when
/// this value is dropped, or when this process ends in any way. Windows only;
/// elsewhere it holds nothing and assigning does nothing.
///
/// For the children a process starts to try something that may crash or
/// hang (a test encode on a driver), which must not outlive it.
pub struct KillOnCloseJob {
    #[cfg(windows)]
    handle: platform::OwnedHandle,
}

impl KillOnCloseJob {
    /// Creates the job.
    ///
    /// # Errors
    /// When the operating system refuses the job.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            #[cfg(windows)]
            handle: platform::kill_on_close_job()?,
        })
    }

    /// Puts `child` in the job; call it right after the spawn.
    ///
    /// # Errors
    /// When the operating system refuses (the child has exited already).
    pub fn assign(&self, child: &std::process::Child) -> io::Result<()> {
        #[cfg(windows)]
        {
            platform::assign(&self.handle, child)
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(())
        }
    }
}

impl std::fmt::Debug for KillOnCloseJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KillOnCloseJob")
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;

    /// The parent's `pidfd`, or nothing to wait on (the parent is gone).
    pub(super) struct Parent(Option<std::fs::File>);

    impl Parent {
        pub(super) fn open() -> io::Result<Option<Self>> {
            use std::os::fd::FromRawFd;
            // SAFETY: `getppid` cannot fail.
            let parent = unsafe { libc::getppid() };
            if parent <= 1 {
                return Ok(None);
            }
            // SAFETY: `pidfd_open` takes a pid and flags and returns a new descriptor or -1.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, parent, 0) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ESRCH) => Ok(Some(Self(None))),
                    Some(libc::ENOSYS) => death_signal(parent),
                    _ => Err(error),
                };
            }
            let fd = libc::c_int::try_from(fd)
                .map_err(|_| io::Error::other("pidfd_open returned an impossible descriptor"))?;
            // SAFETY: `fd` was just opened by `pidfd_open` and is owned by nothing else.
            let pidfd = unsafe { std::fs::File::from_raw_fd(fd) };
            // The parent may have exited before the descriptor was opened, its id then naming
            // another process: this process was handed to a reaper, and its parent id says so.
            // SAFETY: `getppid` cannot fail.
            if unsafe { libc::getppid() } != parent {
                return Ok(Some(Self(None)));
            }
            Ok(Some(Self(Some(pidfd))))
        }

        /// Waits until the parent has exited; `true` once it has.
        pub(super) fn wait(&self) -> bool {
            use std::os::fd::AsRawFd;
            let Some(pidfd) = &self.0 else {
                return true;
            };
            let mut poll = libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                // SAFETY: `poll` is one valid `pollfd` that outlives the call.
                let ready = unsafe { libc::poll(&raw mut poll, 1, -1) };
                if ready > 0 {
                    return true;
                }
                if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return false;
                }
            }
        }
    }

    /// The kernel has no `pidfd_open`: asks it for `SIGTERM` when the parent
    /// exits, which the termination handler receives as `SIGTERM`.
    fn death_signal(parent: libc::pid_t) -> io::Result<Option<Parent>> {
        // SAFETY: `PR_SET_PDEATHSIG` takes a signal number and changes nothing else.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // A parent that exited before the call sends nothing.
        // SAFETY: `getppid` cannot fail.
        if unsafe { libc::getppid() } != parent {
            return Ok(Some(Parent(None)));
        }
        Ok(None)
    }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
mod platform {
    use std::io;

    /// A `kqueue` with the parent's `NOTE_EXIT` registered, or nothing to wait
    /// on (the parent is gone).
    pub(super) struct Parent(Option<std::fs::File>);

    impl Parent {
        pub(super) fn open() -> io::Result<Option<Self>> {
            use std::os::fd::{AsRawFd, FromRawFd};
            // SAFETY: `getppid` cannot fail.
            let parent = unsafe { libc::getppid() };
            if parent <= 1 {
                return Ok(None);
            }
            // SAFETY: `kqueue` takes nothing and returns a new descriptor or -1.
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just opened by `kqueue` and is owned by nothing else.
            let queue = unsafe { std::fs::File::from_raw_fd(fd) };
            // SAFETY: an all-zero `kevent` is valid; the fields that matter are set below.
            let mut change: libc::kevent = unsafe { std::mem::zeroed() };
            change.ident = usize::try_from(parent).unwrap_or(0);
            change.filter = libc::EVFILT_PROC;
            change.flags = libc::EV_ADD | libc::EV_ONESHOT;
            change.fflags = libc::NOTE_EXIT;
            // SAFETY: `change` is one valid `kevent` that outlives the call; no events are read.
            let registered = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    &raw const change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if registered < 0 {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(Some(Self(None)))
                } else {
                    Err(error)
                };
            }
            // The parent may have exited before it was registered, its id then naming another
            // process.
            // SAFETY: `getppid` cannot fail.
            if unsafe { libc::getppid() } != parent {
                return Ok(Some(Self(None)));
            }
            Ok(Some(Self(Some(queue))))
        }

        /// Waits until the parent has exited; `true` once it has.
        pub(super) fn wait(&self) -> bool {
            use std::os::fd::AsRawFd;
            let Some(queue) = &self.0 else {
                return true;
            };
            loop {
                // SAFETY: an all-zero `kevent` is valid; the call fills it.
                let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                // SAFETY: `event` has room for the one event asked for and outlives the call.
                let ready = unsafe {
                    libc::kevent(
                        queue.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        &raw mut event,
                        1,
                        std::ptr::null(),
                    )
                };
                if ready > 0 {
                    return true;
                }
                if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return false;
                }
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, INFINITE, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    /// A handle this process owns and closes when dropped.
    pub(super) struct OwnedHandle(HANDLE);

    // SAFETY: a kernel handle is a process-wide value; any thread may wait on or close it.
    unsafe impl Send for OwnedHandle {}
    // SAFETY: the calls made through a shared reference (waits, job assignment) are thread-safe.
    unsafe impl Sync for OwnedHandle {}

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle is open and owned by this value alone.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A handle of the parent, or nothing to wait on (the parent is gone).
    pub(super) struct Parent(Option<OwnedHandle>);

    impl Parent {
        pub(super) fn open() -> io::Result<Option<Self>> {
            let Some(parent) = parent_id()? else {
                return Ok(None);
            };
            // SAFETY: `OpenProcess` takes an access mask, an inherit flag and an id, and returns a
            // new handle or null.
            let handle = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    parent,
                )
            };
            if handle.is_null() {
                let error = io::Error::last_os_error();
                // No process has that id any more.
                return if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
                    Ok(Some(Self(None)))
                } else {
                    Err(error)
                };
            }
            let handle = OwnedHandle(handle);
            // SAFETY: the pseudo-handle of the current process needs no closing.
            let ours = created(unsafe { GetCurrentProcess() })?;
            // A process created after this one cannot have started it: the parent exited and its
            // id was reused.
            if created(handle.0)? > ours {
                return Ok(Some(Self(None)));
            }
            Ok(Some(Self(Some(handle))))
        }

        /// Waits until the parent has exited; `true` once it has.
        pub(super) fn wait(&self) -> bool {
            let Some(handle) = &self.0 else {
                return true;
            };
            // SAFETY: the handle is open, with `SYNCHRONIZE` access, for the call.
            unsafe { WaitForSingleObject(handle.0, INFINITE) == WAIT_OBJECT_0 }
        }
    }

    /// The id of the process that created this one, from a process snapshot.
    fn parent_id() -> io::Result<Option<u32>> {
        // SAFETY: a process snapshot takes no process id.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = OwnedHandle(snapshot);
        // SAFETY: the current process id cannot fail.
        let ours = unsafe { GetCurrentProcessId() };
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
            ..PROCESSENTRY32W::default()
        };
        // SAFETY: `entry` is a `PROCESSENTRY32W` with its size set, outliving the call.
        let mut more = unsafe { Process32FirstW(snapshot.0, &raw mut entry) } != 0;
        while more {
            if entry.th32ProcessID == ours {
                return Ok(Some(entry.th32ParentProcessID).filter(|parent| *parent != 0));
            }
            // SAFETY: as for the first entry.
            more = unsafe { Process32NextW(snapshot.0, &raw mut entry) } != 0;
        }
        Ok(None)
    }

    /// When `process` was created, in 100 ns intervals since 1601.
    fn created(process: HANDLE) -> io::Result<u64> {
        let zero = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
        // SAFETY: `process` is open with query access for the call; the four structures outlive
        // it, which writes only them.
        let read = unsafe {
            GetProcessTimes(
                process,
                &raw mut created,
                &raw mut exited,
                &raw mut kernel,
                &raw mut user,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
    }

    pub(super) fn kill_on_close_job() -> io::Result<OwnedHandle> {
        // SAFETY: an unnamed job with default security; the call returns a new handle or null.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle(job);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the structure this information class reads, of the size passed, and
        // outlives the call.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .unwrap_or(0),
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub(super) fn assign(job: &OwnedHandle, child: &std::process::Child) -> io::Result<()> {
        // SAFETY: both handles are open for the call; `child` keeps its process handle open.
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    windows
)))]
mod platform {
    use std::io;

    /// No parent watch on this platform.
    pub(super) struct Parent;

    impl Parent {
        pub(super) fn open() -> io::Result<Option<Self>> {
            Ok(None)
        }

        pub(super) fn wait(&self) -> bool {
            false
        }
    }
}

#[cfg(test)]
#[path = "lifetime_tests.rs"]
mod tests;
