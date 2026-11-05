use super::*;

#[test]
fn levels_follow_the_frame_size_and_bitrate() {
    assert_eq!(avc_level(176, 144, 64_000), 10);
    assert_eq!(avc_level(1280, 720, 8_000_000), 31);
    assert_eq!(avc_level(1920, 1080, 12_000_000), 40);
    assert_eq!(avc_level(1920, 1080, 30_000_000), 41);
    assert_eq!(avc_level(3840, 2160, 40_000_000), 51);
    assert_eq!(avc_level(16_384, 16_384, 1_000_000_000), 62);
}

#[test]
fn codec_strings_carry_profile_constraints_and_level() {
    let strings = AvcProfile::LADDER
        .map(|profile| avc_codec_string(profile, avc_level(1920, 1080, 12_000_000)));
    assert_eq!(strings, ["avc1.640028", "avc1.4d0028", "avc1.42e028"]);
}

fn failure(code: &str, phase: &str, frame: Option<u64>) -> AttemptFailure {
    AttemptFailure {
        code: code.into(),
        phase: phase.into(),
        frame,
        retry_after_ms: None,
    }
}

fn attempt(failure: AttemptFailure, profile: usize, renderer: AttemptRenderer) -> AttemptOutcome {
    AttemptOutcome {
        failure,
        profile: RETRY_PROFILES[profile],
        renderer,
        encoder: AttemptEncoder::Default,
    }
}

fn settings(
    decision: &RetryDecision,
) -> Option<(&'static str, AttemptRenderer, AttemptEncoder, u64)> {
    match decision {
        RetryDecision::Retry {
            profile,
            renderer,
            encoder,
            delay_ms,
            ..
        } => Some((profile.name, *renderer, *encoder, *delay_ms)),
        RetryDecision::Stop { .. } => None,
    }
}

#[test]
fn a_gpu_hang_moves_to_the_cpu_renderer_or_stops() {
    let hung = [attempt(
        failure("DEVICE_HUNG", "render", Some(4)),
        0,
        AttemptRenderer::Gpu,
    )];
    let decision = decide_retry(&hung, RetryOptions::default());
    assert_eq!(
        settings(&decision),
        Some((
            "default",
            AttemptRenderer::Cpu,
            AttemptEncoder::Default,
            1_000
        ))
    );
    let no_cpu = decide_retry(
        &hung,
        RetryOptions {
            cpu_renderer: false,
            ..RetryOptions::default()
        },
    );
    assert!(
        matches!(no_cpu, RetryDecision::Stop { code: Some(code), .. } if code == "DEVICE_HUNG")
    );
}

#[test]
fn a_gpu_loss_recurring_at_one_frame_is_a_hang() {
    let history = [
        attempt(
            failure("DEVICE_LOST", "render", Some(9)),
            0,
            AttemptRenderer::Gpu,
        ),
        attempt(
            failure("GPU_TIMEOUT", "render", Some(9)),
            0,
            AttemptRenderer::Gpu,
        ),
    ];
    let decision = decide_retry(&history, RetryOptions::default());
    assert_eq!(settings(&decision).map(|s| s.1), Some(AttemptRenderer::Cpu));
    let once = decide_retry(&history[..1], RetryOptions::default());
    assert_eq!(settings(&once).map(|s| s.1), Some(AttemptRenderer::Gpu));
}

#[test]
fn memory_steps_down_and_stops_at_the_smallest_profile() {
    let first = [attempt(
        failure("OUT_OF_MEMORY", "decode", None),
        0,
        AttemptRenderer::Gpu,
    )];
    assert_eq!(
        settings(&decide_retry(&first, RetryOptions::default())).map(|s| s.0),
        Some("reduced")
    );
    let last = [
        attempt(
            failure("OUT_OF_MEMORY", "decode", None),
            1,
            AttemptRenderer::Gpu,
        ),
        attempt(
            failure("OUT_OF_MEMORY", "decode", None),
            2,
            AttemptRenderer::Gpu,
        ),
    ];
    let decision = decide_retry(
        &last,
        RetryOptions {
            max_attempts: 5,
            ..RetryOptions::default()
        },
    );
    assert!(
        matches!(decision, RetryDecision::Stop { .. }),
        "{decision:?}"
    );
}

#[test]
fn an_encode_failure_retries_once_in_software() {
    let first = [attempt(
        failure("ENCODER_BUSY", "encode", Some(0)),
        0,
        AttemptRenderer::Gpu,
    )];
    assert_eq!(
        settings(&decide_retry(&first, RetryOptions::default())).map(|s| s.2),
        Some(AttemptEncoder::Software)
    );
    let no_software = decide_retry(
        &first,
        RetryOptions {
            software_encoder: false,
            ..RetryOptions::default()
        },
    );
    match no_software {
        RetryDecision::Retry {
            reason, encoder, ..
        } => {
            assert_eq!(encoder, AttemptEncoder::Default);
            assert!(
                reason.ends_with("(no software video encoder to switch to)"),
                "{reason}"
            );
        }
        RetryDecision::Stop { reason, .. } => panic!("{reason}"),
    }
}

#[test]
fn a_busy_resource_waits_at_least_as_long_as_it_asks() {
    let mut busy = failure("RESOURCE_BUSY", "encode", None);
    busy.retry_after_ms = Some(5_000);
    let history = [AttemptOutcome {
        encoder: AttemptEncoder::Software,
        ..attempt(busy, 0, AttemptRenderer::Gpu)
    }];
    assert_eq!(
        settings(&decide_retry(&history, RetryOptions::default())).map(|s| s.3),
        Some(5_000)
    );
}

#[test]
fn permanent_failures_and_spent_attempts_stop() {
    for code in ["INVALID_INPUT", "CANCELLED", "OUTPUT_IO_FAILED"] {
        let history = [attempt(
            failure(code, "prepare", None),
            0,
            AttemptRenderer::Gpu,
        )];
        assert!(matches!(
            decide_retry(&history, RetryOptions::default()),
            RetryDecision::Stop { .. }
        ));
    }
    let spent = [
        attempt(
            failure("DECODE_STALLED", "decode", Some(1)),
            0,
            AttemptRenderer::Gpu,
        ),
        attempt(
            failure("DECODE_STALLED", "decode", Some(2)),
            0,
            AttemptRenderer::Gpu,
        ),
        attempt(
            failure("DECODE_STALLED", "decode", Some(3)),
            0,
            AttemptRenderer::Gpu,
        ),
    ];
    assert!(matches!(
        decide_retry(&spent, RetryOptions::default()),
        RetryDecision::Stop { .. }
    ));
}

#[test]
fn a_transient_failure_steps_down_only_when_it_recurs_at_the_same_point() {
    let once = [attempt(
        failure("DECODE_STALLED", "decode", Some(7)),
        0,
        AttemptRenderer::Gpu,
    )];
    assert_eq!(
        settings(&decide_retry(&once, RetryOptions::default())).map(|s| s.0),
        Some("default")
    );
    let twice = [
        attempt(
            failure("DECODE_STALLED", "decode", Some(7)),
            0,
            AttemptRenderer::Gpu,
        ),
        attempt(
            failure("DECODE_STALLED", "decode", Some(7)),
            0,
            AttemptRenderer::Gpu,
        ),
    ];
    assert_eq!(
        settings(&decide_retry(&twice, RetryOptions::default())).map(|s| s.0),
        Some("reduced")
    );
}
