use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alpha2_engine::extract::DriveError;
use alpha2_media::CancelToken;
use serde_json::{Value, json};

use super::tests::{
    Local, built, descriptor_of, loaded, loaded_with, manifest, native_allowed, run, scratch,
    script, supplied_words,
};
use super::wire::{self, child, host};
use super::*;

/// Set in the environment of the test binary when it runs as a plugin worker.
const WORKER: &str = "ALPHA2_TEST_PLUGIN_WORKER";
/// Set to a mode of [`lying_worker`] when the test binary runs as one.
const LYING_WORKER: &str = "ALPHA2_TEST_LYING_WORKER";

#[test]
#[ignore = "the plugin worker the isolation tests start"]
fn run_as_worker() {
    if std::env::var_os(WORKER).is_some() {
        worker::main();
    }
}

#[test]
#[ignore = "a plugin worker that breaks the wire, which the isolation tests start"]
fn run_as_lying_worker() {
    if let Some(mode) = std::env::var_os(LYING_WORKER) {
        lying_worker(&mode.to_string_lossy());
    }
}

fn worker_command(test: &str, envs: &[(&str, &str)]) -> WorkerCommand {
    WorkerCommand {
        program: std::env::current_exe().expect("test binary"),
        args: [
            "--exact",
            &format!("plugins::process_tests::{test}"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(Into::into)
        .to_vec(),
        envs: envs
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        frames: FramePath::Shared,
    }
}

fn isolated() -> Policy {
    Policy {
        isolation: Isolation::Child(worker_command("run_as_worker", &[(WORKER, "1")])),
        ..native_allowed()
    }
}

fn with_deadline(policy: Policy, deadline: Duration) -> Policy {
    Policy {
        call_deadline: deadline,
        ..policy
    }
}

fn failure(result: Result<Vec<alpha2_model::DataSet>, DriveError>) -> ExtractorError {
    match result {
        Err(DriveError::Extractor(error)) => error,
        other => panic!("expected an extractor failure, got {other:?}"),
    }
}

fn scripted_failure(params: &Value, policy: &Policy) -> ExtractorError {
    let mut plugin = loaded(&manifest(1, false, true), params, policy).expect("loads");
    failure(run(&mut plugin, params))
}

fn detail(error: &ExtractorError, key: &str) -> Option<Value> {
    error.details.as_ref()?.get(key).cloned()
}

#[test]
fn a_worker_gives_the_data_the_same_plugin_gives_in_process() {
    for plugin in [0, 1] {
        let params = if plugin == 1 { script() } else { json!({}) };
        let mut here =
            loaded(&manifest(plugin, false, true), &params, &native_allowed()).expect("in process");
        let mut there =
            loaded(&manifest(plugin, false, true), &params, &isolated()).expect("isolated");
        assert!(there.calls_are_bounded() && !here.calls_are_bounded());
        assert_eq!(
            run(&mut there, &params).expect("isolated run"),
            run(&mut here, &params).expect("in-process run"),
            "plugin {plugin}"
        );
    }
}

#[test]
fn a_crash_or_an_abort_fails_the_run_and_names_how_the_worker_ended() {
    for key in ["segv_at", "abort_at"] {
        let error = scripted_failure(&json!({ key: 1 }), &isolated());
        assert_eq!(error.code, Some(codes::TRAPPED), "{key}: {error:?}");
        #[cfg(unix)]
        assert_eq!(
            detail(&error, "signal"),
            Some(json!(if key == "segv_at" { 11 } else { 6 })),
            "{key}: {error:?}"
        );
        #[cfg(windows)]
        assert!(detail(&error, "exit").is_some(), "{key}: {error:?}");
    }
}

#[test]
fn a_hung_call_ends_at_the_deadline_and_a_cancel_ends_it_at_once() {
    let started = Instant::now();
    let error = scripted_failure(
        &json!({ "sleep_at": 1 }),
        &with_deadline(isolated(), Duration::from_secs(2)),
    );
    assert_eq!(error.code, Some(codes::LIMIT), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(30));

    let params = json!({ "sleep_at": 1 });
    let cancel = CancelToken::default();
    let parsed = parse_manifest(&manifest(1, false, true).to_string()).expect("manifest");
    let mut plugin = load(
        LoadRequest {
            manifest: &parsed,
            manifest_sha256: super::tests::MANIFEST_SHA256,
            params: params.as_object().expect("params"),
            supplied: &BTreeMap::new(),
        },
        &isolated(),
        &mut Local,
        &cancel,
    )
    .expect("loads");
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.cancel();
        })
    };
    let error = failure(run(&mut plugin, &params));
    canceller.join().expect("canceller");
    assert_eq!(error.code, Some(CANCELLED), "{error:?}");
}

