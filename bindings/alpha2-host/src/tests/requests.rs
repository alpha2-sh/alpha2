use super::*;

fn export_line(video_quality: Option<Value>) -> String {
    let mut request = json!({
        "protocolVersion": 2,
        "operation": "export",
        "requestId": "quality",
        "project": {},
        "assets": {},
        "startTicks": 0,
        "endTicks": 1,
        "frameRate": {"num": 30, "den": 1},
        "output": "output.mp4"
    });
    if let Some(quality) = video_quality {
        request["videoQuality"] = quality;
    }
    request.to_string()
}

fn parsed_video_quality(video_quality: Option<Value>) -> Result<VideoQualityRequest, HostError> {
    match parse_request(&export_line(video_quality))? {
        Request::Export(export) => Ok(export.video_quality),
        Request::Capabilities { .. }
        | Request::Conform(_)
        | Request::Still(_)
        | Request::Compose(_)
        | Request::Edit(_)
        | Request::Extract(_) => {
            unreachable!("an export line parses as an export")
        }
    }
}

/// Small objects are the shape that costs a JSON tree the most per byte.
#[cfg(target_os = "linux")]
fn line_with_a_project_of_small_objects(objects: usize) -> String {
    let mut line = String::from(
        r#"{"protocolVersion":2,"operation":"export","requestId":"wide","assets":{},"startTicks":0,"endTicks":1,"frameRate":{"num":30,"den":1},"output":"out.mp4","project":{"cues":["#,
    );
    for index in 0..objects {
        if index > 0 {
            line.push(',');
        }
        line.push_str(r#"{"a":0}"#);
    }
    line.push_str("]}}");
    line
}

/// The parent closes the child's stderr pipe before the child writes, then its stdin, so the end
/// of input tells the child the read end is gone.
#[cfg(unix)]
fn run_with_stderr_closed(test: &str) -> (std::process::ExitStatus, String) {
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    drop(child.stderr.take());
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("child output");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[cfg(unix)]
#[test]
#[ignore = "run by a_closed_stderr_does_not_end_the_host with its standard error closed"]
fn start_a_host_and_export_with_stderr_closed() {
    std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).expect("read standard input");
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    // A worker thread that panicked on a diagnostic would be reported as some other failure.
    std::thread::spawn(|| alpha2_os::diagnostic!("alpha2-host test: a worker's line"))
        .join()
        .expect("a worker survives a closed stderr");
    drop(host);
}

/// A panic on a dropped diagnostic line would kill a worker or exit mid-export with status 101.
#[cfg(unix)]
#[test]
fn a_closed_stderr_does_not_end_the_host() {
    let (status, stdout) =
        run_with_stderr_closed("tests::requests::start_a_host_and_export_with_stderr_closed");
    assert!(
        status.success() && stdout.contains("1 passed"),
        "{status}\n{stdout}"
    );
}

/// Runs a child whose GPU waits are 1 s. Its exit status is `None` when it hung and was killed.
fn run_with_a_hung_release(test: &str, input: &str) -> (Option<i32>, String, String) {
    use std::io::Read as _;
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env("ALPHA2_GPU_TIMEOUT_MS", "1000")
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(&crate::serve_io::frames_of_lines(input.as_bytes()))
        .expect("write the input");
    drop(stdin);
    let readers = [
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    ]
    .map(|pipe| {
        let mut pipe = pipe.expect("pipe");
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let _ = pipe.read_to_end(&mut output);
            String::from_utf8_lossy(&output).into_owned()
        })
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("child status") {
            break status.code();
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let [stdout, stderr] = readers.map(|reader| reader.join().expect("reader"));
    (status, stdout, stderr)
}

fn assert_reported_before_the_release(stderr: &str, outcome: &str) {
    let reported = stderr.find(outcome);
    let released = stderr.find("releasing the GPU device took longer than 1000 ms");
    assert!(
        matches!((reported, released), (Some(reported), Some(released)) if reported < released),
        "{stderr}"
    );
}

/// `wgpu`'s queue drop calls `vkQueueWaitIdle`, which has no timeout, so the error must be printed
/// before the GPU host is dropped and the drop must be bounded.
#[test]
fn a_cli_export_whose_device_never_goes_idle_reports_its_error_and_exits() {
    let _gpu = gpu_test_lock();
    let (status, _, stderr) =
        run_with_a_hung_release("tests::requests::export_on_the_cli_with_a_hung_release", "");
    assert_eq!(status, Some(1), "{stderr}");
    assert_reported_before_the_release(
        &stderr,
        "alpha2-host: the project's media is not ready: video-source: offline",
    );
}

#[test]
#[ignore = "run by a_cli_export_whose_device_never_goes_idle_reports_its_error_and_exits"]
fn export_on_the_cli_with_a_hung_release() {
    crate::teardown::injected::stall_host_drops();
    let directory = scratch_directory("hung-release-export");
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/host-video-export-project.json")
        .display()
        .to_string();
    let args = [
        "--project".to_owned(),
        project,
        "--asset".to_owned(),
        format!("video-source={}", directory.join("missing.mp4").display()),
        "--out".to_owned(),
        directory.join("out.mp4").display().to_string(),
        "--no-audio".to_owned(),
    ];
    let mut host = None;
    let result = crate::cli::export_cli(args.to_vec(), &mut host);
    assert!(host.is_some(), "the export failed before its host started");
    let code = conclude(result, host);
    panic!("the release returned (status {code}) although it never finishes");
}

#[test]
fn serve_exits_at_the_end_of_its_input_when_its_device_never_goes_idle() {
    let _gpu = gpu_test_lock();
    let (status, stdout, stderr) = run_with_a_hung_release(
        "tests::requests::serve_with_a_hung_release",
        "{\"protocolVersion\":2,\"requestId\":\"caps\",\"operation\":\"capabilities\"}\n",
    );
    assert_eq!(status, Some(0), "{stderr}");
    assert!(
        stdout.contains(r#""requestId":"caps""#) && stdout.contains(r#""ok":true"#),
        "{stdout}"
    );
    assert_reported_before_the_release(&stderr, "alpha2-host MP4 encoders:");
}

#[test]
#[ignore = "run by serve_exits_at_the_end_of_its_input_when_its_device_never_goes_idle"]
fn serve_with_a_hung_release() {
    crate::teardown::injected::stall_host_drops();
    let mut host = None;
    let result = serve(&mut host, test_serve_limits());
    assert!(host.is_some(), "the request started no host");
    let code = conclude(result, host);
    panic!("the release returned (status {code}) although it never finishes");
}

#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_invalid_input() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let arguments = [
        OsString::from("export"),
        OsString::from("--asset"),
        OsString::from_vec(b"v=caf\xe9.mp4".to_vec()),
    ];
    let error = utf8_arguments(arguments.into_iter()).expect_err("not UTF-8");
    assert_eq!(error.code, "INVALID_INPUT");
    assert!(
        error.message.contains("v=caf\u{fffd}.mp4"),
        "{}",
        error.message
    );
    assert_eq!(
        utf8_arguments([OsString::from("serve")].into_iter()).expect("UTF-8"),
        ["serve"]
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "run by a_request_is_read_without_a_tree_of_its_project in a child process whose heap is limited"]
fn parse_a_wide_request_under_a_heap_limit() {
    // 15 MiB of project: under the engine's 64 MiB, and over a gigabyte as a tree.
    let line = line_with_a_project_of_small_objects(15 << 17);
    let request = parse_request(&line).expect("the request parses");
    let Request::Export(export) = request else {
        panic!("an export line parses as an export");
    };
    println!("project text: {} bytes", export.project.text().len());
    assert_eq!(crate::protocol::request_id_from_header(&line), "wide");
}

/// The project stays text until the engine's budget sees it; as a tree, a 15 MiB project of small
/// objects takes over a gigabyte, so the child runs with its heap limited to 256 MiB.
#[cfg(target_os = "linux")]
#[test]
fn a_request_is_read_without_a_tree_of_its_project() {
    let child = std::process::Command::new("prlimit")
        .arg(format!("--data={}", 256 << 20))
        .arg(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::requests::parse_a_wide_request_under_a_heap_limit",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    let stdout = String::from_utf8_lossy(&child.stdout);
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        child.status.success() && stdout.contains("1 passed"),
        "{}\n{stdout}\n{stderr}",
        child.status
    );
}

#[test]
fn a_frame_over_the_limit_is_skipped_and_the_next_is_read_whole() {
    use alpha2_host_core::host_frames::frame_bytes;
    let header_limit = crate::protocol::MAX_REQUEST_HEADER_BYTES as usize;
    let mut input = frame_bytes(&vec![b'x'; header_limit + 1], &[]).expect("frame");
    input.extend(frame_bytes(br#"{"next":1}"#, &[]).expect("frame"));
    input.extend(frame_bytes(b"\xFF", &[]).expect("frame"));
    input.extend(
        frame_bytes(br#"{"attachments":[{"type":"a/b","bytes":3}]}"#, &[b"abc"]).expect("frame"),
    );
    input.extend(frame_bytes(br#"{"attachments":[]}"#, &[b"abc"]).expect("frame"));
    let out = crate::serve_io::Captured::default().responder();
    let lifecycle = crate::lifecycle::Lifecycle::new(crate::lifecycle::SoftLimits::OFF);
    let requests = crate::serve_io::requests_of(&input, &out, &lifecycle);
    let mut read = Vec::new();
    loop {
        read.push(match requests.next() {
            InputFrame::End => break,
            InputFrame::Request {
                header,
                attachments: Ok(attachments),
            } => format!("{header} with {}", attachments.len()),
            InputFrame::Request {
                attachments: Err(reason),
                ..
            } => format!("refused: {reason}"),
            InputFrame::TooLarge(bytes) => format!("too large {bytes}"),
            InputFrame::NotUtf8 => "not utf-8".to_owned(),
        });
    }
    assert_eq!(
        read,
        [
            format!("too large {}", header_limit + 1),
            r#"{"next":1} with 0"#.to_owned(),
            "not utf-8".to_owned(),
            r#"{"attachments":[{"type":"a/b","bytes":3}]} with 1"#.to_owned(),
            "refused: the attachments list 0 bytes; the frame carries 3".to_owned(),
        ]
    );
}

#[test]
fn a_request_with_attachments_is_refused_by_its_id() {
    use alpha2_host_core::host_frames::frame_bytes;
    let input = frame_bytes(
        br#"{"protocolVersion":2,"requestId":"attached","operation":"compose","action":"templates","attachments":[{"type":"a/b","bytes":1}]}"#,
        &[b"x"],
    )
    .expect("frame");
    let captured = crate::serve_io::Captured::default();
    let out = captured.responder();
    let mut serving = Serving::new(VideoDecodePolicy::Software);
    serving.create = no_gpu;
    let requests = crate::serve_io::requests_of(&input, &out, &serving.lifecycle);
    serve_requests(&mut serving, &requests, &out, &|_| None).expect("serve");
    let answers = answers_of(&captured.bytes());
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["requestId"], "attached");
    assert_eq!(answers[0]["error"]["code"], "INVALID_REQUEST");
}

#[test]
fn serve_answers_oversized_requests_and_then_the_next() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = |id: &str, operation: &str, project: Option<String>| {
        let mut line =
            format!(r#"{{"protocolVersion":2,"requestId":"{id}","operation":"{operation}""#);
        if let Some(project) = project {
            line.push_str(r#","project":"#);
            line.push_str(&project);
        }
        line.push_str("}\n");
        line
    };
    let over_budget = format!(
        r#"{{"padding":"{}"}}"#,
        "x".repeat(alpha2_model::MAX_PROJECT_JSON_BYTES - 13)
    );
    assert_eq!(over_budget.len(), alpha2_model::MAX_PROJECT_JSON_BYTES + 1);
    let mut input = String::new();
    input.push_str(&request(
        "too-long",
        "conform",
        Some(format!(
            r#""{}""#,
            "x".repeat(crate::protocol::MAX_REQUEST_HEADER_BYTES as usize)
        )),
    ));
    input.push_str(&request("over-budget", "conform", Some(over_budget)));
    input.push_str(&request("next", "capabilities", None));
    let (served, output) = serve_input(
        &mut Serving::started(host),
        input.as_bytes(),
        // Other tests leave decode threads behind on purpose; `serve` would retire for them.
        &|_| None,
    );
    served.expect("serve");
    let answers = answers_of(&output);
    assert_eq!(answers.len(), 3, "{answers:?}");
    assert_eq!(answers[0]["requestId"], "unknown");
    assert_eq!(answers[0]["error"]["code"], "INVALID_REQUEST");
    assert!(
        answers[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("the request frame is")),
        "{}",
        answers[0]
    );
    assert_eq!(answers[1]["requestId"], "over-budget");
    assert_eq!(answers[1]["ok"], false);
    assert!(
        answers[1]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(&format!(
                "the maximum is {}",
                alpha2_model::MAX_PROJECT_JSON_BYTES
            ))),
        "{}",
        answers[1]
    );
    assert_eq!(answers[2]["requestId"], "next");
    assert_eq!(answers[2]["ok"], true, "{}", answers[2]);
}

/// The GPU host (and under `hardware-required` the decode device) is made for the first request
/// that needs it, so `conform` works without a GPU.
#[test]
fn serve_answers_conform_without_a_gpu_and_reports_a_failed_start_on_the_first_request_needing_one()
{
    let clip = corpus("parity-rgb.mov");
    let conform = json!({
        "protocolVersion": 2, "requestId": "conform", "operation": "conform",
        "project": conform_project(&json!({})),
        "assets": {"video": clip}
    });
    let input = format!(
        "{conform}\n{}\n{}\n",
        r#"{"protocolVersion":2,"requestId":"caps","operation":"capabilities"}"#,
        r#"{"protocolVersion":2,"requestId":"after","operation":"capabilities"}"#
    );
    for policy in [
        VideoDecodePolicy::Software,
        VideoDecodePolicy::HardwareRequired,
    ] {
        let mut serving = Serving::new(policy);
        serving.create = no_gpu;
        let answered = std::sync::atomic::AtomicUsize::new(0);
        let (served, output) = serve_input(&mut serving, input.as_bytes(), &|host| {
            assert!(host.is_none(), "conform made no host");
            answered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        });
        let error = served.expect_err("capabilities needs the GPU host");
        assert_eq!(error.code, "GPU_INIT_FAILED");
        assert_eq!(answered.into_inner(), 1);
        let answers = answers_of(&output);
        assert_eq!(answers.len(), 2, "{answers:?}");
        assert_eq!(answers[0]["requestId"], "conform", "{}", answers[0]);
        assert_eq!(answers[0]["ok"], true, "{}", answers[0]);
        assert_eq!(
            answers[0]["result"]["report"]["ready"], true,
            "{}",
            answers[0]
        );
        assert_eq!(answers[1]["type"], "fatal", "{}", answers[1]);
        assert_eq!(
            answers[1]["error"]["code"], "GPU_INIT_FAILED",
            "{}",
            answers[1]
        );
        assert_eq!(
            answers[1]["error"]["failure"]["hostCode"], "GPU_INIT_FAILED",
            "{}",
            answers[1]
        );
    }
}

#[test]
fn serve_refuses_a_composition_scale_with_a_zero_term_as_invalid_input() {
    let mut input = String::new();
    for (id, num, den) in [("num-zero", 0, 1), ("den-zero", 1, 0)] {
        let mut project = conform_project(&json!({}));
        project["compositions"] = json!([{
            "id": "g", "nodes": [{"id": "clip", "op": {"type": "clip"}}], "output": "clip"
        }]);
        project["timeline"]["tracks"][0]["clips"][0]["composition"] =
            json!({"id": "g", "scale": {"num": num, "den": den}});
        let request = json!({
            "protocolVersion": 2, "requestId": id, "operation": "conform",
            "project": project, "assets": {}
        });
        input.push_str(&request.to_string());
        input.push('\n');
    }
    let mut serving = Serving::new(VideoDecodePolicy::Software);
    serving.create = no_gpu;
    let (served, output) = serve_input(&mut serving, input.as_bytes(), &|host| {
        assert!(host.is_none(), "conform made no host");
        None
    });
    served.expect("serve");
    let answers = answers_of(&output);
    assert_eq!(answers.len(), 2, "{answers:?}");
    for (answer, id) in answers.iter().zip(["num-zero", "den-zero"]) {
        assert_eq!(answer["requestId"], id, "{answer}");
        assert_eq!(answer["ok"], false, "{answer}");
        assert_eq!(answer["error"]["code"], "INVALID_INPUT", "{answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("invalid composition reference")),
            "{answer}"
        );
    }
}

#[test]
fn serve_answers_each_edge_line_by_its_id_and_serves_the_rest() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let mut input = Vec::new();
    for line in [
        &br#"{"protocolVersion":2,"requestId":"first","operation":"capabilities"}"#[..],
        br#"{"protocolVersion":2,"requestId":"bad-operation","operation":"render"}"#,
        br#"{"protocolVersion":2,"requestId":"truncated","operation":"capabilities","#,
        b"",
        b"   ",
        br#"{"protocolVersion":2,"requestId":"","operation":"capabilities"}"#,
        b"{\"protocolVersion\":2,\"requestId\":\"latin-1\",\"path\":\"caf\xe9\"}",
        b"not JSON",
        br#"{"protocolVersion":2,"requestId":"last","operation":"capabilities"}"#,
    ] {
        input.extend_from_slice(line);
        input.push(b'\n');
    }
    let answered = std::sync::atomic::AtomicUsize::new(0);
    let (served, output) = serve_input(&mut Serving::started(host), input.as_slice(), &|_| {
        answered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    });
    served.expect("serve");
    let answers = answers_of(&output);
    assert_eq!(answered.into_inner(), answers.len());
    let summary: Vec<(String, Value)> = answers
        .iter()
        .map(|answer| {
            (
                answer["requestId"].as_str().expect("an id").to_owned(),
                answer["error"]["code"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("first".to_owned(), Value::Null),
            ("bad-operation".to_owned(), json!("INVALID_REQUEST")),
            ("unknown".to_owned(), json!("INVALID_REQUEST")),
            ("unknown".to_owned(), json!("INVALID_REQUEST")),
            ("unknown".to_owned(), json!("INVALID_REQUEST")),
            ("unknown".to_owned(), json!("INVALID_REQUEST")),
            ("last".to_owned(), Value::Null),
        ],
        "{answers:?}"
    );
    assert!(
        answers[3]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("requestId must not be empty")),
        "{}",
        answers[3]
    );
    assert!(
        answers[4]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("UTF-8")),
        "{}",
        answers[4]
    );
    assert_eq!(answers[6]["ok"], true, "{}", answers[6]);
}

fn lose_device(host: &Host) {
    host.gpu_log.push(alpha2_gpu::GpuDiagnostic::DeviceLost {
        reason: "Unknown".to_owned(),
        message: "injected by the test".to_owned(),
    });
}

/// The renderer learns of a loss only at its next submission, so without retiring the host every
/// later export would open its output and fail on the dead device.
#[test]
fn a_device_wgpu_reported_lost_retires_the_host() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    assert_eq!(gpu_unusable(&host), None);
    lose_device(&host);
    host.gpu_log.clear();
    let reason = gpu_unusable(&host).expect("a lost device retires the host");
    assert!(reason.contains("lost"), "{reason}");
}

#[test]
#[ignore = "run by a_host_that_cannot_take_another_request_drains_and_exits_after_answering"]
fn serve_pipelined_requests_on_an_unusable_host() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    match std::env::var("ALPHA2_TEST_UNUSABLE").as_deref() {
        Ok("device-lost") => lose_device(&host),
        Ok("threads-abandoned") => {
            let stuck = std::thread::spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            });
            assert!(!crate::heartbeat::join_or_abandon(
                stuck,
                &[],
                Duration::ZERO,
                "test worker"
            ));
        }
        other => panic!("ALPHA2_TEST_UNUSABLE={other:?}"),
    }
    let input = [
        r#"{"protocolVersion":2,"requestId":"first","operation":"capabilities"}"#,
        r#"{"protocolVersion":2,"requestId":"second","operation":"capabilities"}"#,
    ]
    .join("\n");
    let out = crate::serve_io::Responder::new(Box::new(std::io::stdout()));
    out.start().expect("magic");
    let mut serving = Serving::started(host);
    let frames = crate::serve_io::frames_of_lines(input.as_bytes());
    let requests = crate::serve_io::requests_of(&frames, &out, &serving.lifecycle);
    let served = serve_requests(&mut serving, &requests, &out, &unusable_reason);
    exit_if_retired(&served);
    panic!("the host answered every request instead of exiting after the first: {served:?}");
}

#[test]
fn a_host_that_cannot_take_another_request_drains_and_exits_after_answering() {
    for unusable in ["device-lost", "threads-abandoned"] {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::requests::serve_pipelined_requests_on_an_unusable_host",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ALPHA2_TEST_UNUSABLE", unusable)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{unusable}\n{stdout}\n{stderr}"
        );
        let messages = child_messages(&output.stdout);
        assert_eq!(messages.len(), 3, "{unusable}\n{stdout}");
        assert_eq!(messages[0]["type"], "state", "{unusable}\n{stdout}");
        assert_eq!(messages[0]["state"], "draining", "{unusable}\n{stdout}");
        assert_eq!(messages[0]["reason"], unusable, "{unusable}\n{stdout}");
        assert_eq!(messages[1]["requestId"], "first", "{unusable}\n{stdout}");
        assert_eq!(messages[1]["ok"], true, "{unusable}\n{stdout}");
        assert_eq!(messages[1]["hostState"], "draining", "{unusable}\n{stdout}");
        assert_eq!(messages[2]["requestId"], "second", "{unusable}\n{stdout}");
        assert_eq!(
            messages[2]["error"]["code"], "HOST_RETIRING",
            "{unusable}\n{stdout}"
        );
    }
}

