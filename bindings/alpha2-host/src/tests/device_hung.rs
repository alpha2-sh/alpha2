//! A device lost after the export's own long submission is reported hung,
//! one lost after short ones stays lost. No test may stop a GPU, so the loss
//! is injected at a frame's readback (`ALPHA2_EXPORT_LOSE_DEVICE_AT_FRAME`)
//! after its submission ran, and the watchdog is shortened
//! (`ALPHA2_GPU_WATCHDOG_MS`) so the frame's submissions cross the line. Both
//! are read from the environment, so each export runs in a child process.

use super::*;

/// The child half of the tests below: a two-effect 1080p export whose frame 1
/// is lost, answered on stdout as the protocol answers it.
#[test]
#[ignore = "run by the device-hung tests with ALPHA2_EXPORT_LOSE_DEVICE_AT_FRAME set"]
fn export_that_loses_its_device() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("device-hung");
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let mut project = still_project([0.2, 0.3, 0.4], "bypass");
    project["settings"]["width"] = json!(1920);
    project["settings"]["height"] = json!(1080);
    let param = |name: &str, value: f64| json!({name: {"mode": "constant", "value": {"type": "float", "value": value}}});
    project["compositions"] = json!([{"id": "heavy", "output": "grain", "nodes": [
        {"id": "clip", "op": {"type": "clip"}},
        {"id": "blur", "op": {"type": "effect", "input": "clip", "effect": {
            "id": "alpha2.effect.gaussian-blur", "version": 1,
            "params": param("radius", 0.08)}}},
        {"id": "grain", "op": {"type": "effect", "input": "blur", "effect": {
            "id": "alpha2.effect.film-grain", "version": 1,
            "params": param("amount", 0.2)}}}]}]);
    project["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({"id": "heavy"});
    let request = parse_request(
        &json!({
            "protocolVersion": 1,
            "operation": "export",
            "requestId": "hung",
            "project": project,
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png")},
            "startTicks": 0,
            "endTicks": alpha2_time::TICKS_PER_SECOND / 6,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            // A readback-path codec: the loss is injected where a frame is read back.
            "videoCodec": "mpeg4-part2",
            "output": directory.join("hung.mp4"),
        })
        .to_string(),
    )
    .expect("request");
    let answer = match host.handle(request, &mut |_| Ok(())) {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    println!("{answer}");
}

/// Runs [`export_that_loses_its_device`] in a child with `watchdog_ms`, when
/// given, and returns its answer.
fn lost_export(watchdog_ms: Option<&str>) -> Value {
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
    child
        .args([
            "--exact",
            "tests::device_hung::export_that_loses_its_device",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ALPHA2_EXPORT_LOSE_DEVICE_AT_FRAME", "1")
        .env_remove("ALPHA2_EXPORT_FAIL_ONCE_MARKER")
        .env_remove("ALPHA2_GPU_WATCHDOG_MS")
        .stdin(std::process::Stdio::null());
    if let Some(watchdog_ms) = watchdog_ms {
        child.env("ALPHA2_GPU_WATCHDOG_MS", watchdog_ms);
    }
    let output = child.output().expect("child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 passed"), "{stdout}");
    stdout
        .lines()
        .find_map(|line| line.find("{\"").map(|at| &line[at..]))
        .map_or_else(
            || panic!("no answer in {stdout}"),
            |line| serde_json::from_str(line).expect("JSON"),
        )
}

#[test]
fn an_injected_loss_after_a_heavy_frame_is_device_hung() {
    // A 1 ms watchdog puts the line at 0 ms. A longer one would not do: a heavy frame is cut into
    // submissions of an eighth of the watchdog, so on a fast GPU none of them reaches half of it.
    let answer = lost_export(Some("1"));
    let error = &answer["error"];
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(error["code"], "GPU_DEVICE_HUNG", "{answer}");
    assert_eq!(error["failure"]["code"], "DEVICE_HUNG", "{answer}");
    assert_eq!(error["failure"]["resource"], "gpu", "{answer}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("1 ms watchdog")),
        "{answer}"
    );
    let submission = &error["failure"]["diagnostics"]["gpuSubmission"];
    assert_eq!(submission["watchdogMs"], 1, "{answer}");
    assert!(
        submission["maxMs"].as_f64().is_some_and(|ms| ms >= 0.0),
        "{answer}"
    );
    // The frame was planned: the report carries what its longest submission was predicted to
    // take.
    assert!(submission["predictedMs"].as_f64().is_some(), "{answer}");
}

#[test]
fn an_injected_loss_after_light_frames_stays_device_lost() {
    let answer = lost_export(None);
    let error = &answer["error"];
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(error["code"], "GPU_DEVICE_LOST", "{answer}");
    assert_eq!(error["failure"]["code"], "DEVICE_LOST", "{answer}");
    let submission = &error["failure"]["diagnostics"]["gpuSubmission"];
    assert_eq!(
        submission["watchdogMs"],
        alpha2_gpu::WATCHDOG_MS,
        "{answer}"
    );
}

/// Exports three 640x360 frames of a still through the heavy watchdog fixture with a watchdog
/// of `watchdog_ms` (zero: the default), and returns the result's `gpuSubmissions`.
fn heavy_export_submissions(watchdog_ms: u32) -> Value {
    let directory = scratch_directory("split-frames");
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let fixture = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/cases/gpu-watchdog/heavy-effects.json"),
    )
    .expect("the heavy fixture");
    let heavy: Value = serde_json::from_str(&fixture).expect("JSON");
    let mut project = still_project([0.0, 0.0, 0.0], "bypass");
    project["settings"]["width"] = json!(640);
    project["settings"]["height"] = json!(360);
    project["compositions"] = json!([{"id": "look", "output": "effect", "nodes": [
        {"id": "clip", "op": {"type": "clip"}},
        {"id": "effect", "op": {"type": "effect", "input": "clip",
            "effect": {"id": heavy[0]["id"], "version": 1}}}]}]);
    project["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({"id": "look"});
    let request = parse_request(
        &json!({
            "protocolVersion": 1,
            "operation": "export",
            "requestId": "split",
            "project": project,
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png")},
            "effects": heavy,
            "startTicks": 0,
            "endTicks": 3 * 4_000,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "mpeg4-part2",
            "output": directory.join("split.mp4"),
        })
        .to_string(),
    )
    .expect("request");
    alpha2_gpu::set_watchdog_ms(watchdog_ms);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host.handle(request, &mut |_| Ok(()))
    }));
    alpha2_gpu::set_watchdog_ms(0);
    let result = result
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        .expect("the export finishes");
    result["performance"]["gpuSubmissions"].clone()
}

#[test]
fn export_performance_reports_split_frames() {
    let _gpu = gpu_test_lock();
    // A one-millisecond watchdog leaves no budget: every heavy frame is drawn in the most bands.
    let split = heavy_export_submissions(1);
    assert!(
        split["frames"].as_u64().is_some_and(|frames| frames >= 1),
        "{split}"
    );
    assert_eq!(split["splitFrames"], split["frames"], "{split}");
    // The heavy pass's bands, and the work before it in a command buffer of its own.
    assert!(
        split["maxPerFrame"].as_u64().is_some_and(|most| most >= 64),
        "{split}"
    );
    assert!(split["maxPredictedMs"].as_f64().is_some(), "{split}");
}