#[test]
fn text_a_plugin_prints_to_standard_output_leaves_the_run_intact() {
    let params = json!({ "print_at": 0, "per_delivery": script()["per_delivery"], "at_end": script()["at_end"] });
    let mut here =
        loaded(&manifest(1, false, true), &params, &native_allowed()).expect("in process");
    let mut there = loaded(&manifest(1, false, true), &params, &isolated()).expect("isolated");
    assert_eq!(
        run(&mut there, &params).expect("isolated"),
        run(&mut here, &params).expect("in process")
    );
}

#[test]
fn supplied_files_read_in_a_worker_reach_the_data_and_its_failures_keep_their_details() {
    let dir = scratch("isolated-supplied");
    let model = dir.join("model.bin");
    std::fs::write(&model, b"\x01\x02weights").expect("model");
    std::fs::create_dir_all(dir.join("extra")).expect("dir");
    std::fs::write(dir.join("extra/big"), b"123456").expect("file");
    let supplied = BTreeMap::from([
        ("model".to_owned(), model.clone()),
        ("extra".to_owned(), dir.join("extra")),
    ]);
    let policy = Policy {
        resource_root: ResourceRoot::Any,
        ..isolated()
    };
    let params = json!({ "read": ["model"], "paths": ["extra"] });
    let (words, made_by) = supplied_words(&policy, &supplied, &params).expect("isolated");
    assert_eq!(words[0], "model 9 0102776569676874");
    assert_eq!(
        words[1],
        format!(
            "path extra {}",
            resources::canonical(&dir.join("extra"))
                .expect("canonical")
                .display()
        )
    );
    assert_eq!(
        made_by
            .supplied
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        ["extra/big", "model"]
    );

    let mut capped = manifest(2, false, true);
    capped["resources"][1]["supplied"]["max_bytes"] = json!(4);
    let params = json!({ "read": ["extra/big"] });
    let mut plugin = loaded_with(&capped, &params, &supplied, &policy).expect("loads");
    let error = failure(run(&mut plugin, &params));
    assert_eq!(error.code, Some(codes::RESOURCE_INVALID), "{error:?}");
    assert_eq!(detail(&error, "reason"), Some(json!("too_large")));
}

/// A fetcher whose native copy is not the checked file.
struct Tampered(PathBuf);

impl Fetch for Tampered {
    fn fetch(&mut self, url: &str, sha256: &str, bytes: u64) -> Result<Fetched, PluginError> {
        Local.fetch(url, sha256, bytes)
    }