#[test]
#[ignore = "run by an_unknown_decode_mode_is_a_fatal_invalid_input with ALPHA2_VIDEO_DECODE set"]
fn serve_with_an_unknown_decode_mode() {
    let error = serve(&mut None, test_serve_limits()).expect_err("an unknown mode");
    assert_eq!(error.code, "INVALID_INPUT");
}

#[test]
fn an_unknown_decode_mode_is_a_fatal_invalid_input() {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::requests::serve_with_an_unknown_decode_mode",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ALPHA2_VIDEO_DECODE", "hardware")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("child");
    // Once `serve` owns stdout, libtest's summary goes to stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("1 passed"), "{stderr}");
    let fatal = child_messages(&output.stdout)
        .into_iter()
        .find(|message| message["type"] != "state")
        .expect("a fatal notification");
    assert_eq!(fatal["type"], "fatal", "{fatal}");
    assert_eq!(fatal["error"]["code"], "INVALID_INPUT", "{fatal}");
    assert!(
        fatal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("got hardware")),
        "{fatal}"
    );
    assert_eq!(
        fatal["error"]["failure"]["code"], "INVALID_INPUT",
        "{fatal}"
    );
}

#[test]
fn a_refused_export_and_a_failed_start_carry_a_failure_report() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let lines = [
        r#"{"protocolVersion":2,"requestId":"json","operation":"export","#.to_owned(),
        r#"{"protocolVersion":9,"requestId":"version","operation":"export"}"#.to_owned(),
        r#"{"protocolVersion":2,"requestId":"quality","operation":"export","videoQuality":{"bitrate":-1}}"#.to_owned(),
        r#"{"protocolVersion":2,"requestId":"conform","operation":"conform"}"#.to_owned(),
        format!(
            r#"{{"protocolVersion":2,"requestId":"long","operation":"export","project":"{}"}}"#,
            "x".repeat(crate::protocol::MAX_REQUEST_HEADER_BYTES as usize)
        ),
        r#"{"protocolVersion":2,"requestId":"next","operation":"capabilities"}"#.to_owned(),
    ];
    let input = lines.join("\n") + "\n";
    let (served, output) = serve_input(&mut Serving::started(host), input.as_bytes(), &|_| None);
    served.expect("serve");
    let answers = answers_of(&output);
    assert_eq!(answers.len(), lines.len(), "{answers:?}");
    for (answer, code) in answers.iter().zip([
        Some("INVALID_REQUEST"),
        Some("UNSUPPORTED_PROTOCOL"),
        Some("INVALID_INPUT"),
        None,
        Some("INVALID_REQUEST"),
    ]) {
        let error = &answer["error"];
        assert_eq!(answer["ok"], false, "{answer}");
        let Some(code) = code else {
            assert!(error.get("failure").is_none(), "{answer}");
            continue;
        };
        assert_eq!(error["code"], code, "{answer}");
        let failure = &error["failure"];
        assert_eq!(failure["code"], "INVALID_INPUT", "{answer}");
        assert_eq!(failure["hostCode"], code, "{answer}");
        assert_eq!(failure["phase"], "prepare", "{answer}");
        assert_eq!(failure["origin"], "native-host", "{answer}");
        assert!(
            failure["diagnostics"]["environment"].is_object(),
            "{answer}"
        );
        assert!(failure["diagnostics"].get("request").is_none(), "{answer}");
    }
    assert_eq!(answers[5]["ok"], true, "{}", answers[5]);

    let fatal = serde_json::to_value(fatal_error(HostError::new(
        "HARDWARE_DECODE_UNAVAILABLE",
        "no decode device",
    )))
    .expect("JSON");
    assert_eq!(fatal["failure"]["code"], "INVALID_INPUT");
    assert_eq!(fatal["failure"]["hostCode"], "HARDWARE_DECODE_UNAVAILABLE");
}

