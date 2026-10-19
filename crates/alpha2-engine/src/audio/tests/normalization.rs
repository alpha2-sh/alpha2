//! The master limiter and loudness normalization.

use super::*;

#[test]
fn the_master_limiter_bounds_a_hot_mix() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![
            track("a1", vec![audio_clip("c1", "tone", 0, seconds)]),
            track("a2", vec![audio_clip("c2", "tone", 0, seconds)]),
        ],
        vec!["tone"],
    );
    base.audio.limiter = Some(Limiter {
        threshold: 0.5,
        release_ms: 20.0,
    });
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    let (renderer, _) = render(base, 256, &fixture);
    for &value in &renderer.channel(0)[..256] {
        assert!(value.abs() <= 0.5 + 1.0e-4, "{value} exceeds the ceiling");
    }
}

#[test]
fn a_normalized_mix_measures_at_its_target_and_the_limiter_is_not_metered() {
    let seconds = MediaTime::from_seconds(4).ticks();
    let mut base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    // A limiter far below the signal: it must not change what is metered.
    base.audio.limiter = Some(Limiter {
        threshold: 0.05,
        release_ms: 20.0,
    });
    let snapshot = EngineProject::new(base).expect("valid project");
    let provider = Sine::new(0.1, 4);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    let end = i64::from(RATE) * 4;
    let before = meter(&mut renderer, &snapshot, &provider, (0, end), None);
    // A 0.1 (-20 dBFS) stereo sine reads -20 LUFS, limiter or not.
    let integrated = before.integrated_lufs.expect("loud enough");
    assert!((integrated + 20.0).abs() < 0.1, "{integrated}");
    assert_eq!(before.frames, u64::try_from(end).expect("small"));

    // Without a limiter after it, the mix normalizes to the target where the meter reads.
    let plan = alpha2_audio::LoudnessNormalization::plan(&before, -14.0, -1.0, None).expect("plan");
    assert!(!plan.peak_limited);
    renderer.reset();
    let after = meter(
        &mut renderer,
        &snapshot,
        &provider,
        (0, end),
        Some(plan.gain_db),
    );
    let normalized = after.integrated_lufs.expect("loud enough");
    assert!((normalized + 14.0).abs() < 0.05, "{normalized}");

    // A seek replays history before the requested block; only the block is metered.
    renderer.set_loudness_metering(true).expect("meter");
    renderer
        .render(&snapshot, 96_000, 4_096, &provider, TransportState::Playing)
        .expect("render after a seek");
    assert_eq!(renderer.loudness_report().expect("on").frames, 4_096);
}

/// Renders `[start, end)` in 4096-sample blocks and measures what the
/// renderer outputs, after the limiter, with a meter of its own.
fn measure_output(
    renderer: &mut AudioRenderer,
    snapshot: &EngineProject,
    provider: &Sine,
    (start, end): (i64, i64),
) -> LoudnessReport {
    let mut output = LoudnessMeter::new(RATE, 2).expect("meter");
    let mut position = start;
    while position < end {
        let frames = usize::try_from((end - position).min(4_096)).expect("small");
        renderer
            .render(
                snapshot,
                position,
                frames,
                provider,
                TransportState::Playing,
            )
            .expect("render");
        output
            .push(&[
                &renderer.channel(0)[..frames],
                &renderer.channel(1)[..frames],
            ])
            .expect("block");
        position += i64::try_from(frames).expect("small");
    }
    output.report()
}

