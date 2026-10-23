//! Protocol requests, the CLI, and the serve loop: parsing, refusals, and how the host answers.

use super::*;

fn export_line(video_quality: Option<Value>) -> String {
    let mut request = json!({
        "protocolVersion": 1,
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
        Request::Capabilities { .. } | Request::Conform(_) => {
            unreachable!("an export line parses as an export")
        }
    }
}

/// An export request line whose project is `objects` small objects: the
/// shape that costs a JSON tree the most per byte.
fn line_with_a_project_of_small_objects(objects: usize) -> String {
    let mut line = String::from(
        r#"{"protocolVersion":1,"operation":"export","requestId":"wide","assets":{},"startTicks":0,"endTicks":1,"frameRate":{"num":30,"den":1},"output":"out.mp4","project":{"cues":["#,
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

/// Runs `test` of this binary in a child process whose standard error is a
/// pipe the parent has already closed, and returns the child's status and
/// standard output.
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    drop(child.stderr.take());
    let output = child.wait_with_output().expect("child output");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// The child half of `a_closed_stderr_does_not_end_the_host`.
#[test]
#[ignore = "run by a_closed_stderr_does_not_end_the_host with its standard error closed"]
fn start_a_host_and_export_with_stderr_closed() {
    // Its parent closes the read end right after the spawn; give it the time to, so every line
    // meets a closed pipe.
    std::thread::sleep(Duration::from_millis(200));
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    // A worker thread's diagnostics too: a thread that panicked on one would be reported as some
    // other failure.
    std::thread::spawn(|| alpha2_os::diagnostic!("alpha2-host test: a worker's line"))
        .join()
        .expect("a worker survives a closed stderr");
    drop(host);
}

/// A host whose client stopped reading its standard error (closed the pipe, or
/// a timeout closed it first) drops its diagnostic lines. A panic on one would
/// exit the main thread 101 mid-export, or kill a worker thread and report its
/// failure as another.
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

/// Runs `test`, one of the hung-release halves below, in a child of this
/// test binary whose GPU waits (the release's deadline) are 1 s, with
/// `input` as its standard input, which then ends. Returns its exit status
/// (`None` when it had to be killed: it hung) and its standard output and
/// error.
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
    stdin.write_all(input.as_bytes()).expect("write the input");
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
            let mut output = String::new();
            let _ = pipe.read_to_string(&mut output);
            output
        })
    });
    // Startup, the command and the release take a few seconds; a minute is far past all of them.
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

/// Asserts that `stderr` reports the release that did not finish, after
/// `outcome`, the line the command's outcome was reported with.
fn assert_reported_before_the_release(stderr: &str, outcome: &str) {
    let reported = stderr.find(outcome);
    let released = stderr.find("releasing the GPU device took longer than 1000 ms");
    assert!(
        matches!((reported, released), (Some(reported), Some(released)) if reported < released),
        "{stderr}"
    );
}

/// A CLI export prints its error before it drops its GPU host, and the drop is
/// bounded. The drop waits for the device to go idle (`wgpu`'s queue drop calls
/// `vkQueueWaitIdle`, which has no timeout), so on a device that stopped
/// completing work it never returns, and the caller would see neither the error
/// nor an exit.
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

/// The child half of
/// `a_cli_export_whose_device_never_goes_idle_reports_its_error_and_exits`:
/// an export of a project whose source is missing, which fails after the
/// host has started, concluded as `main` concludes it.
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

