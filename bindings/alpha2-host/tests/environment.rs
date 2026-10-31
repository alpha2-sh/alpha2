//! Every report names the operating system's version beside the adapter's, as a start failure's
//! environment does.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn capabilities_name_the_os_version() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2-host"))
        .arg("serve")
        .env("ALPHA2_RENDERER", "auto")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the host starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"{\"protocolVersion\":1,\"requestId\":\"os\",\"operation\":\"capabilities\"}\n")
        .expect("the request is written");
    let output = child.wait_with_output().expect("the host ends");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let answer = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| line["requestId"] == "os")
        .unwrap_or_else(|| panic!("no answer: {stdout}"));
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(
        answer["result"]["osVersion"],
        alpha2_os::os_version(),
        "{answer}"
    );
    // The start line names it too.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("OS {}", alpha2_os::os_version())),
        "{stderr}"
    );
}