#[test]
fn video_quality_defaults_to_high_and_takes_both_forms() {
    assert_eq!(
        parsed_video_quality(None).expect("absent"),
        VideoQualityRequest::Level(QualityLevel::High)
    );
    for level in QualityLevel::ALL {
        assert_eq!(
            parsed_video_quality(Some(json!(level.as_str()))).expect("level"),
            VideoQualityRequest::Level(level)
        );
    }
    assert_eq!(
        parsed_video_quality(Some(json!({"bitrate": 8_000_000}))).expect("bitrate"),
        VideoQualityRequest::Explicit(VideoRateControl {
            bitrate: 8_000_000,
            mode: BitrateMode::Variable,
            quantizer: None,
        })
    );
    assert_eq!(
        parsed_video_quality(Some(json!({"bitrate": 8_000_000, "mode": "constant"})))
            .expect("constant"),
        VideoQualityRequest::Explicit(VideoRateControl {
            bitrate: 8_000_000,
            mode: BitrateMode::Constant,
            quantizer: None,
        })
    );
}

#[test]
fn an_export_includes_audio_unless_the_request_says_not_to() {
    let include_audio = |value: Option<bool>| {
        let mut request: Value = serde_json::from_str(&export_line(None)).expect("line");
        if let Some(value) = value {
            request["includeAudio"] = json!(value);
        }
        match parse_request(&request.to_string()).expect("export") {
            Request::Export(export) => export.include_audio,
            Request::Capabilities { .. }
            | Request::Conform(_)
            | Request::Still(_)
            | Request::Compose(_)
            | Request::Edit(_)
            | Request::Extract(_) => {
                unreachable!("an export line parses as an export")
            }
        }
    };
    assert!(include_audio(None));
    assert!(include_audio(Some(true)));
    assert!(!include_audio(Some(false)));
}

#[test]
fn a_malformed_memory_is_invalid_input_naming_it() {
    for (memory, needle) in [
        (json!({"inflightBytes": 1_048_576}), "inflightBytes"),
        (json!({"inFlightBytes": "1048576"}), "memory"),
        (json!({"scale": "half"}), "memory"),
        (json!(1_048_576), "memory"),
        (Value::Null, "memory"),
    ] {
        let mut request: Value = serde_json::from_str(&export_line(None)).expect("line");
        request["memory"] = memory.clone();
        let Err(error) = parse_request(&request.to_string()) else {
            panic!("{memory} must be refused");
        };
        assert_eq!(error.code, "INVALID_INPUT", "{memory}: {}", error.message);
        assert!(
            error.message.contains(needle),
            "{memory}: {} does not name {needle}",
            error.message
        );
    }
    let mut request: Value = serde_json::from_str(&export_line(None)).expect("line");
    request["memory"] = json!({"inFlightBytes": 1_048_576, "scale": 0.5});
    assert!(parse_request(&request.to_string()).is_ok());
}

#[test]
fn invalid_video_quality_is_invalid_input_naming_the_field() {
    for (value, needle) in [
        (json!("ultra"), "videoQuality"),
        (json!("lossless"), "videoQuality"),
        (json!({"bitrate": 0}), "videoQuality.bitrate"),
        (json!({"bitrate": -8_000_000}), "videoQuality.bitrate"),
        (json!({"bitrate": 1.5}), "videoQuality.bitrate"),
        (json!({"bitrate": "8000000"}), "videoQuality.bitrate"),
        (json!({}), "videoQuality.bitrate"),
        (
            json!({"bitrate": 8_000_000, "mode": "cbr"}),
            "videoQuality.mode",
        ),
        (json!({"bitrate": 8_000_000, "crf": 23}), "videoQuality"),
        (json!(8_000_000), "videoQuality"),
        (Value::Null, "videoQuality"),
    ] {
        let error = parsed_video_quality(Some(value.clone()))
            .expect_err(&format!("{value} must be rejected"));
        assert_eq!(error.code, "INVALID_INPUT", "{value}: {}", error.message);
        assert!(
            error.message.contains(needle),
            "{value}: {} does not name {needle}",
            error.message
        );
    }
}

#[test]
fn an_explicit_rate_may_carry_a_quantizer_with_its_bitrate_as_fallback() {
    assert_eq!(
        parsed_video_quality(Some(json!({"bitrate": 8_000_000, "quantizer": 26})))
            .expect("quantizer"),
        VideoQualityRequest::Explicit(VideoRateControl {
            bitrate: 8_000_000,
            mode: BitrateMode::Variable,
            quantizer: Some(26),
        })
    );
    for value in [
        json!({"bitrate": 8_000_000, "quantizer": 256}),
        json!({"bitrate": 8_000_000, "quantizer": -1}),
        json!({"bitrate": 8_000_000, "quantizer": 1.5}),
        json!({"quantizer": 26}),
    ] {
        let error = parsed_video_quality(Some(value.clone())).expect_err("rejected");
        assert_eq!(error.code, "INVALID_INPUT", "{value}");
        assert!(
            error.message.contains("videoQuality"),
            "{value}: {}",
            error.message
        );
    }
    let cli = |args: &[&str]| {
        crate::cli::parse_export_args(args.iter().map(|arg| (*arg).to_owned()).collect())
    };
    assert_eq!(
        cli(&["--video-quality", "8000000", "--video-quantizer", "26"])
            .expect("parses")
            .video_quantizer,
        Some(26)
    );
    assert!(cli(&["--video-quantizer", "300"]).is_err());
}