/// `serve` drops its GPU host when its input ends, before exiting, and a device
/// that never goes idle holds that drop in the same wait. Every answer is out
/// by then; the release is bounded and the process exits as usual.
#[test]
fn serve_exits_at_the_end_of_its_input_when_its_device_never_goes_idle() {
    let _gpu = gpu_test_lock();
    let (status, stdout, stderr) = run_with_a_hung_release(
        "tests::requests::serve_with_a_hung_release",
        "{\"protocolVersion\":1,\"requestId\":\"caps\",\"operation\":\"capabilities\"}\n",
    );
    assert_eq!(status, Some(0), "{stderr}");
    assert!(
        stdout.contains(r#""requestId":"caps""#) && stdout.contains(r#""ok":true"#),
        "{stdout}"
    );
    assert_reported_before_the_release(&stderr, "alpha2-host MP4 encoders:");
}

/// The child half of
/// `serve_exits_at_the_end_of_its_input_when_its_device_never_goes_idle`:
/// `serve` on this process's standard input, concluded as `main` concludes
/// it.
#[test]
#[ignore = "run by serve_exits_at_the_end_of_its_input_when_its_device_never_goes_idle"]
fn serve_with_a_hung_release() {
    crate::teardown::injected::stall_host_drops();
    let mut host = None;
    let result = serve(&mut host);
    assert!(host.is_some(), "the request started no host");
    let code = conclude(result, host);
    panic!("the release returned (status {code}) although it never finishes");
}

/// A command-line argument that is not UTF-8 (a Linux file name in a legacy
/// encoding) is `INVALID_INPUT` naming the argument, not a CLI panic with exit
/// 101 and no `alpha2-host:` line.
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

/// The child half of `a_request_is_read_without_a_tree_of_its_project`, which
/// runs it with its heap limited.
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
    assert_eq!(request_id_from_line(&line), "wide");
}

/// `serve` parses a request line into its fields' types and keeps the project
/// as text for the engine, so no JSON tree of the line is built before the
/// engine's project budget sees the project: as a tree, a 15 MiB project of
/// small objects takes over a gigabyte. The child parses one with its heap
/// limited to 256 MiB.
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

/// A line longer than `serve` reads is answered and skipped through its end;
/// the request after it is read whole.
#[test]
fn a_request_line_over_the_limit_is_skipped_and_the_next_is_read_whole() {
    let mut bytes = "x".repeat(100).into_bytes();
    bytes.extend_from_slice(b"\n{\"next\":1}\r\n\n\xFF\n{\"last\":true}");
    let mut input = std::io::BufReader::with_capacity(7, bytes.as_slice());
    let mut buffer = Vec::new();
    let mut lines = Vec::new();
    loop {
        let line = match read_request_line(&mut input, 64, &mut buffer).expect("read") {
            RequestLine::End => break,
            RequestLine::Line(line) => format!("line {line}"),
            RequestLine::TooLong(bytes) => format!("too long {bytes}"),
            RequestLine::NotUtf8 => "not utf-8".to_owned(),
        };
        lines.push(line);
    }
    assert_eq!(
        lines,
        [
            "too long 100",
            r#"line {"next":1}"#,
            "line ",
            "not utf-8",
            r#"line {"last":true}"#,
        ]
    );
}