    fn prepare_native(&mut self, _: &Path, bytes: &[u8], _: &str) -> Result<PathBuf, PluginError> {
        let mut changed = bytes.to_vec();
        changed.push(0);
        std::fs::write(&self.0, changed).expect("copy");
        Ok(self.0.clone())
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_worker_refuses_a_library_that_is_not_the_pinned_artifact() {
    let copy = scratch("isolated-tampered").join(built().native[1].file_name().expect("name"));
    let parsed = parse_manifest(&manifest(1, false, true).to_string()).expect("manifest");
    let refused = load(
        LoadRequest {
            manifest: &parsed,
            manifest_sha256: super::tests::MANIFEST_SHA256,
            params: &serde_json::Map::new(),
            supplied: &BTreeMap::new(),
        },
        &isolated(),
        &mut Tampered(copy),
        &CancelToken::default(),
    )
    .expect_err("the copy differs");
    assert_eq!(refused.code, codes::HASH_MISMATCH, "{refused:?}");
}

#[cfg(unix)]
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn gone_within(pid: &str, wait: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < wait {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(unix)]
#[test]
fn nothing_the_plugin_started_survives_the_kill_at_the_deadline() {
    let pid_file = scratch("isolated-spawn").join("pid");
    let error = scripted_failure(
        &json!({ "spawn_at": { "delivery": 0, "pid_file": pid_file } }),
        &with_deadline(isolated(), Duration::from_secs(2)),
    );
    assert_eq!(error.code, Some(codes::LIMIT), "{error:?}");
    let pid = std::fs::read_to_string(&pid_file).expect("the plugin started a process");
    assert!(
        gone_within(&pid, Duration::from_secs(5)),
        "process {pid} outlived its worker"
    );
}

#[cfg(unix)]
#[test]
fn nothing_the_plugin_started_survives_its_worker_crashing_or_ending() {
    for then in ["abort", "return"] {
        let pid_file = scratch(&format!("isolated-spawn-{then}")).join("pid");
        let params = json!({ "spawn_at": { "delivery": 0, "pid_file": pid_file, "then": then } });
        let mut plugin = loaded(&manifest(1, false, true), &params, &isolated()).expect("loads");
        let outcome = run(&mut plugin, &params);
        drop(plugin);
        match then {
            "abort" => assert_eq!(failure(outcome).code, Some(codes::TRAPPED)),
            _ => drop(outcome.expect("the run ends")),
        }
        let pid = std::fs::read_to_string(&pid_file).expect("the plugin started a process");
        assert!(
            gone_within(&pid, Duration::from_secs(5)),
            "process {pid} outlived its worker ({then})"
        );
    }
}

#[test]
#[ignore = "a host the isolation tests kill while its worker runs"]
fn run_a_host_whose_worker_sleeps() {
    let Ok(report) = std::env::var("ALPHA2_TEST_WORKER_REPORT") else {
        return;
    };
    let params = json!({ "report": report, "sleep_at": 0 });
    let mut plugin = loaded(&manifest(1, false, true), &params, &isolated()).expect("loads");
    let _ = run(&mut plugin, &params);
}

#[cfg(unix)]
#[test]
fn a_worker_whose_host_was_killed_ends() {
    let report = scratch("isolated-orphan").join("report");
    let _ = std::fs::remove_file(&report);
    let mut host = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "plugins::process_tests::run_a_host_whose_worker_sleeps",
            "--ignored",
            "--test-threads=1",
        ])
        .env("ALPHA2_TEST_WORKER_REPORT", &report)
        .env("CARGO_TERM_QUIET", "true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("host");
    let started = Instant::now();
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&report)
            && let Some(pid) = text.lines().next().filter(|pid| !pid.is_empty())
        {
            break pid.to_owned();
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the worker never reported"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    host.kill().expect("kill the host");
    host.wait().expect("reap the host");
    assert!(
        gone_within(&pid, Duration::from_secs(5)),
        "worker {pid} outlived its host"
    );
}

/// The descriptors an isolated scripted run's worker holds at `begin`.
#[cfg(unix)]
fn worker_descriptors(name: &str) -> Vec<i32> {
    let report = scratch(name).join("report");
    let params = json!({ "report": report });
    let mut plugin = loaded(&manifest(1, false, true), &params, &isolated()).expect("loads");
    run(&mut plugin, &params).expect("runs");
    std::fs::read_to_string(&report)
        .expect("report")
        .lines()
        .skip(1)
        .filter_map(|line| line.parse().ok())
        .collect()
}

#[cfg(unix)]
#[test]
#[expect(unsafe_code, reason = "clears close-on-exec with fcntl")]
fn a_worker_holds_none_of_the_descriptors_it_inherited() {
    use std::os::fd::AsRawFd;
    let baseline = worker_descriptors("isolated-descriptors");
    let leaked = std::fs::File::open(built().native[1].clone()).expect("a file");
    // SAFETY: `leaked` is open for the call.
    let flags = unsafe { libc::fcntl(leaked.as_raw_fd(), libc::F_GETFD) };
    // SAFETY: `leaked` is open for the call.
    unsafe { libc::fcntl(leaked.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    let with_leak = worker_descriptors("isolated-descriptors-leaked");
    drop(leaked);
    assert_eq!(
        with_leak.len(),
        baseline.len(),
        "{with_leak:?} {baseline:?}"
    );
}

#[test]
#[ignore = "runs an isolated plugin that floods standard error; run by the budget test"]
fn flood_standard_error_through_a_worker() {
    if std::env::var_os("ALPHA2_TEST_FLOOD").is_none() {
        return;
    }
    let params = json!({ "stderr_lines": 100_000 });
    let mut plugin = loaded(&manifest(1, false, true), &params, &isolated()).expect("loads");
    run(&mut plugin, &params).expect("the run ends while standard error floods");
}

#[test]
fn a_worker_flooding_standard_error_finishes_and_its_host_writes_at_most_the_budget() {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "plugins::process_tests::flood_standard_error_through_a_worker",
            "--ignored",
            "--test-threads=1",
        ])
        .env("ALPHA2_TEST_FLOOD", "1")
        .env("CARGO_TERM_QUIET", "true")
        .output()
        .expect("child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{}\n{stdout}",
        output.status
    );
    let plugin_lines = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.starts_with("plugin: "))
        .map(str::len)
        .sum::<usize>();
    assert!(
        plugin_lines < 80 * 1024,
        "{plugin_lines} bytes of plugin diagnostics"
    );
}

#[cfg(unix)]
fn end_stream(output: std::fs::File) {
    let socket = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(output));
    socket
        .shutdown(std::net::Shutdown::Write)
        .expect("shut the stream");
}

