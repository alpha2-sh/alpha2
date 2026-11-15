use super::requests::{SERVE_CHILD, ServeChild};
use super::*;

/// An image over a 6 s tone, with its audio, optionally normalized.
fn tone_export(id: &str, directory: &Path, loudness: bool) -> String {
    let end = 6 * alpha2_time::TICKS_PER_SECOND;
    let mut request = json!({
        "protocolVersion": 2, "operation": "export", "requestId": id,
        "project": image_and_tone_project(end, 48_000),
        "assets": {
            "image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png"),
            "tone": write_tone(directory),
        },
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "videoCodec": "mpeg4-part2",
        "output": directory.join("out.mp4"),
    });
    if loudness {
        request["loudness"] = json!({"targetLufs": -16.0, "truePeakCeilingDbtp": -1.0});
    }
    request.to_string()
}

fn write_tone(directory: &Path) -> PathBuf {
    let path = directory.join("tone.wav");
    let samples = std::iter::successors(Some(0.0_f32), |phase| Some(phase + 0.05))
        .take(48_000 * 6)
        .map(|phase| phase.sin() * 0.1)
        .collect::<Vec<_>>();
    write_float_wav(&path, 48_000, &samples);
    path
}

#[test]
fn a_request_stuck_in_a_step_is_answered_by_its_side_and_retires_the_host() {
    for (step, loudness, code) in [
        ("audio windows", false, "DECODE_STALLED"),
        ("loudness pass", true, "DECODE_STALLED"),
        ("encoder open", false, "ENCODE_STALLED"),
    ] {
        let _gpu = gpu_test_lock();
        let directory = scratch_directory("request-stuck");
        let mut serving = ServeChild::start(
            SERVE_CHILD,
            &[
                ("ALPHA2_TEST_REQUEST_STALL_MS", "1000"),
                ("ALPHA2_TEST_REQUEST_STALL_AT", step),
                ("ALPHA2_TEST_TERMINATION_GRACE_MS", "1000"),
            ],
        );
        assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
        let started = Instant::now();
        serving.write(&tone_export("stuck", &directory, loudness));
        let state = serving.message(Duration::from_secs(60));
        assert_eq!(state["type"], "state", "{step}: {state}");
        assert_eq!(state["reason"], "threads-abandoned", "{step}: {state}");
        let answer = serving.message(Duration::from_secs(10));
        assert_eq!(answer["requestId"], "stuck", "{step}: {answer}");
        assert_eq!(answer["error"]["code"], code, "{step}: {answer}");
        assert_eq!(
            answer["error"]["failure"]["hostCode"], code,
            "{step}: {answer}"
        );
        assert_eq!(answer["hostState"], "draining", "{step}: {answer}");
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(&format!("its {step} step")), "{message}");
        assert!(message.contains("past the 1000 ms limit"), "{message}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{step}: answered after {:?}",
            started.elapsed()
        );
        assert_eq!(serving.finish(), Some(1), "{step}");
    }
}

#[test]
fn a_slow_step_that_keeps_beating_is_not_stalled() {
    let directory = scratch_directory("request-beats");
    let path = directory.join("long.wav");
    // A sparse silent WAV near the format's 4 GiB limit: hashing it takes many times the limit.
    let samples: u64 = 1000 << 20;
    write_float_wav(&path, 48_000, &[]);
    let mut header = std::fs::read(&path).expect("header");
    let data = u32::try_from(samples * 4).expect("fits");
    header[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    header[40..44].copy_from_slice(&data.to_le_bytes());
    std::fs::write(&path, &header).expect("header");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_len(44 + u64::from(data)))
        .expect("length");
    // Longer than the step's beat interval on a slow machine under GPU validation.
    let stall = Duration::from_millis(300);
    let mut serving = ServeChild::start(SERVE_CHILD, &[("ALPHA2_TEST_REQUEST_STALL_MS", "300")]);
    assert_eq!(serving.message(Duration::from_secs(30))["state"], "ready");
    let mut project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    project["sources"][1]["fingerprint"] = json!({"content_sha256": "0".repeat(64)});
    let started = Instant::now();
    serving.write(
        &json!({
            "protocolVersion": 2, "requestId": "conform", "operation": "conform",
            "project": project,
            "assets": {"tone": path},
        })
        .to_string(),
    );
    let answer = serving.message(Duration::from_secs(120));
    let took = started.elapsed();
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["hostState"], "ready", "{answer}");
    assert!(
        took > stall * 2,
        "hashing took {took:?}, too short to prove the step beats"
    );
    assert_eq!(serving.finish(), Some(0));
}