/// `serve` answers a line longer than it reads, and a project over the
/// engine's budget, each with its own failure, and the next request as usual.
#[test]
fn serve_answers_oversized_requests_and_then_the_next() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = |id: &str, operation: &str, project: Option<String>| {
        let mut line =
            format!(r#"{{"protocolVersion":1,"requestId":"{id}","operation":"{operation}""#);
        if let Some(project) = project {
            line.push_str(r#","project":"#);
            line.push_str(&project);
        }
        line.push_str("}\n");
        line
    };
    // Valid JSON, and a project the engine counts as one byte over its budget.
    let over_budget = format!(
        r#"{{"padding":"{}"}}"#,
        "x".repeat(alpha2_model::MAX_PROJECT_JSON_BYTES - 13)
    );
    assert_eq!(over_budget.len(), alpha2_model::MAX_PROJECT_JSON_BYTES + 1);
    let mut input = String::new();
    input.push_str(&request(
        "too-long",
        "conform",
        Some(format!(r#""{}""#, "x".repeat(MAX_REQUEST_LINE_BYTES))),
    ));
    input.push_str(&request("over-budget", "conform", Some(over_budget)));
    input.push_str(&request("next", "capabilities", None));
    let mut output = Vec::new();
    serve_requests(
        &mut Serving::started(host),
        &mut std::io::BufReader::new(input.as_bytes()),
        &mut output,
        // Other tests leave decode threads behind on purpose; `serve` would exit for them.
        &mut |_| {},
    )
    .expect("serve");
    let answers: Vec<Value> = String::from_utf8(output)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("an answer is JSON"))
        .collect();
    assert_eq!(answers.len(), 3, "{answers:?}");
    assert_eq!(answers[0]["requestId"], "unknown");
    assert_eq!(answers[0]["error"]["code"], "INVALID_REQUEST");
    assert!(
        answers[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("the request line is")),
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

/// `conform` is answered without a GPU, as the documentation says: the GPU
/// host, and under `hardware-required` the decode device, is made for the first
/// request that needs it, not before `serve` reads any request. When it cannot
/// be made, that request gets the `fatal` notification a failed start writes.
#[test]
fn serve_answers_conform_without_a_gpu_and_reports_a_failed_start_on_the_first_request_needing_one()
{
    let clip = corpus("parity-rgb.mov");
    let conform = json!({
        "protocolVersion": 1, "requestId": "conform", "operation": "conform",
        "project": conform_project(&json!({})),
        "assets": {"video": clip}
    });
    let input = format!(
        "{conform}\n{}\n{}\n",
        r#"{"protocolVersion":1,"requestId":"caps","operation":"capabilities"}"#,
        r#"{"protocolVersion":1,"requestId":"after","operation":"capabilities"}"#
    );
    for policy in [
        VideoDecodePolicy::Software,
        VideoDecodePolicy::HardwareRequired,
    ] {
        let mut serving = Serving::new(policy);
        serving.create = no_gpu;
        let mut output = Vec::new();
        let mut answered = 0;
        let error = serve_requests(
            &mut serving,
            &mut std::io::BufReader::new(input.as_bytes()),
            &mut output,
            &mut |host| {
                assert!(host.is_none(), "conform made no host");
                answered += 1;
            },
        )
        .expect_err("capabilities needs the GPU host");
        assert_eq!(error.code, "GPU_INIT_FAILED");
        assert_eq!(answered, 1);
        let answers = answers_of(output);
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

/// A composition scale with a zero term is a project the engine refuses, so
/// `serve` answers it `INVALID_INPUT` as it does any invalid document, not
/// `INTERNAL` from a panic inside validation.
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
            "protocolVersion": 1, "requestId": id, "operation": "conform",
            "project": project, "assets": {}
        });
        input.push_str(&request.to_string());
        input.push('\n');
    }
    let mut serving = Serving::new(VideoDecodePolicy::Software);
    serving.create = no_gpu;
    let mut output = Vec::new();
    serve_requests(
        &mut serving,
        &mut std::io::BufReader::new(input.as_bytes()),
        &mut output,
        &mut |host| assert!(host.is_none(), "conform made no host"),
    )
    .expect("serve");
    let answers = answers_of(output);
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

/// The serve loop's edges: a refused line is answered by the `requestId` it
/// names (`unknown` when it names none or an empty one), an empty `requestId`
/// is refused, blank lines are skipped, a line that is not UTF-8 is refused
/// without ending `serve`, and the requests pipelined after all of them are
/// answered in order.
#[test]
fn serve_answers_each_edge_line_by_its_id_and_serves_the_rest() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let mut input = Vec::new();
    for line in [
        &br#"{"protocolVersion":1,"requestId":"first","operation":"capabilities"}"#[..],
        br#"{"protocolVersion":1,"requestId":"bad-operation","operation":"render"}"#,
        br#"{"protocolVersion":1,"requestId":"truncated","operation":"capabilities","#,
        b"",
        b"   ",
        br#"{"protocolVersion":1,"requestId":"","operation":"capabilities"}"#,
        b"{\"protocolVersion\":1,\"requestId\":\"latin-1\",\"path\":\"caf\xe9\"}",
        b"not JSON",
        br#"{"protocolVersion":1,"requestId":"last","operation":"capabilities"}"#,
    ] {
        input.extend_from_slice(line);
        input.push(b'\n');
    }
    let mut output = Vec::new();
    let mut answered = 0;
    serve_requests(
        &mut Serving::started(host),
        &mut std::io::BufReader::new(input.as_slice()),
        &mut output,
        &mut |_| answered += 1,
    )
    .expect("serve");
    let answers = answers_of(output);
    assert_eq!(answered, answers.len());
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

/// A loss `wgpu` reported retires the host. The renderer learns of it only at
/// its next submission, so otherwise a readback that failed to map on the lost
/// device is answered and every export after it runs conform, opens its output
/// and fails on the dead device.
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

/// The child half of `a_host_that_cannot_take_another_request_exits_after_answering`.
#[test]
#[ignore = "run by a_host_that_cannot_take_another_request_exits_after_answering"]
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
        r#"{"protocolVersion":1,"requestId":"first","operation":"capabilities"}"#,
        r#"{"protocolVersion":1,"requestId":"second","operation":"capabilities"}"#,
    ]
    .join("\n");
    serve_requests(
        &mut Serving::started(host),
        &mut std::io::BufReader::new(input.as_bytes()),
        &mut std::io::stdout().lock(),
        &mut exit_if_unusable,
    )
    .expect("serve");
    panic!("the host answered every request instead of exiting after the first");
}

/// A host whose decode or encode threads are stuck, or whose device cannot
/// run work, answers the request it is on and then exits with status 1; a
/// request pipelined behind it is not read and gets no answer.
#[test]
fn a_host_that_cannot_take_another_request_exits_after_answering() {
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
        let answers: Vec<Value> = stdout
            .lines()
            .filter_map(|line| line.find("{\"").map(|at| &line[at..]))
            .map(|line| serde_json::from_str(line).expect("an answer is JSON"))
            .collect();
        assert_eq!(answers.len(), 1, "{unusable}\n{stdout}");
        assert_eq!(answers[0]["requestId"], "first", "{unusable}\n{stdout}");
        assert_eq!(answers[0]["ok"], true, "{unusable}\n{stdout}");
    }
}

