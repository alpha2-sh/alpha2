use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Set in a child of these tests: the file its termination handler writes the
/// termination's name to.
const MARKER: &str = "ALPHA2_OS_TEST_MARKER";
/// Set in a child of these tests: it also watches its parent.
const WATCH_PARENT: &str = "ALPHA2_OS_TEST_WATCH_PARENT";
/// Set in a child of these tests: its handler returns at once and the child exits this many
/// seconds later, as a host does that cancels an export and waits for it to stop.
const HANDLER_SLEEP: &str = "ALPHA2_OS_TEST_HANDLER_SLEEP";

/// Held while a test starts a child process, as the lock tests require.
fn fork_turn() -> MutexGuard<'static, ()> {
    crate::tests::FORK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn marker(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("alpha2-os-{}-{name}.marker", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// This test binary running the ignored test `test` alone.
fn test_child(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--exact",
        test,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    command
}

/// Reads `child`'s standard output until a line says `ready`.
fn wait_ready(child: &mut Child) {
    let stdout = child.stdout.take().expect("piped stdout");
    let ready = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains("ready"));
    assert!(ready, "the child ended before it was ready");
}

/// What `path` holds once it appears, waiting at most `limit`.
fn contents_within(path: &Path, limit: Duration) -> Option<String> {
    let started = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return Some(text);
        }
        if started.elapsed() > limit {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Not a test of its own: a child that installs a termination handler writing
/// the termination's name to [`MARKER`] and exiting with its status (later, with
/// [`HANDLER_SLEEP`]), watches
/// its parent when [`WATCH_PARENT`] is set, says `ready`, and waits.
#[test]
#[ignore = "run by the tests of this module in a child process"]
fn termination_child() {
    let Some(marker) = std::env::var_os(MARKER) else {
        return;
    };
    let sleep = std::env::var(HANDLER_SLEEP)
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map(Duration::from_secs);
    crate::on_termination(move |termination| {
        let _ = std::fs::write(&marker, termination.name());
        let code = i32::from(termination.exit_code());
        match sleep {
            Some(sleep) => {
                std::thread::spawn(move || {
                    std::thread::sleep(sleep);
                    std::process::exit(code);
                });
            }
            None => std::process::exit(code),
        }
    })
    .expect("handler");
    if std::env::var_os(WATCH_PARENT).is_some() {
        crate::watch_parent().expect("watch the parent");
    }
    println!("ready");
    std::thread::sleep(Duration::from_secs(30));
    std::process::exit(2);
}

/// Not a test of its own: starts [`termination_child`] watching this process,
/// waits until it is ready, and exits without waiting for it.
#[test]
#[ignore = "run by a_process_whose_parent_exits_is_told in a child process"]
fn parent_child() {
    let Some(marker) = std::env::var_os(MARKER) else {
        return;
    };
    let mut grandchild = test_child("lifetime::tests::termination_child")
        .env(MARKER, marker)
        .env(WATCH_PARENT, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("grandchild");
    wait_ready(&mut grandchild);
}

/// A process watching its parent is told when the parent exits, through the
/// termination handler, as `PARENT_EXITED`.
#[test]
fn a_process_whose_parent_exits_is_told() {
    let _turn = fork_turn();
    let marker = marker("parent");
    let status = test_child("lifetime::tests::parent_child")
        .env(MARKER, &marker)
        .stdout(Stdio::null())
        .status()
        .expect("child");
    assert!(status.success(), "{status}");
    assert_eq!(
        contents_within(&marker, Duration::from_secs(5)).as_deref(),
        Some("PARENT_EXITED"),
        "the watching process was not told its parent exited"
    );
    let _ = std::fs::remove_file(&marker);
}

/// `SIGHUP` (a closed terminal) is a termination like `SIGTERM`, with status
/// 129.
#[cfg(unix)]
#[test]
fn a_hangup_is_a_termination() {
    let _turn = fork_turn();
    let marker = marker("hangup");
    let mut child = test_child("lifetime::tests::termination_child")
        .env(MARKER, &marker)
        .stdout(Stdio::piped())
        .spawn()
        .expect("child");
    wait_ready(&mut child);
    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    // SAFETY: `kill` takes any pid and signal and reports a bad one as an error.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGHUP) }, 0);
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(129), "{status}");
    assert_eq!(std::fs::read_to_string(&marker).expect("marker"), "SIGHUP");
    let _ = std::fs::remove_file(&marker);
}

/// Starts [`termination_child`] in a new process group on this process's
/// console (made now when there is none, as over SSH), sends it `breaks`
/// `CTRL_BREAK_EVENT`s, and returns its exit status, how long it took to end
/// after the first, and what its handler wrote.
#[cfg(windows)]
fn send_breaks(
    breaks: usize,
    handler_sleep: Option<u64>,
) -> (std::process::ExitStatus, Duration, Option<String>) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::{
        AllocConsole, CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent,
    };
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    // SAFETY: fails harmlessly when the process already has a console.
    unsafe { AllocConsole() };
    let marker = marker(&format!("break-{breaks}"));
    let mut command = test_child("lifetime::tests::termination_child");
    command
        .env(MARKER, &marker)
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .stdout(Stdio::piped());
    if let Some(seconds) = handler_sleep {
        command.env(HANDLER_SLEEP, seconds.to_string());
    }
    let mut child = command.spawn().expect("child");
    wait_ready(&mut child);
    let sent = Instant::now();
    for _ in 0..breaks {
        // SAFETY: the child's process group shares this console; the call only queues the event.
        let delivered = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
        assert_ne!(delivered, 0, "{}", std::io::Error::last_os_error());
        std::thread::sleep(Duration::from_millis(200));
    }
    let status = child.wait().expect("wait");
    let took = sent.elapsed();
    let written = std::fs::read_to_string(&marker).ok();
    let _ = std::fs::remove_file(&marker);
    (status, took, written)
}

/// A console break reaches the termination handler, which ends the process
/// with status 130.
#[cfg(windows)]
#[test]
fn a_console_break_reaches_the_termination_handler() {
    let _turn = fork_turn();
    let (status, _, written) = send_breaks(1, None);
    assert_eq!(status.code(), Some(130), "{status}");
    assert_eq!(written.as_deref(), Some("CTRL_BREAK_EVENT"));
}

/// A second break while the process still stops after the first ends it at
/// once, with status 130, without its exit handlers.
#[cfg(windows)]
#[test]
fn a_second_break_ends_the_process_at_once() {
    let _turn = fork_turn();
    let (status, took, _) = send_breaks(2, Some(10));
    assert_eq!(status.code(), Some(130), "{status}");
    assert!(
        took < Duration::from_secs(2),
        "ended {took:?} after the first break"
    );
}

/// Closing a kill-on-close job ends the processes in it.
#[cfg(windows)]
#[test]
fn a_closed_job_ends_its_processes() {
    let _turn = fork_turn();
    let marker = marker("job");
    let mut child = test_child("lifetime::tests::termination_child")
        .env(MARKER, &marker)
        .stdout(Stdio::piped())
        .spawn()
        .expect("child");
    wait_ready(&mut child);
    let job = crate::KillOnCloseJob::new().expect("job");
    job.assign(&child).expect("assign");
    drop(job);
    let started = Instant::now();
    let ended = loop {
        if let Some(status) = child.try_wait().expect("child status") {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(ended.is_some(), "the child outlived its job");
    assert_ne!(ended.and_then(|status| status.code()), Some(2));
}

/// After the call, a child this process starts no longer inherits its
/// standard handles unasked.
#[cfg(windows)]
#[test]
fn std_handles_are_not_inherited_after_the_call() {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    let _turn = fork_turn();
    crate::stop_std_handle_inheritance();
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: any standard handle id may be asked for; the handle is not closed here.
        let handle = unsafe { GetStdHandle(stream) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut flags = 0_u32;
        // SAFETY: `handle` is open and `flags` outlives the call, which writes only it.
        if unsafe { GetHandleInformation(handle, &raw mut flags) } == 0 {
            // A console pseudo-handle on an old system has no flags to read.
            continue;
        }
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0, "standard handle {stream}");
    }
}
