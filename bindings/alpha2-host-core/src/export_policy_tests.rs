use super::*;

#[test]
fn levels_follow_the_frame_size_frame_rate_and_bitrate() {
    let fps = |num| FrameRate::new(num, 1).expect("rate");
    assert_eq!(avc_level(176, 144, fps(15), 64_000), 10);
    assert_eq!(avc_level(1280, 720, fps(30), 8_000_000), 31);
    assert_eq!(avc_level(1920, 1080, fps(30), 12_000_000), 40);
    assert_eq!(avc_level(1920, 1080, fps(30), 30_000_000), 41);
    assert_eq!(avc_level(3840, 2160, fps(30), 40_000_000), 51);
    assert_eq!(avc_level(16_384, 16_384, fps(30), 1_000_000_000), 62);
    // Macroblocks per second: 1080p60 needs 4.2, 2160p60 needs 5.2 rather than 5.1.
    assert_eq!(avc_level(1920, 1080, fps(60), 12_000_000), 42);
    assert_eq!(avc_level(3840, 2160, fps(60), 40_000_000), 52);
    let ntsc = FrameRate::new(30_000, 1_001).expect("rate");
    assert_eq!(avc_level(1280, 720, ntsc, 8_000_000), 31);
}

#[test]
fn codec_strings_carry_profile_constraints_and_level() {
    let strings = AvcProfile::LADDER.map(|profile| {
        let level = avc_level(1920, 1080, FrameRate::new(30, 1).expect("rate"), 12_000_000);
        avc_codec_string(profile, level)
    });
    assert_eq!(strings, ["avc1.640028", "avc1.4d0028", "avc1.42e028"]);
}

/// `tests/cases/retry-decisions.json`, which the TypeScript policy and the editor's job loop
/// replay too.
#[test]
fn the_retry_decisions_corpus() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/retry-decisions.json");
    let corpus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("the corpus")).expect("json");
    let text = |value: &serde_json::Value| value.as_str().expect("a string").to_owned();
    let class = |value: &serde_json::Value| FailureClass::parse(&text(value)).expect("a class");
    let profile = |value: &serde_json::Value| {
        RETRY_PROFILES
            .into_iter()
            .find(|profile| profile.name == text(value))
            .expect("a profile")
    };
    let renderer = |value: &serde_json::Value| match value.as_str() {
        Some("gpu") => AttemptRenderer::Gpu,
        Some("cpu") => AttemptRenderer::Cpu,
        other => panic!("renderer {other:?}"),
    };
    let encoder = |value: &serde_json::Value| match value.as_str() {
        Some("default") => AttemptEncoder::Default,
        Some("software") => AttemptEncoder::Software,
        other => panic!("video encoder {other:?}"),
    };
    let mut replayed = 0;
    for case in corpus["cases"].as_array().expect("cases") {
        let name = text(&case["name"]);
        if let Some(surfaces) = case["surfaces"].as_array()
            && !surfaces.iter().any(|surface| surface == "native")
        {
            continue;
        }
        let history = case["history"]
            .as_array()
            .expect("history")
            .iter()
            .map(|attempt| {
                let failure = &attempt["failure"];
                AttemptOutcome {
                    failure: AttemptFailure {
                        code: class(&failure["code"]),
                        resource: MemoryResource::parse(&text(&failure["resource"]))
                            .expect("a resource"),
                        phase: text(&failure["phase"]),
                        frame: failure["frameIndex"].as_u64(),
                        retry_after_ms: failure["retryAfterMs"].as_u64(),
                    },
                    profile: profile(&attempt["profile"]),
                    renderer: renderer(&attempt["renderer"]),
                    encoder: encoder(&attempt["videoEncoder"]),
                }
            })
            .collect::<Vec<_>>();
        let options = &case["options"];
        let defaults = RetryOptions::default();
        let options = RetryOptions {
            max_attempts: options["maxAttempts"]
                .as_u64()
                .map_or(defaults.max_attempts, |max| {
                    usize::try_from(max).expect("attempts")
                }),
            cpu_renderer: options["cpuRenderer"]
                .as_bool()
                .unwrap_or(defaults.cpu_renderer),
            software_encoder: options["softwareVideoEncoder"]
                .as_bool()
                .unwrap_or(defaults.software_encoder),
        };
        let expected = &case["decision"];
        match decide_retry(&history, options) {
            RetryDecision::Retry {
                delay_ms,
                profile: next,
                renderer: next_renderer,
                encoder: next_encoder,
                ..
            } => {
                assert_eq!(expected["retry"], true, "{name}: retries");
                assert_eq!(Some(delay_ms), expected["delayMs"].as_u64(), "{name}");
                assert_eq!(next, profile(&expected["profile"]), "{name}");
                assert_eq!(next_renderer, renderer(&expected["renderer"]), "{name}");
                assert_eq!(next_encoder, encoder(&expected["videoEncoder"]), "{name}");
            }
            RetryDecision::Stop { code, .. } => {
                assert_eq!(expected["retry"], false, "{name}: stops");
                assert_eq!(code, Some(class(&expected["failureCode"])), "{name}");
            }
        }
        replayed += 1;
    }
    assert!(replayed > 20, "{replayed} cases");
}

#[test]
fn an_empty_history_never_retries() {
    assert_eq!(
        decide_retry(&[], RetryOptions::default()),
        RetryDecision::Stop {
            reason: "nothing failed".into(),
            code: None,
        }
    );
}

#[test]
fn a_host_error_code_is_not_a_failure_class() {
    assert_eq!(FailureClass::parse("ENCODER_BUSY"), None);
    for class in FailureClass::ALL {
        assert_eq!(FailureClass::parse(class.as_str()), Some(class));
    }
}