/// An empty render-cache directory would bake into `./v1`, so the CLI must refuse it up front.
#[test]
fn a_cli_export_is_checked_as_the_protocol_checks_it() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/host-video-export-project.json")
        .display()
        .to_string();
    let request = |extra: &[&str]| {
        let mut args = ["--project", project.as_str(), "--out", "unused.mp4"]
            .map(str::to_owned)
            .to_vec();
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        crate::cli::parse_export_args(args).and_then(crate::cli::export_request)
    };
    let refused = |extra: &[&str], words: &str| {
        let error = request(extra)
            .err()
            .unwrap_or_else(|| panic!("{extra:?} must be refused"));
        assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
        assert!(error.message.contains(words), "{}", error.message);
    };
    refused(&["--no-audio", "--loudness-target", "-14"], "includeAudio");
    refused(&["--render-cache", ""], "non-empty");
    refused(
        &["--render-cache", "cache", "--render-cache-quota", "0"],
        "positive",
    );
    assert!(request(&["--loudness-target", "-14"]).is_ok());
    assert!(request(&["--render-cache", "cache"]).is_ok());
    let line = |fields: Value| {
        let mut request: Value = serde_json::from_str(&export_line(None)).expect("line");
        for (key, value) in fields.as_object().expect("fields") {
            request[key] = value.clone();
        }
        crate::protocol::parse_request(&request.to_string()).map(|_| ())
    };
    let loudness = json!({"targetLufs": -14, "truePeakCeilingDbtp": -1});
    assert_eq!(
        line(json!({"loudness": loudness, "includeAudio": false}))
            .expect_err("no audio")
            .code,
        "INVALID_INPUT"
    );
    assert!(line(json!({"loudness": loudness, "includeAudio": true})).is_ok());
    assert_eq!(
        line(json!({"renderCache": {"directory": ""}}))
            .expect_err("empty directory")
            .code,
        "INVALID_INPUT"
    );
}

#[test]
fn cli_video_quality_takes_a_level_or_bits_per_second() {
    let parse = |value: &str| {
        crate::cli::parse_export_args(vec!["--video-quality".to_owned(), value.to_owned()])
            .map(|options| options.video_quality)
    };
    assert_eq!(
        parse("medium").expect("level"),
        VideoQualityRequest::Level(QualityLevel::Medium)
    );
    assert_eq!(
        parse("12000000").expect("bits per second"),
        VideoQualityRequest::Explicit(VideoRateControl {
            bitrate: 12_000_000,
            mode: BitrateMode::Variable,
            quantizer: None,
        })
    );
    for invalid in ["0", "ultra", "-5", "12e6", ""] {
        let error = parse(invalid)
            .err()
            .unwrap_or_else(|| panic!("{invalid:?} must be rejected"));
        assert_eq!(error.code, "INVALID_INPUT");
        assert!(
            error.message.contains("--video-quality"),
            "{}",
            error.message
        );
    }
}

#[test]
fn audio_quality_takes_a_level_or_a_bitrate_and_names_itself_when_wrong() {
    let parse = |quality: Option<Value>| {
        let mut request: Value = serde_json::from_str(&export_line(None)).expect("line");
        if let Some(quality) = quality {
            request["audioQuality"] = quality;
        }
        match parse_request(&request.to_string())? {
            Request::Export(export) => Ok(export.audio_quality),
            Request::Capabilities { .. }
            | Request::Conform(_)
            | Request::Still(_)
            | Request::Compose(_)
            | Request::Edit(_)
            | Request::Extract(_) => {
                unreachable!("an export line parses as an export")
            }
        }
    };
    assert_eq!(
        parse(None).expect("absent"),
        AudioQualityRequest::Level(QualityLevel::High)
    );
    assert_eq!(
        parse(Some(json!("low"))).expect("level"),
        AudioQualityRequest::Level(QualityLevel::Low)
    );
    assert_eq!(
        parse(Some(json!({"bitrate": 96_000}))).expect("bitrate"),
        AudioQualityRequest::Explicit(96_000)
    );
    for value in [
        json!("ultra"),
        json!({"bitrate": 0}),
        json!({"bitrate": 96_000, "mode": "constant"}),
        json!({"bitrate": 5_000_000_000_u64}),
    ] {
        let error: HostError = parse(Some(value.clone())).expect_err("rejected");
        assert_eq!(error.code, "INVALID_INPUT", "{value}");
        assert!(
            error.message.contains("audioQuality"),
            "{value}: {}",
            error.message
        );
    }
    let cli = |value: &str| {
        crate::cli::parse_export_args(vec!["--audio-quality".to_owned(), value.to_owned()])
            .map(|options| options.audio_quality)
    };
    assert_eq!(
        cli("128000").expect("bps"),
        AudioQualityRequest::Explicit(128_000)
    );
    assert!(cli("0").is_err());
}

#[test]
fn shared_rust_and_node_protocol_corpus_agrees() {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-protocol-corpus.json"
    ))
    .expect("protocol corpus is JSON");
    for fixture in fixtures.as_array().expect("protocol corpus is an array") {
        let name = fixture["name"].as_str().expect("fixture name");
        let line = serde_json::to_string(&fixture["request"]).expect("request JSON");
        if fixture["request"]["operation"] == "cancel" {
            // The request reader takes a cancel line; the server never sees one.
            let cancel = crate::protocol::parse_cancel(&line);
            assert_eq!(
                cancel.is_ok(),
                fixture["requestValid"].as_bool() == Some(true),
                "{name}: {cancel:?}"
            );
        } else if fixture["request"]["operation"] == "status" {
            let status = crate::protocol::parse_status(&line);
            assert_eq!(
                status.is_ok(),
                fixture["requestValid"].as_bool() == Some(true),
                "{name}: {status:?}"
            );
        } else if fixture["requestValid"].as_bool() == Some(true) {
            let request = parse_request(&line).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(request.protocol_version(), PROTOCOL_VERSION, "{name}");
        } else {
            let error = parse_request(&line)
                .err()
                .unwrap_or_else(|| panic!("{name}: expected error"));
            assert_eq!(
                error.code,
                fixture["expectedErrorCode"]
                    .as_str()
                    .expect("expected error code"),
                "{name}"
            );
        }
        for message in fixture["messages"].as_array().expect("fixture messages") {
            assert_eq!(
                message["protocolVersion"].as_u64(),
                Some(u64::from(PROTOCOL_VERSION)),
                "{name}"
            );
            if message.get("ok").is_some() {
                assert!(
                    ["ready", "draining"].contains(&message["hostState"].as_str().unwrap_or("")),
                    "{name}"
                );
            }
            if message["type"] == "state" {
                assert!(
                    ["ready", "draining"].contains(&message["state"].as_str().unwrap_or("")),
                    "{name}"
                );
                assert!(message["reason"].as_str().is_some(), "{name}");
                assert!(message["details"]["limits"].is_object(), "{name}");
            } else if message["type"] == "progress" {
                assert!(message["completed"].as_u64().is_some(), "{name}");
                assert!(message["total"].as_u64().is_some(), "{name}");
            } else if message["ok"] == false {
                assert!(message["error"]["code"].as_str().is_some(), "{name}");
                assert!(message["error"]["message"].as_str().is_some(), "{name}");
            } else if message["type"] == "fatal" {
                assert!(message["error"]["code"].as_str().is_some(), "{name}");
            } else {
                assert_eq!(message["ok"].as_bool(), Some(true), "{name}");
            }
        }
    }
}

#[test]
fn runtime_effect_wire_shape_matches_the_timeline_editor() {
    let request: EffectRequest = serde_json::from_value(json!({
        "id": "demo.effect.test",
        "version": 1,
        "kind": "effect",
        "inputs": [{ "name": "map" }],
        "params": [{
            "name": "amount",
            "type": "float",
            "default": 0.5,
            "min": 0,
            "max": 1,
            "step": 0.01,
            "unit": "normalized"
        }],
        "wgslBody": "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }"
    }))
    .expect("web effect request parses");
    let definition = request.into_definition().expect("definition converts");
    assert_eq!(definition.id, "demo.effect.test");
    assert_eq!(definition.inputs, [InputSpec { name: "map".into() }]);
    assert_eq!(definition.params.len(), 1);
}

#[test]
fn an_effect_request_lists_its_named_inputs_or_leaves_them_out() {
    let wire = |inputs: Option<Value>| {
        let mut request = json!({
            "id": "demo.effect.plain",
            "version": 1,
            "kind": "effect",
            "wgslBody": "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }"
        });
        if let Some(inputs) = inputs {
            request["inputs"] = inputs;
        }
        serde_json::from_value::<EffectRequest>(request)
    };
    let definition = wire(None)
        .expect("inputs may be left out")
        .into_definition()
        .expect("definition converts");
    assert_eq!(definition.inputs, []);
    let error = wire(Some(json!(1))).expect_err("a count is not a list of inputs");
    assert!(error.to_string().contains("invalid type"), "{error}");
    let error = wire(Some(json!([{ "name": "map", "wrap": "repeat" }])))
        .expect_err("an input has a name only");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

fn format_line(fields: &Value) -> String {
    let mut request = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "operation": "export",
        "requestId": "format",
        "project": {},
        "assets": {},
        "startTicks": 0,
        "endTicks": 1,
        "frameRate": {"num": 30, "den": 1},
        "output": "output.mp4"
    });
    for (key, value) in fields.as_object().expect("fields") {
        request[key] = value.clone();
    }
    request.to_string()
}