#[cfg(not(unix))]
fn end_stream(output: std::fs::File) {
    drop(output);
}

/// A worker that speaks the wire honestly for the scripted fixture until its `mode` breaks it.
fn lying_worker(mode: &str) {
    let input = alpha2_os::claim_stdin().expect("stdin");
    let mut output = alpha2_os::claim_stdout().expect("stdout");
    let send = |output: &mut std::fs::File, tag: u8, parts: &[&[u8]]| {
        wire::write_frame(output, tag, parts).expect("write");
    };
    match mode {
        "no_magic" => {
            output.write_all(&[b'x'; 8192]).expect("write");
            std::thread::sleep(Duration::from_secs(30));
        }
        "other_version" => {
            output.write_all(&wire::MAGIC).expect("write");
            let hello = json!({ "wire": wire::WIRE_VERSION + 1, "host_core": "0.0.0" });
            send(&mut output, child::HELLO, &[hello.to_string().as_bytes()]);
            std::thread::sleep(Duration::from_secs(30));
        }
        _ => {}
    }
    output.write_all(&wire::MAGIC).expect("write");
    let hello = json!({ "wire": wire::WIRE_VERSION, "host_core": env!("CARGO_PKG_VERSION") });
    send(&mut output, child::HELLO, &[hello.to_string().as_bytes()]);
    let descriptor = include_str!("../../../../plugins/fixtures/scripted/descriptor.json");
    let mut input = BufReader::new(input);
    let mut sequence = 0_u32;
    while let Ok(frame) = wire::read_frame(&mut input, wire::MAX_HOST_FRAME_BYTES) {
        sequence += 1;
        let seq = sequence.to_le_bytes();
        let ok = 0_i32.to_le_bytes();
        match (frame.tag, mode) {
            (host::LOAD, _) => send(&mut output, child::LOADED, &[&seq]),
            (host::DESCRIBE, _) => send(
                &mut output,
                child::DESCRIBED,
                &[&seq, descriptor.as_bytes()],
            ),
            (host::BEGIN, "oversized") => {
                let length = u32::try_from(wire::MAX_CHILD_FRAME_BYTES + 1).expect("small");
                output.write_all(&length.to_le_bytes()).expect("write");
                output.write_all(&[child::RECORDS]).expect("write");
                std::thread::sleep(Duration::from_secs(30));
            }
            (host::BEGIN, "unknown_tag") | (host::VIDEO_SHARED, "refuse_region") => {
                send(&mut output, 0x7f, &[b"?"]);
            }
            (host::BEGIN, "cut_frame") => {
                output.write_all(&100_u32.to_le_bytes()).expect("write");
                output.write_all(&[child::RECORDS, 1, 2, 3]).expect("write");
                end_stream(output);
                std::thread::sleep(Duration::from_secs(30));
                return;
            }
            (host::BEGIN, "two_replies") => {
                send(&mut output, child::RETURNED, &[&seq, &ok]);
                send(&mut output, child::RETURNED, &[&seq, &ok]);
            }
            (host::BEGIN, "unknown_status") => {
                send(&mut output, child::RETURNED, &[&seq, &7_i32.to_le_bytes()]);
            }
            (host::BEGIN, "false_read") => {
                let read = json!({ "name": "model", "sha256": "a".repeat(64), "bytes": 1 });
                send(
                    &mut output,
                    child::RESOURCE_READ,
                    &[read.to_string().as_bytes()],
                );
                send(&mut output, child::RETURNED, &[&seq, &ok]);
            }
            (host::REGION, "refuse_region") => {
                let failure = json!({ "code": codes::LOAD_FAILED, "message": "refused" });
                send(
                    &mut output,
                    child::REGION_FAILED,
                    &[&seq, failure.to_string().as_bytes()],
                );
            }
            (host::REGION, _) => send(&mut output, child::REGION_MAPPED, &[&seq]),
            (host::VIDEO | host::VIDEO_SHARED, "refused_records") => {
                let mut record = Vec::new();
                alpha2_plugin_abi::records::encode(
                    &alpha2_plugin_abi::records::Record::Interval {
                        output: 31,
                        start: 0,
                        end: 1,
                        label: None,
                        score: None,
                    },
                    &mut record,
                );
                send(&mut output, child::RECORDS, &[&record]);
                send(&mut output, child::RETURNED, &[&seq, &ok]);
            }
            (host::SHUTDOWN, _) => std::process::exit(0),
            _ => send(&mut output, child::RETURNED, &[&seq, &ok]),
        }
    }
}

