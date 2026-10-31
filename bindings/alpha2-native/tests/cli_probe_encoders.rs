//! `alpha2 probe-encoders`: every D3D12VA mode is opened in a child on a named adapter or refused
//! with a reason within the bound; on macOS, every `h264_videotoolbox` mode at each probed size.
#![cfg(all(any(target_os = "windows", target_os = "macos"), feature = "hwencode"))]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Every encoder opened once per mode, with each D3D12VA open in a 30-second-bounded child.
const TIMEOUT: Duration = Duration::from_secs(300);

fn probe_encoders(width: u32, height: u32) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2"))
        .args(["probe-encoders", &width.to_string(), &height.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("alpha2 runs");
    let mut pipe = child.stdout.take().expect("stdout");
    let reader = std::thread::spawn(move || {
        let mut stdout = String::new();
        pipe.read_to_string(&mut stdout).expect("UTF-8 report");
        stdout
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the probe runs") {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("probe-encoders did not finish within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = reader.join().expect("the report");
    assert!(status.success(), "{status}: {stdout}");
    stdout
}

#[cfg(target_os = "windows")]
#[test]
fn every_d3d12va_row_is_opened_on_an_adapter_or_refused_with_a_reason() {
    let stdout = probe_encoders(256, 256);
    let mut encoders = 0;
    for block in stdout.split("\n\n") {
        let Some(header) = block.lines().next() else {
            continue;
        };
        let Some((_, rest)) = header.split_once(':') else {
            continue;
        };
        let Some(encoder) = rest
            .split(' ')
            .next()
            .filter(|name| name.ends_with("_d3d12va"))
        else {
            continue;
        };
        encoders += 1;
        let rows: Vec<_> = block
            .lines()
            .filter_map(|line| line.trim().strip_prefix("rate control "))
            .collect();
        assert_eq!(rows.len(), 3, "{encoder}: {block}");
        for row in rows {
            let (mode, outcome) = row.split_once(": ").expect("mode: outcome");
            if let Some(opened) = outcome.strip_prefix("opened (") {
                let (packets, on) = opened.split_once(" packets) with [").expect("packets");
                assert!(packets.parse::<u32>().expect("a count") > 0, "{row}");
                assert!(on.contains("] on DX12 adapter "), "{encoder} {mode}: {row}");
            } else {
                let (_, reason) = outcome
                    .split_once("]: ")
                    .unwrap_or_else(|| panic!("{encoder} {mode}: no reason in {row}"));
                assert!(
                    outcome.starts_with("refused with [") && reason.contains("DX12 adapter"),
                    "{encoder} {mode}: {row}"
                );
            }
        }
    }
    // `FFmpeg` 8.1 registers all three; 8.0 has only `hevc_d3d12va`.
    assert!(encoders >= 1, "no D3D12VA encoder in {stdout}");
}

#[cfg(target_os = "macos")]
#[test]
fn every_videotoolbox_row_is_opened_or_refused_with_a_reason_at_each_size() {
    let mut opened = 0;
    for (width, height) in [(256_u32, 256_u32), (7_680, 4_320)] {
        let stdout = probe_encoders(width, height);
        let Some(block) = stdout
            .split("\n\n")
            .find(|block| block.starts_with("h264:h264_videotoolbox "))
        else {
            alpha2_test_support::skip!(
                alpha2_test_support::Capability::VideotoolboxEncode,
                "this FFmpeg runtime has no h264_videotoolbox"
            );
            return;
        };
        let rows: Vec<_> = block
            .lines()
            .filter_map(|line| line.trim().strip_prefix("rate control "))
            .collect();
        assert!(!rows.is_empty(), "{width}x{height}: {block}");
        for row in rows {
            let (mode, outcome) = row.split_once(": ").expect("mode: outcome");
            if let Some(packets) = outcome.strip_prefix("opened (") {
                let (packets, _) = packets.split_once(" packets)").expect("packets");
                assert!(packets.parse::<u32>().expect("a count") > 0, "{row}");
                opened += 1;
            } else {
                let (_, reason) = outcome
                    .split_once("]: ")
                    .unwrap_or_else(|| panic!("{width}x{height} {mode}: no reason in {row}"));
                assert!(
                    outcome.starts_with("refused with [") && !reason.trim().is_empty(),
                    "{width}x{height} {mode}: {row}"
                );
            }
        }
    }
    if opened == 0 {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VideotoolboxEncode,
            "h264_videotoolbox opened no mode at any probed size on this Mac"
        );
    }
}