fn resolved_format(fields: &Value) -> Result<crate::protocol::ResolvedFormat, HostError> {
    match parse_request(&format_line(fields))? {
        Request::Export(export) => export.format(),
        Request::Capabilities { .. }
        | Request::Conform(_)
        | Request::Still(_)
        | Request::Compose(_)
        | Request::Edit(_)
        | Request::Extract(_) => {
            unreachable!("an export line parses as an export")
        }
    }
}

#[test]
fn codec_request_ids_match_the_browser_protocol() {
    use alpha2_media::Container;
    for (id, native) in [
        ("h264", NativeVideoCodec::H264),
        ("hevc", NativeVideoCodec::Hevc),
        ("vp9", NativeVideoCodec::Vp9),
        ("av1", NativeVideoCodec::Av1),
        ("mpeg4-part2", NativeVideoCodec::Mpeg4Part2),
    ] {
        let format = resolved_format(&json!({"videoCodec": id, "output": "out.mkv"}))
            .unwrap_or_else(|error| panic!("{id}: {}", error.message));
        assert_eq!(format.video, native, "{id}");
        assert_eq!(format.container, Container::Mkv);
    }
    for (id, native) in [
        ("aac", NativeAudioCodec::Aac),
        ("opus", NativeAudioCodec::Opus),
    ] {
        let format = resolved_format(&json!({"audioCodec": id})).expect(id);
        assert_eq!(format.audio, native, "{id}");
    }
    let format = resolved_format(&json!({})).expect("defaults");
    assert_eq!(
        (format.container, format.video, format.audio),
        (
            Container::Mp4,
            NativeVideoCodec::H264,
            NativeAudioCodec::Aac
        )
    );
    let request = parse_request(
        &json!({
        "protocolVersion": PROTOCOL_VERSION,
        "operation": "export",
        "requestId": "encoder-selection",
        "project": {},
        "assets": {},
        "startTicks": 0,
        "endTicks": 1,
        "frameRate": {"num": 30, "den": 1},
        "videoCodec": "h264",
        "videoEncoder": "h264_nvenc",
        "output": "output.mp4"
        })
        .to_string(),
    )
    .expect("concrete encoder selection parses");
    assert!(matches!(
        request,
        Request::Export(export) if export.video_encoder.as_deref() == Some("h264_nvenc")
    ));
}

#[test]
fn an_export_resolves_its_container_from_the_request_or_its_output() {
    use alpha2_media::Container;
    let format = resolved_format(&json!({"output": "clip.WEBM"})).expect("webm");
    assert_eq!(
        (format.container, format.video, format.audio),
        (
            Container::Webm,
            NativeVideoCodec::Vp9,
            NativeAudioCodec::Opus
        )
    );
    let format = resolved_format(&json!({"container": "mov", "output": "clip.mov"})).expect("mov");
    assert_eq!(format.container, Container::Mov);
    let refused = |fields: Value, words: &str| {
        let error = resolved_format(&fields)
            .err()
            .unwrap_or_else(|| panic!("{fields} must be refused"));
        assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
        assert!(error.message.contains(words), "{fields}: {}", error.message);
    };
    refused(
        json!({"container": "mov", "output": "clip.mp4"}),
        "output: a mov output must end in .mov, not \".mp4\"",
    );
    refused(
        json!({"container": "webm", "output": "clip.webm", "videoCodec": "h264"}),
        "videoCodec: webm does not offer video codec h264; it offers vp9, av1",
    );
    refused(
        json!({"output": "clip.mov", "audioCodec": "opus"}),
        "audioCodec: mov does not offer audio codec opus; it offers aac",
    );
    refused(json!({"output": "clip.avi"}), "names no container");
    refused(json!({"output": "clip"}), "has no extension");
    refused(
        json!({"container": "avi"}),
        "container: unknown container \"avi\"",
    );
    refused(json!({"videoCodec": "theora"}), "videoCodec: unknown codec");
    refused(json!({"audioCodec": "mp3"}), "audioCodec: unknown codec");
    let error = parse_request(&format_line(&json!({"output": "clip.avi"})))
        .err()
        .expect("refused at parse");
    assert_eq!(error.code, "INVALID_INPUT");
}

#[test]
fn a_cli_export_resolves_its_container_as_the_protocol_does() {
    use alpha2_media::Container;
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/host-video-export-project.json")
        .display()
        .to_string();
    let format = |out: &str, extra: &[&str]| {
        let mut args = ["--project", project.as_str(), "--out", out]
            .map(str::to_owned)
            .to_vec();
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        crate::cli::parse_export_args(args)
            .and_then(crate::cli::export_request)
            .and_then(|(request, _)| match request {
                Request::Export(export) => export.format(),
                Request::Capabilities { .. }
                | Request::Conform(_)
                | Request::Still(_)
                | Request::Compose(_)
                | Request::Edit(_)
                | Request::Extract(_) => {
                    unreachable!("an export")
                }
            })
    };
    let webm = format("out.webm", &[]).expect("webm");
    assert_eq!(
        (webm.container, webm.video, webm.audio),
        (
            Container::Webm,
            NativeVideoCodec::Vp9,
            NativeAudioCodec::Opus
        )
    );
    let mkv = format("out.mkv", &["--container", "mkv", "--audio-codec", "opus"]).expect("mkv");
    assert_eq!(
        (mkv.container, mkv.video, mkv.audio),
        (
            Container::Mkv,
            NativeVideoCodec::H264,
            NativeAudioCodec::Opus
        )
    );
    for (out, extra, words) in [
        ("out.mp4", &["--container", "mov"][..], "must end in .mov"),
        (
            "out.webm",
            &["--video-codec", "h264"][..],
            "webm does not offer",
        ),
        ("out.avi", &[][..], "names no container"),
        (
            "out.mp4",
            &["--container", "avi"][..],
            "--container: unknown container",
        ),
    ] {
        let error = format(out, extra)
            .err()
            .unwrap_or_else(|| panic!("{out} {extra:?} must be refused"));
        assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
        assert!(error.message.contains(words), "{}", error.message);
    }
}

#[test]
fn capabilities_report_the_adapter_choice() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let report = host.capability_report();
    let adapters = &report.adapters;
    if host.renderer.cpu().is_some() {
        assert_eq!(*adapters, Value::Null);
        return;
    }
    assert_eq!(
        adapters["policy"],
        json!(crate::adapter_choice::AdapterPolicy::from_env().report_name())
    );
    assert!(
        matches!(
            adapters["reason"].as_str(),
            Some(
                "most-gpu-encoders"
                    | "decodes"
                    | "high-performance-order"
                    | "performance-policy"
                    | "pinned"
                    | "only-adapter"
                    | "no-shared-device"
            )
        ),
        "{adapters}"
    );
    let explanation = adapters["explanation"].as_str().expect("an explanation");
    assert!(explanation.contains(&report.gpu.adapter), "{adapters}");
    let candidates = adapters["candidates"].as_array().expect("candidates");
    match adapters["chosen"].as_u64() {
        Some(index) => {
            let chosen = candidates
                .iter()
                .find(|candidate| candidate["index"].as_u64() == Some(index))
                .expect("the chosen adapter is a candidate");
            assert_eq!(chosen["name"], json!(report.gpu.adapter), "{adapters}");
            assert_eq!(chosen["renders"], json!(true), "{adapters}");
        }
        None => assert_eq!(adapters["reason"], json!("no-shared-device"), "{adapters}"),
    }
    for candidate in candidates {
        for key in [
            "index", "name", "backend", "luid", "software", "renders", "decodes",
        ] {
            assert!(candidate.get(key).is_some(), "{key} in {candidate}");
        }
        assert!(candidate["gpuEncodes"].is_array(), "{candidate}");
    }
    let environment = host.environment_json();
    assert_eq!(environment["adapterChoice"], *adapters);
}