fn lying(mode: &str) -> Policy {
    Policy {
        isolation: Isolation::Child(worker_command(
            "run_as_lying_worker",
            &[(LYING_WORKER, mode)],
        )),
        ..with_deadline(native_allowed(), Duration::from_secs(60))
    }
}

#[test]
fn a_worker_that_breaks_the_wire_fails_the_run_as_a_trap_and_is_killed() {
    for mode in [
        "no_magic",
        "oversized",
        "unknown_tag",
        "cut_frame",
        "two_replies",
        "false_read",
    ] {
        let params = json!({});
        let outcome = loaded(&manifest(1, false, true), &params, &lying(mode))
            .map_err(PluginError::into_call_error)
            .and_then(|mut plugin| {
                run(&mut plugin, &params)
                    .map(drop)
                    .map_err(|error| failure(Err(error)))
            });
        let error = outcome.expect_err(mode);
        assert_eq!(error.code, Some(codes::TRAPPED), "{mode}: {error:?}");
        assert!(detail(&error, "wire").is_some(), "{mode}: {error:?}");
    }
    let other = loaded(
        &manifest(1, false, true),
        &json!({}),
        &lying("other_version"),
    )
    .expect_err("another build");
    assert_eq!(other.code, codes::LOAD_FAILED, "{other:?}");
}

#[test]
fn a_worker_is_held_to_the_codes_the_same_call_gives_in_process() {
    for (mode, code) in [
        ("refused_records", "INVALID_EMIT"),
        ("unknown_status", codes::TRAPPED),
    ] {
        let params = json!({});
        let mut plugin = loaded(&manifest(1, false, true), &params, &lying(mode)).expect("loads");
        let error = failure(run(&mut plugin, &params));
        assert_eq!(error.code, Some(code), "{mode}: {error:?}");
        assert!(detail(&error, "wire").is_none(), "{mode}: {error:?}");
    }
}

#[test]
fn a_worker_that_cannot_start_is_a_load_failure_without_a_fallback() {
    let policy = Policy {
        isolation: Isolation::Child(WorkerCommand {
            program: scratch("isolated-missing").join("no-such-worker"),
            args: Vec::new(),
            envs: Vec::new(),
            frames: FramePath::Shared,
        }),
        ..native_allowed()
    };
    let refused = loaded(&manifest(1, false, true), &json!({}), &policy).expect_err("no worker");
    assert_eq!(refused.code, codes::LOAD_FAILED, "{refused:?}");
}

#[test]
fn record_runs_cut_a_batch_at_record_boundaries_within_a_frame() {
    let mut batch = Vec::new();
    let text = "w".repeat(1 << 20);
    for _ in 0..9 {
        alpha2_plugin_abi::records::encode(
            &alpha2_plugin_abi::records::Record::Word {
                output: 0,
                start: 0,
                end: 1,
                text: text.clone(),
                speaker: None,
            },
            &mut batch,
        );
    }
    let runs = wire::record_runs(&batch);
    assert!(runs.len() > 1);
    assert!(
        runs.iter()
            .all(|run| run.len() <= wire::MAX_CHILD_FRAME_BYTES)
    );
    assert_eq!(runs.concat(), batch);
    let records = |bytes: &[u8]| alpha2_plugin_abi::records::decode(bytes).expect("whole records");
    assert_eq!(runs.iter().map(|run| records(run).len()).sum::<usize>(), 9);
}

const TIMED_FRAMES: u32 = 100;

