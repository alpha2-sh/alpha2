use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alpha2_engine::extract::DriveError;
use alpha2_media::CancelToken;
use serde_json::{Value, json};

use super::tests::{
    Local, built, loaded, loaded_with, manifest, native_allowed, run, scratch, script,
    supplied_words,
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
            std::fs::canonicalize(dir.join("extra"))
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
            (host::BEGIN, "unknown_tag") => send(&mut output, 0x7f, &[b"?"]),
            (host::BEGIN, "cut_frame") => {
                output.write_all(&100_u32.to_le_bytes()).expect("write");
                output.write_all(&[child::RECORDS, 1, 2, 3]).expect("write");
                drop(output);
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
            (host::VIDEO, "refused_records") => {
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