/// The child half of `an_unknown_decode_mode_is_a_fatal_invalid_input`.
#[test]
#[ignore = "run by an_unknown_decode_mode_is_a_fatal_invalid_input with ALPHA2_VIDEO_DECODE set"]
fn serve_with_an_unknown_decode_mode() {
    let error = serve(&mut None).expect_err("an unknown mode");
    assert_eq!(error.code, "INVALID_INPUT");
}

/// An unknown `ALPHA2_VIDEO_DECODE` reaches the client as a `fatal` line with
/// the documented `INVALID_INPUT`, not only as an exit of 1 before the start is
/// reported.
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 passed"), "{stdout}");
    let fatal: Value = stdout
        .lines()
        // libtest writes the test's name on the same line first.
        .find_map(|line| line.find("{\"").map(|at| &line[at..]))
        .map_or_else(
            || panic!("no fatal notification: {stdout}"),
            |line| serde_json::from_str(line).expect("JSON"),
        );
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

/// An export the host refuses before it runs (a line it cannot read, a
/// protocol it does not speak, a setting out of range) answers with a
/// failure report, classified by its code as a running export's would be; a
/// refused request of another operation does not. A start that failed does
/// too.
#[test]
fn a_refused_export_and_a_failed_start_carry_a_failure_report() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Software).expect("host");
    let lines = [
        r#"{"protocolVersion":1,"requestId":"json","operation":"export","#.to_owned(),
        r#"{"protocolVersion":9,"requestId":"version","operation":"export"}"#.to_owned(),
        r#"{"protocolVersion":1,"requestId":"quality","operation":"export","videoQuality":{"bitrate":-1}}"#.to_owned(),
        r#"{"protocolVersion":1,"requestId":"conform","operation":"conform"}"#.to_owned(),
        format!(
            r#"{{"protocolVersion":1,"requestId":"long","operation":"export","project":"{}"}}"#,
            "x".repeat(MAX_REQUEST_LINE_BYTES)
        ),
        r#"{"protocolVersion":1,"requestId":"next","operation":"capabilities"}"#.to_owned(),
    ];
    let input = lines.join("\n") + "\n";
    let mut output = Vec::new();
    serve_requests(
        &mut Serving::started(host),
        &mut std::io::BufReader::new(input.as_bytes()),
        &mut output,
        &mut |_| {},
    )
    .expect("serve");
    let answers: Vec<Value> = String::from_utf8(output)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("an answer is JSON"))
        .collect();
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

