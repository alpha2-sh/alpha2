//! A cargo test runner that stops a test binary stuck for `ALPHA2_TEST_WATCHDOG_SECS`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const LIMIT_VAR: &str = "ALPHA2_TEST_WATCHDOG_SECS";
const DEFAULT_LIMIT: Duration = Duration::from_secs(600);
// libtest names a test of a parallel run once it has run this long.
const LIBTEST_WARNS_AFTER: Duration = Duration::from_secs(60);
const DEBUGGER_LIMIT: Duration = Duration::from_secs(180);
const EXIT_POLL: Duration = Duration::from_secs(1);
const STDOUT_DRAIN_AFTER_EXIT: Duration = Duration::from_secs(5);
const USAGE_EXIT: i32 = 2;
const TIMED_OUT_EXIT: i32 = 124;
const UNKNOWN_TEST: &str = "<unknown>";

#[derive(Default)]
struct RunningTests {
    line: Vec<u8>,
    started: BTreeMap<String, Instant>,
    announced: Option<String>,
}

fn is_outcome(text: &str) -> bool {
    matches!(text, "ok" | "FAILED" | "ignored")
        || text.starts_with("ignored, ")
        || text.starts_with("bench:")
}

impl RunningTests {
    fn feed(&mut self, bytes: &[u8], now: Instant) {
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.complete_line(&String::from_utf8_lossy(&line), now);
            } else {
                self.line.push(byte);
                if self.line.starts_with(b"test ") && self.line.ends_with(b" ... ") {
                    self.announce_open_line(now);
                }
            }
        }
    }

    fn announce_open_line(&mut self, now: Instant) {
        let open = String::from_utf8_lossy(&self.line);
        let Some(name) = open
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... "))
            .filter(|name| !name.contains(' '))
        else {
            return;
        };
        let name = name.to_owned();
        if let Some(previous) = self.announced.replace(name.clone()) {
            self.started.remove(&previous);
        }
        self.started.entry(name).or_insert(now);
    }

    fn complete_line(&mut self, line: &str, now: Instant) {
        let line = line.trim_end_matches('\r');
        if line.starts_with("test result: ") {
            self.started.clear();
            self.announced = None;
        } else if is_outcome(line) {
            if let Some(name) = self.announced.take() {
                self.started.remove(&name);
            }
        } else if let Some((name, rest)) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.split_once(' '))
        {
            if rest.starts_with("has been running for over ") {
                let since = now.checked_sub(LIBTEST_WARNS_AFTER).unwrap_or(now);
                self.started.entry(name.to_owned()).or_insert(since);
            } else if rest.strip_prefix("... ").is_some_and(is_outcome) {
                self.started.remove(name);
                if self.announced.as_deref() == Some(name) {
                    self.announced = None;
                }
            }
        }
    }

    fn oldest_start(&self) -> Option<Instant> {
        self.started.values().min().copied()
    }

    fn names_oldest_first(&self) -> Vec<&str> {
        let mut running: Vec<(&Instant, &str)> = self
            .started
            .iter()
            .map(|(name, since)| (since, name.as_str()))
            .collect();
        running.sort();
        running.into_iter().map(|(_, name)| name).collect()
    }
}

fn limit_from(value: Option<&str>) -> Result<Option<Duration>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(Some(DEFAULT_LIMIT)),
        Some(text) => match text.parse::<u64>() {
            Ok(0) => Ok(None),
            Ok(seconds) => Ok(Some(Duration::from_secs(seconds))),
            Err(_) => Err(format!(
                "{LIMIT_VAR} must be whole seconds (0 turns the watchdog off), not {text:?}"
            )),
        },
    }
}

fn status_line(binary: &str, limit: Duration, running: &[&str]) -> String {
    let running = if running.is_empty() {
        UNKNOWN_TEST.to_owned()
    } else {
        running.join(", ")
    };
    format!(
        "test-watchdog: {binary} exceeded {}s; still running: {running}",
        limit.as_secs()
    )
}

enum Output {
    Bytes(Vec<u8>, Instant),
    Closed,
}

fn forward_stdout(mut stdout: impl Read, sender: &mpsc::Sender<Output>) {
    let mut buffer = [0_u8; 8192];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buffer[..read]).and_then(|()| out.flush());
                drop(out);
                if sender
                    .send(Output::Bytes(buffer[..read].to_vec(), Instant::now()))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    let _ = sender.send(Output::Closed);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn allow_debugger(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only calls prctl, which is async-signal-safe after fork.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
            Ok(())
        });
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn allow_debugger(_command: &mut Command) {}

