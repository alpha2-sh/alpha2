//! A device lost after the export's own long submission is reported hung,
//! one lost after short ones stays lost. No test may stop a GPU, so the loss
//! is injected at a frame's readback (`ALPHA2_EXPORT_LOSE_DEVICE_AT_FRAME`)
//! after its submission ran, and the watchdog is shortened
//! (`ALPHA2_GPU_WATCHDOG_MS`) so a real heavy frame crosses the line. Both
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
    // A 2 ms watchdog puts the line at 1 ms: a blurred, grained 1080p frame runs longer on any
    // GPU, software adapters included.
    let answer = lost_export(Some("2"));
    let error = &answer["error"];
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(error["code"], "GPU_DEVICE_HUNG", "{answer}");
    assert_eq!(error["failure"]["code"], "DEVICE_HUNG", "{answer}");
    assert_eq!(error["failure"]["resource"], "gpu", "{answer}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("2 ms watchdog")),
        "{answer}"
    );
    let submission = &error["failure"]["diagnostics"]["gpuSubmission"];
    assert_eq!(submission["watchdogMs"], 2, "{answer}");
    assert!(
        submission["maxMs"].as_f64().is_some_and(|ms| ms >= 1.0),
        "{answer}"
    );
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
