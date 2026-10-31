//! How many decoder sessions an export keeps open, and what running short of
//! them costs: the result says, and the frames are the same.

use super::*;

/// An export of the nine-clip grid, each clip its own cursor of the corpus
/// video, with the decoder session budget `budget` (the default when `None`),
/// to `output`.
fn grid_export(output: &Path, budget: Option<u64>) -> Request {
    let project: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-decoder-session-grid.json"
    ))
    .expect("grid project");
    let assets = (0..9)
        .map(|index| (format!("v{index}"), corpus("h264-bt709-copy.mp4")))
        .collect::<HashMap<_, _>>();
    let mut request = json!({
        "protocolVersion": 1, "operation": "export", "requestId": "grid",
        "project": project,
        "assets": assets,
        "startTicks": 0, "endTicks": alpha2_time::TICKS_PER_SECOND,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "videoQuality": "very-high",
        "output": output
    });
    if let Some(bytes) = budget {
        request["memory"] = json!({ "decoderSessionBytes": bytes });
    }
    parse_request(&request.to_string()).expect("request")
}

#[test]
fn an_export_opens_a_session_for_each_video_it_reads_at_once_and_short_of_them_writes_the_same_file()
 {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("decoder-sessions");
    let roomy = directory.join("roomy.mp4");
    let result = host
        .handle(grid_export(&roomy, None), &mut |_| Ok(()))
        .expect("export");
    let performance = &result["performance"];
    assert_eq!(performance["decoderSessionPeak"], 9, "{performance}");
    assert_eq!(performance["decoderSessions"], 9, "{performance}");
    assert_eq!(performance["decoderSessionLimit"], "workload");
    assert_eq!(performance["decoderSessionReopens"], 0);
    assert_eq!(result["decodeDiagnostics"], json!([]), "{result}");
    assert_eq!(
        result["memoryPolicy"]["decoderSessionBytes"]["source"],
        "default"
    );

    // A budget no session fits: one session, which every cursor takes in turn.
    let short = directory.join("short.mp4");
    let result = host
        .handle(grid_export(&short, Some(1)), &mut |_| Ok(()))
        .expect("export");
    let performance = &result["performance"];
    assert_eq!(performance["decoderSessions"], 1, "{performance}");
    assert_eq!(performance["decoderSessionLimit"], "memory");
    assert_eq!(performance["decodeConcurrency"], 1);
    let reopens = performance["decoderSessionReopens"]
        .as_u64()
        .expect("reopens");
    assert!(reopens > 0, "{performance}");
    let diagnostics = result["decodeDiagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["type"], "decoderSessionsShort");
    assert_eq!(diagnostics[0]["reopens"], reopens);
    assert_eq!(
        diagnostics[0]["message"],
        format!(
            "9 videos are read at once but 1 decoder sessions fit (memory); {reopens} decoders were reopened"
        )
    );
    assert_eq!(
        std::fs::read(&roomy).expect("roomy"),
        std::fs::read(&short).expect("short"),
        "the frames do not depend on the sessions"
    );
}

/// A decoder session whose estimate at the split's threads is more than the
/// session budget runs with fewer decoder threads, down to what fits, rather
/// than past the budget; the encoder keeps its share.
#[test]
fn a_session_too_large_for_the_budget_runs_with_fewer_threads() {
    use crate::config::decoder_session_bytes;

    let frame = 1920 * 1080 * 3 / 2;
    let mut threads = alpha2_native::CodecThreadAllocation {
        total_threads: 22,
        decode_concurrency: 2,
        decoder_threads: 8,
        hardware_decoder_threads: 2,
        encoder_threads: 6,
        allocated_threads: 22,
    };
    let budget = crate::memory::ByteBudget::exact(decoder_session_bytes(frame, 3));
    crate::export_decode::fit_session_threads(&mut threads, false, frame, budget);
    assert_eq!(threads.decoder_threads, 3);
    assert_eq!(threads.encoder_threads, 6);
    assert_eq!(threads.allocated_threads, 22 - 2 * 5);
    // A budget below one thread's session runs one thread; a scale below it is undone up to
    // the unscaled budget.
    crate::export_decode::fit_session_threads(
        &mut threads,
        true,
        frame,
        crate::memory::ByteBudget::exact(1),
    );
    assert_eq!(
        (threads.decoder_threads, threads.hardware_decoder_threads),
        (1, 1)
    );

    // Through an export: a budget of one session at two threads.
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("decoder-session-threads");
    let probed = alpha2_native::probe(corpus("h264-bt709-copy.mp4")).expect("probe");
    let bytes = crate::assets::VideoFrameSize::of(probed.selected_video_stream().expect("video"))
        .expect("size")
        .bytes;
    let result = host
        .handle(
            grid_export(
                &directory.join("fitted.mp4"),
                Some(decoder_session_bytes(bytes, 2)),
            ),
            &mut |_| Ok(()),
        )
        .expect("export");
    let performance = &result["performance"];
    assert!(
        performance["decoderThreads"]
            .as_i64()
            .is_some_and(|threads| (1..=2).contains(&threads)),
        "{performance}"
    );
    assert_eq!(performance["decoderSessions"], 1, "{performance}");
}

/// An export whose least working set is more than the machine has left is
/// refused before its output opens, naming both numbers, with the stage that
/// says so; the same export on a machine with room runs.
#[test]
fn an_export_larger_than_available_memory_is_refused_before_output() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("machine-memory");
    let output = directory.join("refused.mp4");
    let tight = alpha2_os::MemoryCapacity {
        physical_bytes: Some(64 << 30),
        available_bytes: Some(64 << 10),
        limit_bytes: Some(64 << 30),
        limit_source: "physical",
        used_in_limit_bytes: None,
    };
    crate::memory::injected::CAPACITY.with(|capacity| capacity.set(Some(tight)));
    let refused = host.handle(grid_export(&output, None), &mut |_| Ok(()));
    crate::memory::injected::CAPACITY.with(|capacity| capacity.set(None));
    let error = refused.expect_err("refused");
    assert_eq!(error.code, "RESOURCE_LIMIT", "{error:?}");
    assert!(
        error
            .message
            .contains("this machine can give the host 65536 bytes now"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("decoder sessions"),
        "{}",
        error.message
    );
    let failure = error.failure.as_ref().expect("a failure");
    assert_eq!(failure["code"], "RESOURCE_LIMIT", "{failure}");
    assert_eq!(failure["stage"], "machine-memory", "{failure}");
    assert!(!output.exists());
    assert!(!directory.join("refused.a2partial.mp4").exists());
    host.handle(grid_export(&directory.join("roomy.mp4"), None), &mut |_| {
        Ok(())
    })
    .expect("an export with room");
}