fn debuggers(pid: u32) -> Vec<Command> {
    let pid = pid.to_string();
    let mut found = Vec::new();
    if cfg!(any(target_os = "linux", target_os = "android")) {
        let mut gdb = Command::new("gdb");
        gdb.args(["--nx", "--batch", "-p", &pid])
            .args(["-iex", "set debuginfod enabled off"])
            .args(["-ex", "thread apply all bt"]);
        found.push(gdb);
        let mut eu_stack = Command::new("eu-stack");
        eu_stack.args(["-p", &pid]);
        found.push(eu_stack);
    } else if cfg!(target_os = "macos") {
        let mut lldb = Command::new("lldb");
        lldb.args(["--batch", "-p", &pid, "-o", "thread backtrace all"]);
        found.push(lldb);
        let mut sample = Command::new("sample");
        sample.args([pid.as_str(), "1", "-file", "/dev/stdout"]);
        found.push(sample);
    } else if cfg!(windows) {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x64"
        };
        let mut paths = vec![OsString::from("cdb")];
        if let Some(kits) = std::env::var_os("ProgramFiles(x86)") {
            paths.push(
                Path::new(&kits)
                    .join("Windows Kits/10/Debuggers")
                    .join(arch)
                    .join("cdb.exe")
                    .into_os_string(),
            );
        }
        for path in paths {
            let mut cdb = Command::new(path);
            cdb.args(["-pv", "-p", &pid, "-c", "~*k; q"]);
            found.push(cdb);
        }
    }
    found
}

