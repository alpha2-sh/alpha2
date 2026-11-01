//! Non-UTF-8 arguments and a closed stdout end the CLI with status 1 and one
//! diagnostic, never a panic.

use std::path::PathBuf;
use std::process::{Command, Stdio};

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

#[test]
fn a_closed_stdout_ends_the_command_with_status_1() {
    let corpus =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/h264-long-gop.mp4");
    // Close the reading end first so the first write fails however fast the command runs.
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_alpha2"))
        .arg("probe")
        .arg(&corpus)
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .expect("alpha2 runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("alpha2: standard output:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