fn ms_per_frame(plugin: usize, policy: &Policy, size: (u32, u32), format: VideoFormat) -> f64 {
    let mut plugin = loaded(&manifest(plugin, false, true), &json!({}), policy).expect("loads");
    let (width, height) = size;
    let name = match format {
        VideoFormat::Rgba8 => "rgba8",
        VideoFormat::Luma8 => "luma8",
    };
    let context = json!({
        "range": { "start": 0, "end": i64::from(TIMED_FRAMES + 1) * 4000 },
        "passes": [{ "start": 0, "direction": "forward" }], "from": "source",
        "video": { "size": width.max(height), "format": name }, "audio": null,
        "frame_rate": null, "params": {}, "supplied": plugin.slots_context.clone()
    });
    plugin
        .transport
        .begin(context.to_string().as_bytes())
        .expect("begin");
    plugin.transport.pass(0, direction::FORWARD).expect("pass");
    let bytes_per_pixel = match format {
        VideoFormat::Rgba8 => 4,
        VideoFormat::Luma8 => 1,
    };
    let length = usize::try_from(width * height * bytes_per_pixel).expect("length");
    let mut frame = VideoInput {
        time: MediaTime(0),
        width,
        height,
        format,
        bytes: (0..length)
            .map(|at| u8::try_from(at % 251).unwrap_or(0))
            .collect(),
    };
    let mut out = Emitter::default();
    plugin.video(&frame, &mut out).expect("first frame");
    let started = Instant::now();
    for index in 1..=TIMED_FRAMES {
        frame.time = MediaTime(i64::from(index) * 4000);
        plugin.video(&frame, &mut out).expect("frame");
    }
    started.elapsed().as_secs_f64() * 1000.0 / f64::from(TIMED_FRAMES)
}

fn isolated_through(frames: FramePath) -> Policy {
    let mut command = worker_command("run_as_worker", &[(WORKER, "1")]);
    command.frames = frames;
    Policy {
        isolation: Isolation::Child(command),
        ..native_allowed()
    }
}

#[test]
#[ignore = "a measurement, run in release with --nocapture"]
fn time_video_frames_in_process_and_through_a_worker() {
    for (size, format) in [
        ((1920, 1080), VideoFormat::Rgba8),
        ((960, 540), VideoFormat::Luma8),
    ] {
        let here = ms_per_frame(0, &native_allowed(), size, format);
        for frames in [FramePath::Shared, FramePath::Pipe] {
            let there = ms_per_frame(0, &isolated_through(frames), size, format);
            let idle = ms_per_frame(1, &isolated_through(frames), size, format);
            println!(
                "{}x{} {format:?} {frames:?}: digest in process {here:.3} ms/frame, digest in a \
                 worker {there:.3}, scripted in a worker {idle:.3}; worker cost {:.1} % of in \
                 process",
                size.0,
                size.1,
                (there - here) / here * 100.0
            );
        }
    }
}

fn video_context(frames: i64, size: u32, format: &str) -> Value {
    json!({
        "range": { "start": 0, "end": (frames + 1) * 4000 },
        "passes": [{ "start": 0, "direction": "forward" }], "from": "source",
        "video": { "size": size, "format": format }, "audio": null,
        "frame_rate": null, "params": {}, "supplied": {}
    })
}

fn digest_load() -> (wire::Load, Vec<OutputCap>) {
    let library = built().native[0].clone();
    let sha256 = sha256_hex(&std::fs::read(&library).expect("library"));
    let descriptor: Descriptor = serde_json::from_value(descriptor_of(0)).expect("descriptor");
    let caps = output_caps(&descriptor);
    let load = wire::Load {
        library,
        sha256,
        check: false,
        caps: caps
            .iter()
            .map(|cap| (cap.name.clone(), cap.items))
            .collect(),
        resources: resources::ResourceSet::default(),
    };
    (load, caps)
}

fn digest_worker(frames: FramePath) -> process::ProcessTransport {
    let mut command = worker_command("run_as_worker", &[(WORKER, "1")]);
    command.frames = frames;
    let mut worker = process::ProcessTransport::spawn(
        &command,
        &CancelToken::default(),
        Duration::from_secs(60),
    )
    .expect("spawn");
    let (load, caps) = digest_load();
    worker.load(&load, caps).expect("load");
    worker
        .begin(video_context(8, 128, "rgba8").to_string().as_bytes())
        .expect("begin");
    worker.pass(0, direction::FORWARD).expect("pass");
    worker
}

const SIZES: [(u32, u32); 4] = [(64, 32), (64, 32), (128, 64), (64, 32)];

fn digests(worker: &mut process::ProcessTransport) -> Vec<Vec<alpha2_plugin_abi::records::Record>> {
    SIZES
        .iter()
        .zip(0_u8..)
        .map(|(&(width, height), index)| {
            let pixels = (0..width * height * 4)
                .map(|at| u8::try_from(at % 251).expect("byte") ^ index)
                .collect::<Vec<_>>();
            let time = i64::from(index) * 4000;
            worker
                .video(time, width, height, video_format::RGBA8, &pixels)
                .expect("frame")
        })
        .collect()
}