fn print_stacks(pid: u32) {
    for mut debugger in debuggers(pid) {
        let tool = debugger.get_program().to_string_lossy().into_owned();
        debugger
            .stdin(Stdio::null())
            .stdout(std::io::stderr())
            .stderr(std::io::stderr());
        let Ok(mut running) = debugger.spawn() else {
            continue;
        };
        eprintln!("test-watchdog: stacks of every thread from {tool}");
        let started = Instant::now();
        loop {
            match running.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if started.elapsed() >= DEBUGGER_LIMIT => {
                    let _ = running.kill();
                    let _ = running.wait();
                    eprintln!(
                        "test-watchdog: {tool} did not finish in {}s",
                        DEBUGGER_LIMIT.as_secs()
                    );
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
    eprintln!("test-watchdog: no debugger found to print the stacks");
}

fn exit_like(status: ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            // SAFETY: restoring the default action and signalling this process touch no memory.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::kill(libc::getpid(), signal);
            }
            std::process::exit(128 + signal);
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

fn drain_after_exit(receiver: &mpsc::Receiver<Output>, status: ExitStatus) -> ExitStatus {
    let drained_by = Instant::now() + STDOUT_DRAIN_AFTER_EXIT;
    while let Some(left) = drained_by.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(left) {
            Ok(Output::Bytes(..)) => {}
            Ok(Output::Closed) | Err(_) => break,
        }
    }
    status
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        eprintln!("usage: alpha2-test-watchdog <test binary> [arguments...]");
        std::process::exit(USAGE_EXIT);
    };
    let limit = match limit_from(std::env::var(LIMIT_VAR).ok().as_deref()) {
        Ok(limit) => limit,
        Err(error) => {
            eprintln!("test-watchdog: {error}");
            std::process::exit(USAGE_EXIT);
        }
    };
    let binary = Path::new(&program)
        .file_name()
        .unwrap_or(program.as_os_str())
        .to_string_lossy()
        .into_owned();
    let mut command = Command::new(&program);
    command.args(args).stdout(Stdio::piped());
    allow_debugger(&mut command);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {}: {error}", program.to_string_lossy()));
    let stdout = child.stdout.take().expect("a piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || forward_stdout(stdout, &sender));

    let mut tests = RunningTests::default();
    let mut last_output = Instant::now();
    let mut stdout_closed = false;
    loop {
        let now = Instant::now();
        let deadline = limit.map(|limit| {
            tests
                .oldest_start()
                .map_or(last_output, |oldest| oldest.min(last_output))
                + limit
        });
        if let (Some(limit), Some(deadline)) = (limit, deadline)
            && now >= deadline
        {
            if !tests.line.is_empty() {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(b"\n").and_then(|()| out.flush());
            }
            eprintln!(
                "{}",
                status_line(&binary, limit, &tests.names_oldest_first())
            );
            print_stacks(child.id());
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("test-watchdog: killed {binary}");
            std::process::exit(TIMED_OUT_EXIT);
        }
        let wait = deadline.map_or(EXIT_POLL, |deadline| {
            deadline.saturating_duration_since(now).min(EXIT_POLL)
        });
        if stdout_closed {
            if let Ok(Some(status)) = child.try_wait() {
                exit_like(status);
            }
            std::thread::sleep(wait);
            continue;
        }
        match receiver.recv_timeout(wait) {
            Ok(Output::Bytes(bytes, at)) => {
                tests.feed(&bytes, at);
                last_output = at;
            }
            Ok(Output::Closed) | Err(RecvTimeoutError::Disconnected) => stdout_closed = true,
            Err(RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    exit_like(drain_after_exit(&receiver, status));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [&str; 0] = [];

    fn fed(chunks: &[&str]) -> (RunningTests, Instant) {
        let start = Instant::now() + LIBTEST_WARNS_AFTER;
        let mut tests = RunningTests::default();
        for (index, chunk) in chunks.iter().enumerate() {
            tests.feed(
                chunk.as_bytes(),
                start + Duration::from_secs(u64::try_from(index).expect("few chunks")),
            );
        }
        (tests, start)
    }

    #[test]
    fn a_sequential_test_runs_from_its_open_line_until_its_outcome() {
        let (tests, start) = fed(&[
            "running 2 tests\ntest a::first ... ",
            "ok\ntest a::sec",
            "ond ... ",
        ]);
        assert_eq!(tests.names_oldest_first(), ["a::second"]);
        assert_eq!(tests.oldest_start(), Some(start + Duration::from_secs(2)));
        let (tests, _) = fed(&["test a::first ... ", "FAILED\n"]);
        assert_eq!(tests.names_oldest_first(), NONE);
    }

    #[test]
    fn a_parallel_test_runs_from_a_minute_before_its_warning() {
        let (tests, start) = fed(&[
            "test a::slow has been running for over 60 seconds\n",
            "test a::stuck has been running for over 60 seconds\ntest a::quick ... ",
            "ok\ntest a::slow ... ",
            "ok\n",
        ]);
        assert_eq!(tests.names_oldest_first(), ["a::stuck"]);
        assert_eq!(
            tests.oldest_start(),
            (start + Duration::from_secs(1)).checked_sub(LIBTEST_WARNS_AFTER)
        );
    }

    #[test]
    fn printed_output_of_a_running_test_does_not_end_it() {
        let (tests, _) = fed(&["test a::loud ... ", "progress 1\n", "ok then\n"]);
        assert_eq!(tests.names_oldest_first(), ["a::loud"]);
        for end in ["ok\n", "FAILED\n", "\ntest result: ok. 1 passed\n"] {
            let (tests, _) = fed(&["test a::loud ... ", "progress\n", end]);
            assert_eq!(tests.names_oldest_first(), NONE, "{end:?}");
        }
    }

    #[test]
    fn the_next_announced_test_ends_the_previous_one() {
        let (tests, _) = fed(&["test a::x ... ", "noise\n", "test a::y ... "]);
        assert_eq!(tests.names_oldest_first(), ["a::y"]);
    }

    #[test]
    fn ignored_tests_end_with_or_without_a_reason() {
        let (tests, _) = fed(&["test a::b ... ignored\ntest a::c ... ignored, needs a GPU\n"]);
        assert_eq!(tests.names_oldest_first(), NONE);
    }

    #[test]
    fn the_limit_defaults_turns_off_at_zero_and_refuses_other_text() {
        assert_eq!(limit_from(None), Ok(Some(DEFAULT_LIMIT)));
        assert_eq!(limit_from(Some(" 90 ")), Ok(Some(Duration::from_secs(90))));
        assert_eq!(limit_from(Some("0")), Ok(None));
        assert!(limit_from(Some("10m")).is_err_and(|error| error.contains(LIMIT_VAR)));
    }

    #[test]
    fn the_status_line_names_the_binary_limit_and_tests() {
        assert_eq!(
            status_line("render-0f", Duration::from_secs(600), &["a::b", "c::d"]),
            "test-watchdog: render-0f exceeded 600s; still running: a::b, c::d"
        );
        assert_eq!(
            status_line("render-0f", Duration::from_secs(5), &[]),
            "test-watchdog: render-0f exceeded 5s; still running: <unknown>"
        );
    }
}
