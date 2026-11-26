//! A worker child process on its standard streams, and its parent's handle on its process tree.

use std::fs::File;
use std::io;
use std::process::{Child, Command};

/// Takes standard input for a protocol stream.
///
/// Returns a reader on the stream standard input was, which children do not inherit, and points
/// standard input at the null device, so a library that reads standard input reads nothing. On
/// Windows the C runtime's descriptor 0 follows too.
///
/// # Errors
/// When standard input is closed or cannot be duplicated.
pub fn claim_stdin() -> io::Result<File> {
    #[cfg(unix)]
    {
        crate::platform::claim_stdin()
    }
    #[cfg(windows)]
    {
        crate::platform::claim_stdin()
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// Closes every descriptor above standard error except those of `keep` (Unix only). Call it at
/// the start of the process, before anything opens a descriptor it keeps.
pub fn close_descriptors_except(keep: &[&File]) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let keep = keep.iter().map(|file| file.as_raw_fd()).collect::<Vec<_>>();
        crate::platform::close_descriptors_except(&keep);
    }
    #[cfg(not(unix))]
    let _ = keep;
}

/// Turns off core files (Unix) and crash dialogs (Windows) for this process.
pub fn disable_crash_dumps() {
    #[cfg(unix)]
    {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `none` is a valid `rlimit` read by the call; lowering a limit needs no rights.
        let _ = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const none) };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
        };
        // SAFETY: `SetErrorMode` takes flags only.
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };
    }
}

/// Makes this process the out-of-memory killer's first choice (Linux). Does nothing elsewhere.
pub fn prefer_out_of_memory_kill() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
    }
}

/// Starts the child `command` spawns in its own process group (Unix) or without a console
/// (Windows); on Windows also put the child in a [`crate::KillOnCloseJob`].
pub fn own_process_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = command;
}

/// Whether `child` has ended, without reaping it: until it is waited for, its id names it and the
/// process group it leads, so [`kill_process_tree`] still reaches what it started.
///
/// # Errors
/// When the operating system cannot tell.
pub fn child_exited(child: &mut Child) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let pid = libc::id_t::from(child.id());
        // SAFETY: an all-zero `siginfo_t` is a valid value for the call to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` outlives the call; `WNOWAIT` leaves the child waitable.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if waited != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `waitid` filled `info`; a zero pid means the child has not ended.
        Ok(unsafe { info.si_pid() } != 0)
    }
    #[cfg(not(unix))]
    {
        child.try_wait().map(|status| status.is_some())
    }
}

/// Kills `child` and, on Unix, every process of the group [`own_process_tree`] gave it; call it
/// before the child is reaped, while its id still names the group. On Windows dropping the
/// child's job ends the processes the child started.
///
/// # Errors
/// When the child cannot be killed.
pub fn kill_process_tree(child: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    if let Ok(group) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `kill` takes plain values; the group of a reaped child may be gone (ESRCH).
        let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    child.kill()
}
