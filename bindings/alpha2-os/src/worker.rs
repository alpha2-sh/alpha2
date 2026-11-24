//! A worker child process that runs foreign code for its parent over its standard streams, and the
//! parent's handle on its process tree.

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

/// Closes every descriptor above standard error but those of `keep` (Unix), so a descriptor
/// another library opened without close-on-exec does not live on in this process. Does nothing
/// elsewhere.
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

/// Ends this process without a core file (Unix) or an error dialog (Windows) when it crashes, so
/// a crash ends it at once.
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

/// Starts the child `command` spawns in a process tree of its own: its own process group (Unix),
/// so a terminal's interrupt reaches the parent alone and [`kill_process_tree`] ends what the
/// child started, and without a console (Windows), so console events do not reach it. On
/// Windows put the child in a [`crate::KillOnCloseJob`] for the same effect.
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

/// Kills `child` and, on Unix, every process of the group [`own_process_tree`] gave it. On
/// Windows dropping the child's job ends the processes the child started.
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