#[test]
fn frames_through_shared_memory_give_the_data_the_pipe_gives_and_a_larger_frame_replaces_it() {
    let mut shared = digest_worker(FramePath::Shared);
    let mut piped = digest_worker(FramePath::Pipe);
    let through_memory = digests(&mut shared);
    assert_eq!(through_memory, digests(&mut piped));
    assert_eq!(through_memory.len(), SIZES.len());
    assert_ne!(through_memory[0], through_memory[1]);
    assert!(
        matches!(&shared.frames, process::Frames::Shared(region) if region.len() == 128 * 64 * 4),
        "{:?}",
        shared.frames
    );
    assert!(matches!(piped.frames, process::Frames::Pipe));
}

fn diagnostics_of(run: impl FnOnce()) -> Vec<String> {
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = std::sync::Arc::clone(&lines);
    alpha2_os::set_diagnostic_sink(Some(std::sync::Arc::new(move |line: &str| {
        if line.contains("the worker's pipe") {
            kept.lock().expect("lines").push(line.to_owned());
        }
    })));
    run();
    alpha2_os::set_diagnostic_sink(None);
    lines.lock().expect("lines").clone()
}

#[test]
fn memory_that_cannot_be_made_or_mapped_keeps_the_run_on_the_pipe_with_one_diagnostic() {
    let lines = diagnostics_of(|| {
        let mut unmade = digest_worker(FramePath::Shared);
        unmade.make_region = |_| Err(std::io::Error::other("refused for the test"));
        let mut piped = digest_worker(FramePath::Pipe);
        assert_eq!(digests(&mut unmade), digests(&mut piped));
        assert!(matches!(unmade.frames, process::Frames::Pipe));

        let params = json!({});
        let mut refused =
            loaded(&manifest(1, false, true), &params, &lying("refuse_region")).expect("loads");
        run(&mut refused, &params).expect("the run goes on through the pipe");
    });
    let made = lines
        .iter()
        .filter(|line| line.contains("cannot be made"))
        .count();
    let mapped = lines
        .iter()
        .filter(|line| line.contains("cannot map"))
        .count();
    assert_eq!((made, mapped), (1, 1), "{lines:?}");
}

#[test]
fn a_plugin_that_writes_into_its_frame_fails_the_run_as_a_trap() {
    let error = scripted_failure(&json!({ "write_input_at": 1 }), &isolated());
    assert_eq!(error.code, Some(codes::TRAPPED), "{error:?}");
    #[cfg(unix)]
    assert!(
        matches!(detail(&error, "signal"), Some(signal) if signal == json!(libc::SIGSEGV) || signal == json!(libc::SIGBUS)),
        "{error:?}"
    );
    #[cfg(windows)]
    assert_eq!(
        detail(&error, "exit"),
        Some(json!(0xC000_0005_u32)),
        "{error:?}"
    );
}

struct RawWorker {
    child: std::process::Child,
    #[cfg(unix)]
    input: std::os::unix::net::UnixStream,
    #[cfg(not(unix))]
    input: std::process::ChildStdin,
    output: Box<dyn std::io::Read>,
    sequence: u32,
}

