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