/// A request that leaves `includeAudio` out exports the project's audio, as
/// the CLI, @alpha2-sh/node, and the browser exporter do; only `false` leaves it
/// out.
#[test]
fn an_export_includes_audio_unless_the_request_says_not_to() {
    let include_audio = |value: Option<bool>| {
        let mut request: Value = serde_json::from_str(&export_line(None)).expect("line");
        if let Some(value) = value {
            request["includeAudio"] = json!(value);
        }
        match parse_request(&request.to_string()).expect("export") {
            Request::Export(export) => export.include_audio,
            Request::Capabilities { .. } | Request::Conform(_) => {
                unreachable!("an export line parses as an export")
            }
        }
    };
    assert!(include_audio(None));
    assert!(include_audio(Some(true)));
    assert!(!include_audio(Some(false)));
}

/// A malformed `memory` is the caller's input, as the protocol says, named in
/// an `INVALID_INPUT`: a misspelled field, a value of the wrong type, or no
/// object at all, not an `INVALID_REQUEST`.
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

/// The one-shot CLI's export request is checked as a protocol request is,
/// before a device or an encoder opens: loudness without the audio it
/// normalizes, and an empty render-cache directory (which would bake into
/// `./v1`), are refused rather than failing late or not at all.
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
            Request::Capabilities { .. } | Request::Conform(_) => {
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
        let parsed = parse_request(&line);
        if fixture["requestValid"].as_bool() == Some(true) {
            let request = parsed.unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(request.protocol_version(), PROTOCOL_VERSION, "{name}");
        } else {
            let error = parsed
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
            if message["type"] == "progress" {
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

/// An export request line with `fields` set over a minimal one.
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

/// The container and codecs an export request resolves to, or its refusal.
fn resolved_format(fields: &Value) -> Result<crate::protocol::ResolvedFormat, HostError> {
    match parse_request(&format_line(fields))? {
        Request::Export(export) => export.format(),
        Request::Capabilities { .. } | Request::Conform(_) => {
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
    // A request that names no codec gets the container's defaults: H.264 and AAC for MP4.
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
    // A refused format is refused when the request is parsed, before anything opens.
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
                Request::Capabilities { .. } | Request::Conform(_) => unreachable!("an export"),
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
fn capabilities_are_the_shared_capability_report() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = parse_request(
        &json!({"protocolVersion": 1, "operation": "capabilities", "requestId": "caps"})
            .to_string(),
    )
    .expect("request");
    let result = host.handle(request, &mut |_| Ok(())).expect("capabilities");
    let report = &result["capabilities"];
    assert_eq!(report["schema"], json!("alpha2-capabilities"));
    assert_eq!(report["version"], json!(1));
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
    // The containers are the engine's policy, as the shared corpus pins it.
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
    // Every extension format is the build's to answer, image sequences included: a build without
    // `extensions` reads none of them.
    for format in ["svg", "animated-image", "lottie", "exr", "image-sequence"] {
        assert_eq!(
            report["features"][format!("extensions.{format}")]["available"],
            json!(cfg!(feature = "extensions")),
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