impl RawWorker {
    fn start() -> Self {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .args(worker_command("run_as_worker", &[]).args)
            .env(WORKER, "1")
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        let (child, input, output) = {
            let (host, worker) = std::os::unix::net::UnixStream::pair().expect("socket pair");
            let child = command
                .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                    worker.try_clone().expect("clone"),
                )))
                .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                    worker,
                )))
                .spawn()
                .expect("worker");
            let output: Box<dyn std::io::Read> = Box::new(host.try_clone().expect("clone"));
            (child, host, output)
        };
        #[cfg(not(unix))]
        let (child, input, output) = {
            let mut child = command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("worker");
            let input = child.stdin.take().expect("stdin");
            let output: Box<dyn std::io::Read> = Box::new(child.stdout.take().expect("stdout"));
            (child, input, output)
        };
        let mut worker = Self {
            child,
            input,
            output: Box::new(BufReader::new(output)),
            sequence: 0,
        };
        wire::skip_to_magic(&mut worker.output).expect("magic");
        assert!(matches!(worker.reply(), wire::ChildMessage::Hello(_)));
        let load = serde_json::to_vec(&digest_load().0).expect("load");
        assert!(matches!(
            worker.send(host::LOAD, &[&load], None),
            wire::ChildMessage::Loaded { .. }
        ));
        let context = video_context(2, 64, "rgba8").to_string();
        assert!(matches!(
            worker.send(host::BEGIN, &[context.as_bytes()], None),
            wire::ChildMessage::Returned { status: 0, .. }
        ));
        worker
    }

    fn write(&mut self, tag: u8, parts: &[&[u8]], region: Option<&alpha2_os::SharedRegion>) {
        self.sequence += 1;
        #[cfg(unix)]
        if let Some(region) = region {
            let mut frame = Vec::new();
            wire::write_frame(&mut frame, tag, parts).expect("frame");
            alpha2_os::write_with_descriptor(&self.input, &frame, region.descriptor())
                .expect("write");
            return;
        }
        #[cfg(not(unix))]
        let _ = region;
        wire::write_frame(&mut self.input, tag, parts).expect("write");
    }

    fn reply(&mut self) -> wire::ChildMessage {
        loop {
            let message = wire::read_frame(&mut self.output, wire::MAX_CHILD_FRAME_BYTES)
                .and_then(wire::parse_child)
                .expect("a frame");
            if !matches!(
                message,
                wire::ChildMessage::Log { .. } | wire::ChildMessage::Records(_)
            ) {
                return message;
            }
        }
    }

    fn send(
        &mut self,
        tag: u8,
        parts: &[&[u8]],
        region: Option<&alpha2_os::SharedRegion>,
    ) -> wire::ChildMessage {
        self.write(tag, parts, region);
        self.reply()
    }

    fn region(&mut self, len: usize) -> (alpha2_os::SharedRegion, wire::ChildMessage) {
        let region = alpha2_os::SharedRegion::create(len).expect("region");
        let length = u64::try_from(len).expect("length").to_le_bytes();
        #[cfg(windows)]
        let handle = region.share_with(&self.child).expect("share").to_le_bytes();
        #[cfg(windows)]
        let parts: [&[u8]; 2] = [&length, &handle];
        #[cfg(not(windows))]
        let parts: [&[u8]; 1] = [&length];
        let reply = self.send(host::REGION, &parts, Some(&region));
        (region, reply)
    }

    fn exit_code(mut self) -> Option<i32> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(30) {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        None
    }
}

fn shared_frame(width: u32, height: u32, offset: u64, len: u64) -> Vec<u8> {
    [
        &0_i64.to_le_bytes()[..],
        &width.to_le_bytes(),
        &height.to_le_bytes(),
        &video_format::RGBA8.to_le_bytes(),
        &offset.to_le_bytes(),
        &len.to_le_bytes(),
    ]
    .concat()
}

#[test]
fn a_worker_maps_the_frames_memory_and_ends_broken_on_frames_the_wire_does_not_allow() {
    let mut honest = RawWorker::start();
    let (region, mapped) = honest.region(64 * 64 * 4);
    assert!(
        matches!(mapped, wire::ChildMessage::RegionMapped { .. }),
        "{mapped:?}"
    );
    let frame = shared_frame(64, 64, 0, 64 * 64 * 4);
    let returned = honest.send(host::VIDEO_SHARED, &[&frame], None);
    assert!(
        matches!(returned, wire::ChildMessage::Returned { status: 0, .. }),
        "{returned:?}"
    );
    honest.write(host::SHUTDOWN, &[], None);
    drop(region);
    assert_eq!(honest.exit_code(), Some(0));

    let broken = |case: &str, worker: RawWorker| {
        assert_eq!(worker.exit_code(), Some(3), "{case}");
    };
    let mut worker = RawWorker::start();
    worker.write(host::VIDEO_SHARED, &[&shared_frame(16, 16, 0, 1024)], None);
    broken("no region", worker);

    let mut worker = RawWorker::start();
    let (_region, _) = worker.region(1024);
    worker.write(
        host::VIDEO_SHARED,
        &[&shared_frame(16, 16, 512, 1024)],
        None,
    );
    broken("past the region", worker);

    let mut worker = RawWorker::start();
    let (_region, _) = worker.region(8192);
    worker.write(host::VIDEO_SHARED, &[&shared_frame(32, 32, 0, 4000)], None);
    broken("a short length", worker);

    #[cfg(unix)]
    {
        let mut worker = RawWorker::start();
        worker.write(host::REGION, &[&1024_u64.to_le_bytes()], None);
        broken("a region without a descriptor", worker);

        let region = alpha2_os::SharedRegion::create(1024).expect("region");
        let mut worker = RawWorker::start();
        worker.write(host::DESCRIBE, &[], Some(&region));
        broken("a descriptor with another frame", worker);
    }
}
