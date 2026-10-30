//! The host binary cargo builds for the tests exits cleanly: it is the one cargo copies to
//! `target/<profile>/alpha2-host`, so scripts that run that path run it.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn serve_capabilities_with_renderer_auto_exits_cleanly() {
    for run in 0..5 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2-host"))
            .arg("serve")
            .env("ALPHA2_RENDERER", "auto")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the host starts");
        // One request, then the end of input: the host answers and exits.
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(
                b"{\"protocolVersion\":1,\"requestId\":\"exit\",\"operation\":\"capabilities\"}\n",
            )
            .expect("the request is written");
        let output = child.wait_with_output().expect("the host ends");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "run {run}: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let answer = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|line| line["requestId"] == "exit")
            .unwrap_or_else(|| panic!("run {run}: no complete answer: {stdout}"));
        assert_eq!(answer["ok"], true, "run {run}: {answer}");
    }
}

/// A shader compiler the host cannot load refuses the host under every renderer policy, naming
/// the file: it never renders on another backend or on the CPU instead.
#[cfg(windows)]
#[test]
fn a_missing_shader_compiler_refuses_to_start() {
    let missing = std::env::temp_dir()
        .join("alpha2-no-dxc")
        .join("dxcompiler.dll");
    for renderer in ["gpu", "auto"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2-host"))
            .arg("serve")
            .env("ALPHA2_RENDERER", renderer)
            .env("ALPHA2_DXC_PATH", &missing)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the host starts");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(
                b"{\"protocolVersion\":1,\"requestId\":\"dxc\",\"operation\":\"capabilities\"}\n",
            )
            .expect("the request is written");
        let output = child.wait_with_output().expect("the host ends");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let answer = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            // A host that cannot start answers with its one fatal line.
            .find(|line| line["type"] == "fatal" || line["requestId"] == "dxc")
            .unwrap_or_else(|| panic!("{renderer}: no answer: {stdout}"));
        assert_ne!(answer["ok"], true, "{renderer}: {answer}");
        assert_eq!(
            answer["error"]["code"], "GPU_INIT_FAILED",
            "{renderer}: {answer}"
        );
        assert_eq!(
            answer["error"]["details"]["cause"], "shader-compiler",
            "{answer}"
        );
        assert!(
            answer["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("dxcompiler.dll")),
            "{renderer}: {answer}"
        );
        assert!(
            !stdout.contains("\"backend\":\"Vulkan\""),
            "{renderer}: {stdout}"
        );
    }
}
