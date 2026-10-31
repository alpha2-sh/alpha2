//! `alpha2-host probe-gpu-encode` on this machine's DX12 adapters: every
//! adapter yields a row, and every row and codec either encoded or names the
//! step it stopped at and why, within the probe's bound. The slice renders
//! on an adapter whose GPU-resident path is available, or says why none is.
//! On macOS, `VideoToolbox` on the Metal device: each probed size encodes and
//! renders its slice, or the probe fails naming what refused.
#![cfg(all(any(target_os = "windows", target_os = "macos"), feature = "hwencode"))]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// What the probe may take: every adapter opened, each codec's test encode
/// in a child bounded by the GPU wait timeout (30 seconds), and the slice.
const TIMEOUT: Duration = Duration::from_secs(300);

/// Runs `alpha2-host` with `args` to its end, within [`TIMEOUT`], and returns
/// its status, standard output and standard error.
fn run_host(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2-host"))
        .args(args)
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
            panic!("alpha2-host {args:?} did not finish within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (
        status,
        stdout.join().expect("stdout"),
        stderr.join().expect("stderr"),
    )
}

/// The steps a row or a codec can stop at.
#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
#[test]
fn every_adapter_encodes_or_names_the_step_that_refused_and_why() {
    let output = std::env::temp_dir().join(format!(
        "alpha2-probe-gpu-encode-{}.mp4",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output);
    let output_arg = output.to_str().expect("UTF-8 path");
    let (status, stdout, stderr) = run_host(&["probe-gpu-encode", "256", "256", output_arg]);
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
#[cfg(target_os = "windows")]
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

/// Each size either encodes one `VideoToolbox` frame from the Metal device's
/// planes and renders the eight-frame slice, or fails naming what refused:
/// 256x256 is within every Apple encoder's range, 7680x4320 beyond the
/// largest the host offers.
#[cfg(target_os = "macos")]
#[test]
fn every_probed_size_encodes_and_renders_or_names_what_refused() {
    let mut encoded = 0;
    for (width, height) in [(256_u32, 256_u32), (1_920, 1_080), (7_680, 4_320)] {
        let output = std::env::temp_dir().join(format!(
            "alpha2-probe-gpu-encode-{width}x{height}-{}.mp4",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&output);
        let (status, stdout, stderr) = run_host(&[
            "probe-gpu-encode",
            &width.to_string(),
            &height.to_string(),
            output.to_str().expect("UTF-8 path"),
        ]);
        let size = format!("{width}x{height}");
        if !status.success() {
            // The failing step names itself in the host's one diagnostic line.
            let reason = stderr
                .lines()
                .find_map(|line| line.strip_prefix("alpha2-host: "))
                .unwrap_or_else(|| panic!("{size}: {status} without a reason: {stderr}"));
            assert!(!reason.trim().is_empty(), "{size}: {stderr}");
            assert!(stdout.trim().is_empty(), "{size}: {stdout}");
            eprintln!("{size}: refused: {reason}");
            continue;
        }
        let report: Value = serde_json::from_str(stdout.trim()).expect("one JSON report");
        assert_eq!(report["ok"], true, "{size}: {report}");
        let result = &report["result"];
        assert_eq!(result["encoder"], "h264_videotoolbox", "{size}: {report}");
        assert!(result["packets"].as_u64() > Some(0), "{size}: {report}");
        // The luma plane covers the picture; a pool may pad it.
        let luma = &result["planeSizes"][0];
        assert!(
            luma[0].as_u64() >= Some(u64::from(width))
                && luma[1].as_u64() >= Some(u64::from(height)),
            "{size}: {report}"
        );
        assert_eq!(result["rendered"]["frames"], 8, "{size}: {report}");
        assert!(output.is_file(), "{size}: no slice at {}", output.display());
        let _ = std::fs::remove_file(&output);
        encoded += 1;
    }
    if encoded == 0 {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VideotoolboxEncode,
            "VideoToolbox encoded no probed size on this Mac"
        );
    }
}