/// A limiter below where the target would put the peaks: the plan stops
/// the gain short of it, so the limiter leaves the normalized mix alone
/// and what comes out measures what the plan says. The renderer's own
/// output meter reads exactly what a meter of the output reads.
#[test]
fn a_normalized_mix_under_a_low_limiter_comes_out_as_planned() {
    let seconds = MediaTime::from_seconds(4).ticks();
    let mut base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    base.audio.limiter = Some(Limiter {
        threshold: 0.05,
        release_ms: 20.0,
    });
    let snapshot = EngineProject::new(base).expect("valid project");
    let provider = Sine::new(0.1, 4);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    assert_eq!(renderer.limiter_threshold(), Some(f64::from(0.05_f32)));
    let end = i64::from(RATE) * 4;
    let before = meter(&mut renderer, &snapshot, &provider, (0, end), None);
    renderer.set_loudness_metering(false).expect("meter off");
    renderer.reset();
    let plan = alpha2_audio::LoudnessNormalization::plan(
        &before,
        -14.0,
        -1.0,
        renderer.limiter_threshold(),
    )
    .expect("plan");
    assert!(plan.peak_limited && plan.limiter_limited, "{plan:?}");
    renderer
        .set_normalization_gain_db(plan.gain_db)
        .expect("gain");
    renderer
        .set_output_loudness_metering(true)
        .expect("output meter");
    let output = measure_output(&mut renderer, &snapshot, &provider, (0, end));
    let integrated = output.integrated_lufs.expect("loud enough");
    assert!(
        (integrated - plan.integrated_lufs).abs() < 0.05,
        "the mix comes out at {integrated} LUFS, planned {plan:?}"
    );
    let peak = output.sample_peak_dbfs.expect("a peak");
    assert!(peak <= 20.0 * libm::log10(0.05), "{peak} dBFS");
    assert_eq!(renderer.output_loudness_report(), Some(output));
    // The gain stays while the output is metered, and a seek's replay is not measured.
    renderer
        .render(&snapshot, 96_000, 4_096, &provider, TransportState::Playing)
        .expect("render after a seek");
    assert_eq!(
        renderer.output_loudness_report().expect("on").frames,
        u64::try_from(end).expect("small") + 4_096
    );
    renderer
        .set_output_loudness_metering(false)
        .expect("output meter off");
    assert_eq!(renderer.output_loudness_report(), None);
}

#[test]
fn a_quiet_or_loud_mix_takes_the_gain_its_plan_gives() {
    // A plan can give a quiet mix more than 48 dB of gain after the measuring pass; the
    // renderer must take it rather than refuse it.
    let seconds = MediaTime::from_seconds(4).ticks();
    let base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    let snapshot = EngineProject::new(base).expect("valid project");
    let end = i64::from(RATE) * 4;
    for (amplitude, target) in [(0.000_5_f32, -14.0), (0.316_f32, -60.0)] {
        let provider = Sine::new(amplitude, 4);
        let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
        let measured = meter(&mut renderer, &snapshot, &provider, (0, end), None);
        let plan = alpha2_audio::LoudnessNormalization::plan(
            &measured,
            target,
            -1.0,
            renderer.limiter_threshold(),
        )
        .expect("plan");
        assert!(plan.gain_db.abs() > 48.0, "{}", plan.gain_db);
        renderer
            .set_normalization_gain_db(plan.gain_db)
            .expect("the plan's gain");
        let after = meter(&mut renderer, &snapshot, &provider, (0, end), None);
        let renormalized = alpha2_audio::LoudnessNormalization::plan(
            &after,
            target,
            -1.0,
            renderer.limiter_threshold(),
        )
        .expect("plan")
        .gain_db;
        // Metering again measures the mix as authored, so the plan is the same.
        assert!(
            (renormalized - plan.gain_db).abs() < 1.0e-9,
            "{renormalized}"
        );
        renderer.reset();
        let normalized = meter(
            &mut renderer,
            &snapshot,
            &provider,
            (0, end),
            Some(plan.gain_db),
        )
        .integrated_lufs
        .expect("loud enough");
        assert!(
            (normalized - target).abs() < 0.05,
            "{normalized} for {target}"
        );
    }
    let snapshot_renderer = &mut AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    for gain in [f64::NAN, f64::INFINITY, 1_000.0, -1_000.0] {
        assert_eq!(
            snapshot_renderer.set_normalization_gain_db(gain),
            Err(EngineError::InvalidNormalizationGain),
            "{gain}"
        );
    }
}

#[test]
fn measuring_again_is_not_moved_by_the_gain_already_set() {
    let seconds = MediaTime::from_seconds(4).ticks();
    let base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    let snapshot = EngineProject::new(base).expect("valid project");
    let provider = Sine::new(0.1, 4);
    let end = i64::from(RATE) * 4;
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    let mut gains = Vec::new();
    for _ in 0..2 {
        let measured = meter(&mut renderer, &snapshot, &provider, (0, end), None);
        let plan = alpha2_audio::LoudnessNormalization::plan(
            &measured,
            -14.0,
            -1.0,
            renderer.limiter_threshold(),
        )
        .expect("plan");
        renderer.set_loudness_metering(false).expect("meter off");
        renderer
            .set_normalization_gain_db(plan.gain_db)
            .expect("gain");
        renderer.reset();
        gains.push(plan.gain_db);
    }
    assert!((gains[0] - gains[1]).abs() < 1.0e-9, "{gains:?}");
}
