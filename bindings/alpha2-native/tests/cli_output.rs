//! The `alpha2` CLI's own input and output: an argument that is not UTF-8 and
//! a standard output nobody reads end the command with status 1 and one
//! diagnostic, never a panic (status 101).

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// An argument that is not UTF-8 (a Linux file name in a legacy encoding) is
/// refused, naming it.
#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_refused_without_a_panic() {
    use std::os::unix::ffi::OsStrExt;
    let output = Command::new(env!("CARGO_BIN_EXE_alpha2"))
        .arg("probe")
        .arg(std::ffi::OsStr::from_bytes(b"caf\xff.mp4"))
        .output()
        .expect("alpha2 runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("is not valid UTF-8"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// A reader that closed the pipe before the report was written (as `| head`
/// does once it has its lines) fails the command with status 1 and one line
/// naming standard output.
#[test]
fn a_closed_stdout_ends_the_command_with_status_1() {
    let corpus =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/h264-long-gop.mp4");
    let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2"))
        .arg("probe")
        .arg(&corpus)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("alpha2 runs");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("alpha2 ends");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("alpha2: standard output:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
