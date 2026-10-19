//! `alpha2-host probe-gpu-encode` on this machine's DX12 adapters: every
//! adapter yields a row, and every row and codec either encoded or names the
//! step it stopped at and why, within the probe's bound. The slice renders
//! on an adapter whose GPU-resident path is available, or says why none is.
#![cfg(all(target_os = "windows", feature = "hwencode"))]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// What the probe may take: every adapter opened, each codec's test encode
/// in a child bounded by the GPU wait timeout (30 seconds), and the slice.
const TIMEOUT: Duration = Duration::from_secs(300);

/// The steps a row or a codec can stop at.
const STEPS: [&str; 8] = [
    "open-device",
    "d3d12va-device",
    "encoder",
    "encode-support",
    "extent",
    "session-open",
    "test-encode",
    "first-packet",
];

fn refusal_is_named(refused: &Value, what: &str) {
    let step = refused["step"].as_str().unwrap_or_default();
    assert!(STEPS.contains(&step), "{what}: unknown step in {refused}");
    assert!(
        refused["reason"]
            .as_str()
            .is_some_and(|reason| !reason.trim().is_empty()),
        "{what}: no reason in {refused}"
    );
}

#[test]
fn every_adapter_encodes_or_names_the_step_that_refused_and_why() {
    let output = std::env::temp_dir().join(format!(
        "alpha2-probe-gpu-encode-{}.mp4",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output);
    let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2-host"))
        .args(["probe-gpu-encode", "256", "256"])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("alpha2-host runs");
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the probe runs") {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("probe-gpu-encode did not finish within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, stderr) = (
        stdout.join().expect("stdout"),
        stderr.join().expect("stderr"),
    );
    assert!(status.success(), "{status}: {stdout}{stderr}");
    let report: Value = serde_json::from_str(stdout.trim()).expect("one JSON report");
    assert_eq!(report["ok"], true, "{report}");
    let result = &report["result"];
    let adapters = result["adapters"].as_array().expect("adapters");
    assert!(!adapters.is_empty(), "no DX12 adapter in {report}");
    for adapter in adapters {
        let name = adapter["name"].as_str().expect("a name");
        assert!(
            adapter["vendorDevice"]
                .as_str()
                .is_some_and(|ids| ids.len() == 9),
            "{adapter}"
        );
        // One diagnostic line per adapter, naming it.
        assert!(
            stderr
                .lines()
                .any(|line| line.starts_with("alpha2-host probe-gpu-encode: [")
                    && line.contains(name)),
            "{name}: no line in {stderr}"
        );
        if !adapter["refused"].is_null() {
            refusal_is_named(&adapter["refused"], name);
            assert_eq!(adapter["codecs"], serde_json::json!([]), "{adapter}");
            continue;
        }
        let codecs = adapter["codecs"].as_array().expect("codecs");
        assert_eq!(codecs.len(), 3, "{name}: {adapter}");
        for codec in codecs {
            let what = format!("{name} {}", codec["videoCodec"]);
            if codec["refused"].is_null() {
                let encoded = &codec["result"];
                assert_eq!(codec["usable"], true, "{what}: {codec}");
                assert!(encoded["packets"].as_u64() > Some(0), "{what}: {codec}");
                assert_eq!(encoded["firstPacketDecodes"], true, "{what}: {codec}");
            } else {
                assert_eq!(codec["usable"], false, "{what}: {codec}");
                refusal_is_named(&codec["refused"], &what);
            }
        }
    }
    slice_rendered_or_says_why(adapters, &result["rendered"], &output);
    let _ = std::fs::remove_file(&output);
}

/// The slice rendered eight frames into `output` where an adapter's path is
/// usable, and otherwise says why.
fn slice_rendered_or_says_why(adapters: &[Value], rendered: &Value, output: &std::path::Path) {
    let usable = adapters.iter().any(|adapter| {
        adapter["codecs"]
            .as_array()
            .is_some_and(|codecs| codecs.iter().any(|codec| codec["usable"] == true))
    });
    if let Some(reason) = rendered["skipped"].as_str() {
        assert!(
            !usable,
            "an adapter is usable, and the slice skipped: {reason}"
        );
        assert_ne!(reason.trim(), "");
    } else {
        assert!(rendered["failed"].is_null(), "{rendered}");
        assert_eq!(rendered["frames"], 8, "{rendered}");
        assert!(output.is_file(), "{rendered}");
    }
}