#[test]
fn capabilities_are_the_shared_capability_report() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = parse_request(
        &json!({"protocolVersion": 2, "operation": "capabilities", "requestId": "caps"})
            .to_string(),
    )
    .expect("request");
    let result = host.handle(request, &mut |_| Ok(())).expect("capabilities");
    let report = &result["capabilities"];
    assert_eq!(report["schema"], json!("alpha2-capabilities"));
    assert_eq!(report["version"], json!(2));
    assert_eq!(report["runtime"], json!("native"));
    assert!(matches!(report["gpu"]["tier"].as_str(), Some("a" | "b")));
    assert_eq!(report["color"]["workingSpace"], json!("linear_bt709_sdr"));
    assert_eq!(
        report["playback"]["defaultPolicy"],
        json!("drop_video_keep_audio")
    );
    assert_eq!(
        report["playback"]["policies"].as_array().map(Vec::len),
        Some(4)
    );
    let corpus: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/export-container-corpus.json"
    ))
    .expect("container corpus");
    let containers = corpus["containers"]
        .as_array()
        .expect("containers")
        .iter()
        .map(|entry| {
            json!({
                "id": entry["id"],
                "extensions": entry["extensions"],
                "video": entry["video"],
                "audio": entry["audio"],
                "defaultVideoCodec": entry["defaultVideoCodec"],
                "defaultAudioCodec": entry["defaultAudioCodec"],
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(report["encode"]["containers"], json!(containers));
    assert!(report["encode"].get("container").is_none());
    if alpha2_native::ffmpeg_runtime_identity()
        .pinned_sdk_mismatch()
        .is_none()
    {
        assert_eq!(
            report["encode"]["audio"],
            json!([
                {"id": "aac", "encoder": "aac"},
                {"id": "opus", "encoder": "libopus"}
            ])
        );
    }
    assert!(report["encode"]["gpuOutputPath"].is_object());
    assert_eq!(report["decode"]["mode"], json!("software"));
    // What the extensions crate was built with decides availability; this crate's `extensions`
    // feature turns every format on, and another package of the same build can as well.
    let providers = alpha2_extensions::Registry::with_defaults().names();
    for format in [
        "svg",
        "animated-image",
        "lottie",
        "exr",
        "latex",
        "pdf",
        "image-sequence",
    ] {
        let built = match format {
            "image-sequence" => alpha2_extensions::Registry::reads_image_sequences(),
            "latex" => alpha2_extensions::Registry::reads_latex(),
            _ => providers.contains(&format),
        };
        assert!(built || !cfg!(feature = "extensions"), "{format}");
        assert_eq!(
            report["features"][format!("extensions.{format}")]["available"],
            json!(built),
            "extensions.{format}"
        );
    }
    assert_eq!(
        report["features"]["proxyGeneration"]["available"],
        json!(false)
    );
    assert!(result.get("encodeCapabilities").is_none());
    assert!(result["effectCatalog"].is_object());
}

/// A child of this test binary running `serve` on its stdin, as `main` runs it.
pub(super) struct ServeChild {
    child: std::process::Child,
    pub(super) stdin: Option<std::process::ChildStdin>,
    messages: mpsc::Receiver<(Value, Vec<u8>)>,
}

impl ServeChild {
    pub(super) fn start(test: &str, env: &[(&str, &str)]) -> Self {
        use alpha2_host_core::host_frames::{FrameLimits, ReadFrame, read_frame, skip_to_magic};
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--exact",
                test,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("child");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = std::io::BufReader::new(stdout);
            // libtest writes the test's name before `serve` takes stdout.
            if !matches!(skip_to_magic(&mut stdout, 1 << 20), Ok(Some(_))) {
                return;
            }
            let limits = FrameLimits {
                header_bytes: u32::MAX,
                attachment_bytes: u32::MAX,
            };
            while let Ok(ReadFrame::Frame(frame)) = read_frame(&mut stdout, limits) {
                let header: Value =
                    serde_json::from_slice(&frame.header).expect("a header is JSON");
                if sender.send((header, frame.attachments)).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            messages,
        }
    }

    pub(super) fn message(&self, limit: Duration) -> Value {
        self.message_with_attachments(limit).0
    }

    /// A header and its attachments' bytes, not split.
    pub(super) fn message_with_attachments(&self, limit: Duration) -> (Value, Vec<u8>) {
        self.messages
            .recv_timeout(limit)
            .unwrap_or_else(|error| panic!("no message within {limit:?}: {error}"))
    }

    pub(super) fn write(&mut self, header: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        let frame =
            alpha2_host_core::host_frames::frame_bytes(header.as_bytes(), &[]).expect("frame");
        stdin.write_all(&frame).expect("write");
        stdin.flush().expect("flush");
    }

    /// The exit status if the child ends within `limit`.
    pub(super) fn exited_within(&mut self, limit: Duration) -> Option<std::process::ExitStatus> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                return Some(status);
            }
            if started.elapsed() > limit {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub(super) fn terminate(&self) {
        let killed = std::process::Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(killed.success());
    }

    pub(super) fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                return status.code();
            }
            if started.elapsed() > Duration::from_secs(60) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `ALPHA2_TEST_SLOW_ENCODE_MS` makes the first export's encoder take that long over each frame.
#[test]
#[ignore = "run by the tests that start a ServeChild"]
fn serve_standard_input() {
    if std::env::var_os("ALPHA2_TEST_STRAY_STDOUT").is_some() {
        // Native code that prints to stdout, before and after `serve` owns the stream.
        std::thread::spawn(|| {
            for _ in 0..200 {
                print!("stray {{\"protocolVersion\":2}}\n\0\x01\x02");
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        std::thread::sleep(Duration::from_millis(20));
    }
    if std::env::var_os("ALPHA2_TEST_REQUEST_PARK").is_some() {
        crate::compose_request::INJECT_REQUEST_PARK
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Ok(milliseconds) = std::env::var("ALPHA2_TEST_TERMINATION_GRACE_MS") {
        *crate::termination::TEST_GRACE.lock().expect("grace") =
            Some(Duration::from_millis(milliseconds.parse().expect("ms")));
    }
    if std::env::var_os("ALPHA2_TEST_REQUEST_PANIC").is_some() {
        crate::compose_request::INJECT_REQUEST_PANIC
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Ok(milliseconds) = std::env::var("ALPHA2_TEST_SLOW_ENCODE_MS") {
        let milliseconds = milliseconds.parse().expect("milliseconds");
        *crate::encode::SERVE_SLOW_ENCODE
            .lock()
            .expect("slow encode") = Some(Duration::from_millis(milliseconds));
    }
    let mut host = None;
    serve(&mut host, test_serve_limits()).expect("serve");
}

pub(super) const SERVE_CHILD: &str = "tests::requests::serve_standard_input";

/// A client that sees no ready line in time knows the process is held before `main` (a security
/// scan, an approval prompt) rather than busy.
#[test]
fn serve_writes_ready_before_reading_input() {
    let serving = ServeChild::start(SERVE_CHILD, &[]);
    let ready = serving.message(Duration::from_secs(30));
    assert_eq!(ready["type"], "state", "{ready}");
    assert_eq!(ready["state"], "ready", "{ready}");
    assert_eq!(ready["reason"], "started", "{ready}");
    assert_eq!(ready["details"]["requestsServed"], 0, "{ready}");
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn the_end_of_input_finishes_queued_requests_before_exiting() {
    let _gpu = gpu_test_lock();
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[("ALPHA2_TEST_SERVE_ARGS", "--max-running 2")],
    );
    serving.write(r#"{"protocolVersion":2,"requestId":"one","operation":"capabilities"}"#);
    serving.write(r#"{"protocolVersion":2,"requestId":"two","operation":"capabilities"}"#);
    drop(serving.stdin.take());
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    // Both run at once, so either may answer first.
    let mut answered = (0..2)
        .map(|_| {
            let answer = serving.message(Duration::from_secs(60));
            assert_eq!(answer["ok"], true, "{answer}");
            answer["requestId"].as_str().unwrap_or_default().to_owned()
        })
        .collect::<Vec<_>>();
    answered.sort();
    assert_eq!(answered, ["one", "two"]);
    assert_eq!(serving.finish(), Some(0));
}

const TEMPLATES: &str =
    r#"{"protocolVersion":2,"requestId":"REQUEST","operation":"compose","action":"templates"}"#;

fn templates(id: &str) -> String {
    TEMPLATES.replace("REQUEST", id)
}

#[test]
fn stray_writes_to_stdout_do_not_reach_the_protocol_stream() {
    let mut serving = ServeChild::start(SERVE_CHILD, &[("ALPHA2_TEST_STRAY_STDOUT", "1")]);
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    for index in 0..20 {
        let id = format!("templates-{index}");
        serving.write(&templates(&id));
        let answer = serving.message(Duration::from_secs(30));
        assert_eq!(answer["requestId"], id.as_str(), "{answer}");
        assert_eq!(answer["ok"], true, "{answer}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn each_soft_limit_drains_the_host_and_later_requests_are_retired() {
    for (flags, reason) in [
        ("--max-running 1 --max-requests 1", "requests"),
        ("--max-running 1 --max-memory-share 0.000001", "memory"),
        ("--max-running 1 --max-handles 1", "handles"),
    ] {
        let mut serving = ServeChild::start(SERVE_CHILD, &[("ALPHA2_TEST_SERVE_ARGS", flags)]);
        assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
        serving.write(&templates("first"));
        serving.write(&templates("second"));
        let state = serving.message(Duration::from_secs(30));
        assert_eq!(state["type"], "state", "{flags}: {state}");
        assert_eq!(state["state"], "draining", "{flags}: {state}");
        assert_eq!(state["reason"], reason, "{flags}: {state}");
        assert_eq!(state["details"]["requestsServed"], 1, "{flags}: {state}");
        let first = serving.message(Duration::from_secs(30));
        assert_eq!(first["requestId"], "first", "{flags}: {first}");
        assert_eq!(first["ok"], true, "{flags}: {first}");
        assert_eq!(first["hostState"], "draining", "{flags}: {first}");
        let second = serving.message(Duration::from_secs(30));
        assert_eq!(second["requestId"], "second", "{flags}: {second}");
        assert_eq!(
            second["error"]["code"], "HOST_RETIRING",
            "{flags}: {second}"
        );
        assert_eq!(second["hostState"], "draining", "{flags}: {second}");
        assert_eq!(serving.finish(), Some(0), "{flags}");
    }
}

#[test]
fn an_idle_host_drains_and_exits_on_its_own() {
    let serving = ServeChild::start(
        SERVE_CHILD,
        &[("ALPHA2_TEST_SERVE_ARGS", "--idle-timeout 100")],
    );
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    let state = serving.message(Duration::from_secs(30));
    assert_eq!(state["state"], "draining", "{state}");
    assert_eq!(state["reason"], "idle", "{state}");
    assert_eq!(state["details"]["activity"], "idle", "{state}");
    let mut serving = serving;
    serving.write(&templates("late"));
    let late = serving.message(Duration::from_secs(30));
    assert_eq!(late["requestId"], "late", "{late}");
    assert_eq!(late["error"]["code"], "HOST_RETIRING", "{late}");
    let started = Instant::now();
    let child = &mut serving.child;
    let status = loop {
        if let Some(status) = child.try_wait().expect("child status") {
            break status;
        }
        assert!(
            started.elapsed() < crate::lifecycle::DRAIN_GRACE + Duration::from_secs(30),
            "a draining host with open input outlived its grace"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
}

#[cfg(unix)]
#[test]
fn a_termination_while_idle_reports_draining_before_the_exit() {
    let mut serving = ServeChild::start(SERVE_CHILD, &[]);
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &serving.child.id().to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let state = serving.message(Duration::from_secs(30));
    assert_eq!(state["state"], "draining", "{state}");
    assert_eq!(state["reason"], "terminating", "{state}");
    drop(serving.stdin.take());
    assert_eq!(serving.child.wait().expect("exit").code(), Some(143));
}

#[test]
fn status_answers_at_once_while_an_export_runs() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("status-line");
    let output = directory.join("busy.mp4");
    let end = 4 * alpha2_time::TICKS_PER_SECOND;
    let export = json!({
        "protocolVersion": 2, "operation": "export", "requestId": "long",
        "project": {
            "schema_version": 1, "id": "cancel", "name": "Cancel",
            "settings": {"width": 320, "height": 180, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        },
        "assets": {"video": corpus("h264-long-gop.mp4")},
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "output": output
    });
    let mut serving = ServeChild::start(SERVE_CHILD, &[("ALPHA2_TEST_SLOW_ENCODE_MS", "100")]);
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    serving.write(&export.to_string());
    let progress = serving.message(Duration::from_secs(60));
    assert_eq!(progress["type"], "progress", "{progress}");
    serving.write(r#"{"protocolVersion":2,"requestId":"status","operation":"status"}"#);
    let status = std::iter::repeat_with(|| serving.message(Duration::from_secs(60)))
        .find(|message| message["type"] != "progress")
        .expect("an answer");
    assert_eq!(status["requestId"], "status", "{status}");
    assert_eq!(status["hostState"], "ready", "{status}");
    assert_eq!(status["result"]["state"], "ready", "{status}");
    assert_eq!(status["result"]["details"]["activity"], "busy", "{status}");
    assert!(
        ["gpu", "cpu"].contains(
            &status["result"]["details"]["renderer"]
                .as_str()
                .unwrap_or("")
        ),
        "{status}"
    );
    serving.write(
        r#"{"protocolVersion":2,"requestId":"long:cancel","operation":"cancel","target":"long"}"#,
    );
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn an_export_a_still_and_capabilities_run_at_once_on_one_host() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("concurrent-requests");
    let end = alpha2_time::TICKS_PER_SECOND;
    let export = json!({
        "protocolVersion": 2, "operation": "export", "requestId": "export",
        "project": {
            "schema_version": 1, "id": "cancel", "name": "Cancel",
            "settings": {"width": 320, "height": 180, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        },
                "assets": {"video": corpus("h264-long-gop.mp4")},
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "output": directory.join("slow.mp4")
    });
    let mut still = json!({
        "protocolVersion": 2, "operation": "still", "requestId": "still",
        "project": export["project"].clone(),
        "assets": export["assets"].clone(),
        "stills": [{"time": 0}],
    });
    still["size"] = json!({"width": 32, "height": 18});
    // 30 frames at 100 ms each keep the export running while the others are answered.
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[
            ("ALPHA2_TEST_SLOW_ENCODE_MS", "100"),
            ("ALPHA2_TEST_SERVE_ARGS", "--max-running 3"),
        ],
    );
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    // The first capabilities probes the encoders, which can outlast the export.
    serving.write(r#"{"protocolVersion":2,"requestId":"warm","operation":"capabilities"}"#);
    assert_eq!(
        serving.message(Duration::from_secs(120))["requestId"],
        "warm"
    );
    serving.write(&export.to_string());
    let progress = serving.message(Duration::from_secs(60));
    assert_eq!(progress["requestId"], "export", "{progress}");
    serving.write(&still.to_string());
    serving.write(r#"{"protocolVersion":2,"requestId":"caps","operation":"capabilities"}"#);
    let answers =
        std::iter::repeat_with(|| serving.message_with_attachments(Duration::from_secs(120)))
            .filter(|(message, _)| message["type"] != "progress")
            .take(3)
            .collect::<Vec<_>>();
    let order = answers
        .iter()
        .map(|(answer, _)| {
            assert_eq!(answer["ok"], true, "{answer}");
            answer["requestId"].as_str().unwrap_or_default().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        order.last().map(String::as_str),
        Some("export"),
        "{order:?}"
    );
    let (_, png) = answers
        .iter()
        .find(|(answer, _)| answer["requestId"] == "still")
        .expect("the still");
    assert!(
        png.starts_with(b"\x89PNG"),
        "the still's attachment is a PNG"
    );
    assert_eq!(serving.finish(), Some(0));
}

#[test]
fn concurrent_requests_hold_their_own_definition_of_one_effect() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let device = std::sync::Arc::clone(host.device());
    let effect = |body: &str| -> EffectRequest {
        serde_json::from_value(json!({
            "id": "test.isolated", "version": 1, "kind": "effect", "wgslBody": body
        }))
        .expect("an effect request")
    };
    let red = "fn effect(uv: vec2f, src: vec4f) -> vec4f { return vec4f(1.0, 0.0, 0.0, 1.0); }";
    let blue = "fn effect(uv: vec2f, src: vec4f) -> vec4f { return vec4f(0.0, 0.0, 1.0, 1.0); }";
    let id = alpha2_render::EffectId::new("test.isolated".to_owned(), 1);
    let body = |device: &crate::host::HostDevice| {
        device
            .renderer
            .gpu()
            .and_then(|renderer| renderer.effect_definition(&id))
            .map(|definition| definition.wgsl_body.clone())
    };
    // The CPU renderer refuses runtime effects but holds them the same way.
    let gpu = device.renderer.gpu().is_some();
    let mut first = Host::for_request(std::sync::Arc::clone(&device));
    first
        .register_request_effects(vec![effect(red)])
        .expect("first");
    let (sender, registered) = mpsc::channel();
    let waiting = {
        let device = std::sync::Arc::clone(&device);
        let effect = effect(blue);
        std::thread::spawn(move || {
            let mut second = Host::for_request(device);
            second
                .register_request_effects(vec![effect])
                .expect("second");
            sender.send(()).expect("send");
            second
        })
    };
    assert!(
        registered.recv_timeout(Duration::from_millis(300)).is_err(),
        "another definition waits while the first request holds its own"
    );
    let mut sharing = Host::for_request(std::sync::Arc::clone(&device));
    sharing
        .register_request_effects(vec![effect(red)])
        .expect("the same definition is shared at once");
    assert_eq!(body(&device).as_deref(), gpu.then_some(red));
    drop(first);
    assert!(registered.recv_timeout(Duration::from_millis(300)).is_err());
    drop(sharing);
    registered
        .recv_timeout(Duration::from_secs(30))
        .expect("registered once the first definition is released");
    let second = waiting.join().expect("join");
    assert_eq!(body(&device).as_deref(), gpu.then_some(blue));
    drop(second);
    let mut plain = Host::for_request(std::sync::Arc::clone(&device));
    plain.register_request_effects(Vec::new()).expect("none");
    assert_eq!(
        body(&device),
        None,
        "an effect nobody holds goes when a request wants others"
    );
}

#[test]
fn a_cancel_line_stops_the_running_export_and_the_host_serves_the_next_request() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cancel-line");
    let output = directory.join("cancelled.mp4");
    let end = 4 * alpha2_time::TICKS_PER_SECOND;
    let export = json!({
        "protocolVersion": 2, "operation": "export", "requestId": "long",
        "project": {
            "schema_version": 1, "id": "cancel", "name": "Cancel",
            "settings": {"width": 320, "height": 180, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        },
        "assets": {"video": corpus("h264-long-gop.mp4")},
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "output": output
    });
    // 120 frames at 100 ms each: far longer than the cancel takes to arrive.
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[
            ("ALPHA2_TEST_SLOW_ENCODE_MS", "100"),
            ("ALPHA2_TEST_SERVE_ARGS", "--max-running 2"),
        ],
    );
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    serving.write(&export.to_string());
    let progress = serving.message(Duration::from_secs(60));
    assert_eq!(progress["type"], "progress", "{progress}");
    assert_eq!(progress["requestId"], "long", "{progress}");
    serving.write(
        r#"{"protocolVersion":2,"requestId":"long:cancel","operation":"cancel","target":"long"}"#,
    );
    serving.write(r#"{"protocolVersion":2,"requestId":"next","operation":"capabilities"}"#);
    let mut answers: Vec<Value> =
        std::iter::repeat_with(|| serving.message(Duration::from_secs(60)))
            .filter(|message| message["type"] != "progress")
            .take(3)
            .collect();
    assert_eq!(
        answers[0],
        json!({
            "protocolVersion": 2, "requestId": "long:cancel", "ok": true,
            "result": {"target": "long", "state": "running"}, "hostState": "ready"
        })
    );
    // The next request runs beside the stopping export, so either may answer first.
    answers[1..].sort_by_key(|answer| answer["requestId"] != "long");
    assert_eq!(answers[1]["requestId"], "long", "{}", answers[1]);
    assert_eq!(answers[1]["error"]["code"], "CANCELLED", "{}", answers[1]);
    assert_eq!(
        answers[1]["error"]["failure"]["code"], "CANCELLED",
        "{}",
        answers[1]
    );
    assert!(
        answers[1]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("the client cancelled request long")),
        "{}",
        answers[1]
    );
    assert_eq!(answers[2]["requestId"], "next", "{}", answers[2]);
    assert_eq!(answers[2]["ok"], true, "{}", answers[2]);
    assert_eq!(serving.finish(), Some(0));
    let left: Vec<_> = std::fs::read_dir(&directory)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "the cancelled export left {left:?}");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_cancel_for_a_request_not_yet_read_answers_it_cancelled_without_running_it() {
    let input = [
        r#"{"protocolVersion":2,"requestId":"later:cancel","operation":"cancel","target":"later-request"}"#,
        r#"{"protocolVersion":2,"requestId":"later-request","operation":"capabilities"}"#,
    ]
    .join("\n");
    let mut serving = Serving::new(VideoDecodePolicy::Software);
    serving.create = no_gpu;
    let (served, output) = serve_input(&mut serving, input.as_bytes(), &|host| {
        assert!(host.is_none(), "a cancelled request made no host");
        None
    });
    served.expect("serve");
    let answers = answers_of(&output);
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert_eq!(answers[0]["result"]["state"], "pending", "{answers:?}");
    assert_eq!(answers[1]["requestId"], "later-request", "{answers:?}");
    assert_eq!(answers[1]["error"]["code"], "CANCELLED", "{answers:?}");
    assert!(answers[1]["error"].get("failure").is_none(), "{answers:?}");
}

#[test]
fn a_cancel_without_a_target_is_an_invalid_request() {
    let input = [
        r#"{"protocolVersion":2,"requestId":"no-target","operation":"cancel"}"#,
        r#"{"protocolVersion":2,"requestId":"empty-target","operation":"cancel","target":""}"#,
        r#"{"protocolVersion":2,"operation":"cancel","target":"something"}"#,
        r#"{"protocolVersion":3,"requestId":"version","operation":"cancel","target":"something"}"#,
    ]
    .join("\n");
    let mut serving = Serving::new(VideoDecodePolicy::Software);
    serving.create = no_gpu;
    let (served, output) = serve_input(&mut serving, input.as_bytes(), &|_| None);
    served.expect("serve");
    let summary: Vec<(Value, Value)> = answers_of(&output)
        .iter()
        .map(|answer| (answer["requestId"].clone(), answer["error"]["code"].clone()))
        .collect();
    assert_eq!(
        summary,
        [
            (json!("no-target"), json!("INVALID_REQUEST")),
            (json!("empty-target"), json!("INVALID_REQUEST")),
            (json!("unknown"), json!("INVALID_REQUEST")),
            (json!("version"), json!("UNSUPPORTED_PROTOCOL")),
        ]
    );
}

/// Windows gives its main thread 1 MiB, which three megabytes of frames overflow, twice that in
/// a debug build.
#[test]
fn a_command_runs_on_a_thread_with_room_for_deep_recursion() {
    #[allow(clippy::large_stack_arrays)]
    fn deep(levels: u8) -> u8 {
        let mut frame = [levels; 64 * 1024];
        std::hint::black_box(&mut frame);
        if levels == 0 {
            frame[0]
        } else {
            deep(levels - 1).wrapping_add(frame[1])
        }
    }
    // 1 + 2 + ... + 48 = 1176, which is 152 modulo 256.
    assert_eq!(on_command_thread(|| deep(48)).expect("no overflow"), 152);
}

/// A reader that decodes the output in a legacy code page reads the same characters back.
#[test]
fn every_line_the_host_writes_is_ascii_and_reads_back_the_same() {
    let value = json!({
        "output": "C:\\exports\\\u{d55c}\u{ad6d}\u{c5b4}-\u{1f3ac}.mp4",
        "message": "caf\u{e9} \"quoted\"\n\u{7f}",
    });
    let line = crate::serve_io::json_line(&value);
    assert!(line.is_ascii(), "{}", String::from_utf8_lossy(&line));
    let text = std::str::from_utf8(&line).expect("ASCII");
    assert!(
        text.contains(r"\ud55c\uad6d\uc5b4-\ud83c\udfac.mp4"),
        "{text}"
    );
    assert_eq!(serde_json::from_str::<Value>(text).expect("JSON"), value);
}

#[test]
fn a_request_that_panics_is_answered_and_the_host_serves_the_next() {
    let edit = json!({
        "protocolVersion": 2, "requestId": "P", "operation": "edit",
        "project": {
            "schema_version": 1, "id": "p", "name": "P",
            "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [], "timeline": {"tracks": []}
        },
        "edit": {"op": "remove_ranges", "ranges": []}
    });
    let compose =
        r#"{"protocolVersion":2,"requestId":"P","operation":"compose","action":"templates"}"#;
    for first in [compose.to_owned(), edit.to_string()] {
        let mut serving = ServeChild::start(SERVE_CHILD, &[("ALPHA2_TEST_REQUEST_PANIC", "1")]);
        assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
        serving.write(&first);
        let answer = serving.message(Duration::from_secs(10));
        assert_eq!(answer["requestId"], "P", "{answer}");
        assert_eq!(answer["error"]["code"], "INTERNAL", "{answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("injected panic")),
            "{answer}"
        );
        serving.write(r#"{"protocolVersion":2,"requestId":"st","operation":"status"}"#);
        let status = serving.message(Duration::from_secs(10));
        assert_eq!(
            status["result"]["details"]["runningRequests"], 0,
            "{status}"
        );
        assert_eq!(status["result"]["details"]["activity"], "idle", "{status}");
        serving.write(&templates("next"));
        let next = serving.message(Duration::from_secs(10));
        assert_eq!(next["requestId"], "next", "{next}");
        assert_eq!(next["ok"], true, "{next}");
        assert_eq!(serving.finish(), Some(0));
    }
}

#[test]
fn an_unknown_operation_names_every_operation_the_host_takes() {
    let error = parse_request(r#"{"protocolVersion":2,"requestId":"x","operation":"bogus"}"#)
        .err()
        .expect("refused");
    assert_eq!(error.code, "INVALID_REQUEST");
    for operation in [
        "capabilities",
        "export",
        "conform",
        "still",
        "compose",
        "edit",
        "extract",
        "cancel",
        "status",
    ] {
        assert!(
            error.message.contains(&format!("`{operation}`")),
            "{operation}: {}",
            error.message
        );
    }
}

/// An adapter whose intermediate is 8-bit refuses a 10-bit export with a code the protocol lists.
#[test]
fn ten_bit_output_on_an_eight_bit_renderer_is_invalid_input() {
    let _gpu = gpu_test_lock();
    crate::INJECT_RGBA8_INTERMEDIATE.with(|inject| inject.set(true));
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    assert_eq!(
        host.device().renderer.intermediate_format(),
        wgpu::TextureFormat::Rgba8Unorm
    );
    let directory = scratch_directory("ten-bit-on-eight");
    let end = alpha2_time::TICKS_PER_SECOND / 10;
    let request = json!({
        "protocolVersion": 2, "operation": "export", "requestId": "U",
        "project": {
            "schema_version": 1, "id": "p", "name": "P",
            "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [], "timeline": {"tracks": []}
        },
        "assets": {},
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "prores",
        "bitDepth": 10,
        "output": directory.join("out.mov")
    });
    let error = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |_| Ok(()),
        )
        .expect_err("refused");
    assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
}

#[test]
fn every_operation_refuses_an_unknown_field_and_takes_an_attachment_list() {
    let project = json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [], "timeline": {"tracks": []}
    });
    let requests = [
        json!({"protocolVersion": 2, "requestId": "r", "operation": "export", "project": project,
               "assets": {}, "startTicks": 0, "endTicks": 4000, "frameRate": {"num": 30, "den": 1},
               "output": "/nowhere/out.mp4"}),
        json!({"protocolVersion": 2, "requestId": "r", "operation": "conform", "project": project}),
        json!({"protocolVersion": 2, "requestId": "r", "operation": "compose", "action": "templates"}),
        json!({"protocolVersion": 2, "requestId": "r", "operation": "edit", "project": project,
               "edit": {"op": "split_at", "time": 0}}),
        json!({"protocolVersion": 2, "requestId": "r", "operation": "capabilities"}),
        json!({"protocolVersion": 2, "requestId": "r", "operation": "still", "project": project,
               "assets": {}, "stills": [{"time": 0}]}),
    ];
    let refusal = |result: Result<(), HostError>, what: &str| {
        let error = result.err().unwrap_or_else(|| panic!("{what}: accepted"));
        assert_eq!(error.code, "INVALID_REQUEST", "{what}: {}", error.message);
        assert!(
            error.message.contains("aUnknownField"),
            "{what}: {}",
            error.message
        );
    };
    for request in &requests {
        let operation = request["operation"].as_str().expect("operation");
        let mut with_attachments = request.clone();
        with_attachments["attachments"] = json!([]);
        assert!(
            parse_request(&with_attachments.to_string()).is_ok(),
            "{operation} with an attachment list"
        );
        let mut unknown = request.clone();
        unknown["aUnknownField"] = json!(true);
        refusal(parse_request(&unknown.to_string()).map(|_| ()), operation);
    }
    let cancel =
        json!({"protocolVersion": 2, "requestId": "c", "operation": "cancel", "target": "r"});
    let status = json!({"protocolVersion": 2, "requestId": "s", "operation": "status"});
    for (name, request) in [("cancel", &cancel), ("status", &status)] {
        let mut unknown = request.clone();
        unknown["aUnknownField"] = json!(true);
        let parse = |line: &str| {
            if name == "cancel" {
                crate::protocol::parse_cancel(line).map(|_| ())
            } else {
                crate::protocol::parse_status(line).map(|_| ())
            }
        };
        let mut with_attachments = request.clone();
        with_attachments["attachments"] = json!([]);
        assert!(parse(&with_attachments.to_string()).is_ok(), "{name}");
        refusal(parse(&unknown.to_string()), name);
    }
    for (field, value) in [
        ("includeaudio", json!(false)),
        ("overWrite", json!(true)),
        ("bitdepth", json!(10)),
        ("inputpolicy", json!("cpu-readback")),
        ("loudnes", json!({"targetLufs": -16.0})),
        ("rendercache", json!({"directory": "/nowhere"})),
        ("proxys", json!({})),
    ] {
        let mut request = requests[0].clone();
        request[field] = value;
        let error = parse_request(&request.to_string())
            .err()
            .unwrap_or_else(|| panic!("{field}: accepted"));
        assert_eq!(error.code, "INVALID_REQUEST", "{field}");
        assert!(error.message.contains(field), "{field}: {}", error.message);
    }
    let mut rate = requests[0].clone();
    rate["frameRate"]["denominator"] = json!(1);
    assert_eq!(
        parse_request(&rate.to_string())
            .err()
            .map(|error| error.code),
        Some("INVALID_REQUEST")
    );
}

/// A request thread that never returns cannot keep a terminated host alive past its grace.
#[cfg(unix)]
#[test]
fn a_terminated_host_whose_request_never_returns_exits_at_its_deadline() {
    let mut serving = ServeChild::start(
        SERVE_CHILD,
        &[
            ("ALPHA2_TEST_REQUEST_PARK", "1"),
            ("ALPHA2_TEST_TERMINATION_GRACE_MS", "1000"),
        ],
    );
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    serving.write(&templates("parked"));
    std::thread::sleep(Duration::from_millis(300));
    serving.terminate();
    let status = serving
        .exited_within(Duration::from_secs(10))
        .expect("the host outlived its deadline");
    assert_eq!(status.code(), Some(143));
}

/// A client that stops reading cannot hold the exit: the deadline writes nothing.
#[cfg(unix)]
#[test]
fn a_terminated_host_whose_output_nobody_reads_exits_at_its_deadline() {
    use std::io::Write as _;
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            SERVE_CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ALPHA2_TEST_TERMINATION_GRACE_MS", "1000")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("child");
    let mut stdin = child.stdin.take().expect("stdin");
    let frame = alpha2_host_core::host_frames::frame_bytes(
        br#"{"protocolVersion":2,"requestId":"st","operation":"status"}"#,
        &[],
    )
    .expect("frame");
    // Under a pipe's worth of requests, whose answers are far more than a pipe holds.
    for _ in 0..800 {
        if stdin.write_all(&frame).is_err() {
            break;
        }
    }
    let _ = stdin.flush();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("kill")
            .success()
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("child status") {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    assert_eq!(status.and_then(|status| status.code()), Some(143));
}
