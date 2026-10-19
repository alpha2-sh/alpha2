//! `alpha2 probe-encoders` on this machine's DX12 adapters: every mode of
//! every D3D12VA encoder is opened on a named adapter, in a child of the
//! probe, or refused with a reason, and the report finishes within its
//! bound.
#![cfg(all(target_os = "windows", feature = "hwencode"))]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the whole report may take: every encoder of the runtime opened once
/// per mode, and each D3D12VA open in a child bounded by 30 seconds.
const TIMEOUT: Duration = Duration::from_secs(300);

#[test]
fn every_d3d12va_row_is_opened_on_an_adapter_or_refused_with_a_reason() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_alpha2"))
        .args(["probe-encoders", "256", "256"])
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
    // Every FFmpeg 8.1 runtime registers the three; 8.0 has `hevc_d3d12va` alone.
    assert!(encoders >= 1, "no D3D12VA encoder in {stdout}");
}
