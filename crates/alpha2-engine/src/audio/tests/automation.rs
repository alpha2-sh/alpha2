//! Automated gain and pan, evaluated at control points and the same in any block.

use super::*;

#[test]
fn a_unity_clip_passes_its_source_through_sample_for_sample() {
    for rate in [44_100, 48_000, 96_000] {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let sine = (0..rate as usize)
            .map(|index| {
                (libm::sin(std::f64::consts::TAU * 10_000.0 * index as f64 / f64::from(rate)) * 0.5)
                    as f32
            })
            .collect::<Vec<_>>();
        let seconds = MediaTime::from_seconds(1).ticks();
        let snapshot = rate_project(rate, vec![audio_clip("c1", "tone", 0, seconds)]);
        let provider = Plane(AudioBlock {
            start_sample: 0,
            sample_rate: rate,
            speakers: SpeakerLayout::STEREO,
            channels: vec![sine.clone(), sine.clone()],
        });
        let output = render_all(&snapshot, &provider, rate as usize, 512);
        let (worst, error) = output
            .iter()
            .zip(&sine)
            .map(|(mixed, source)| (mixed - source).abs())
            .enumerate()
            .fold((0, 0.0_f32), |worst, (index, error)| {
                if error > worst.1 {
                    (index, error)
                } else {
                    worst
                }
            });
        assert!(
            error < 1e-6,
            "{rate} Hz: max error {error} at sample {worst}"
        );
    }
}

#[test]
fn constant_gain_and_pan_mix_exactly_like_their_automated_equivalents() {
    // Two equal keyframes sample the same value at every time but take the per-sample path;
    // constants take the hoisted one. Fades end inside blocks, so some blocks mix both
    // ways.
    let flat = |value: f32| Animated::Keyframes {
        keyframes: [0, 2]
            .into_iter()
            .map(|seconds| Keyframe {
                time: MediaTime::from_seconds(seconds),
                value,
                interpolation: Interpolation::Linear,
            })
            .collect(),
    };
    let build = |automated: bool| {
        let animate = |value: f32| {
            if automated {
                flat(value)
            } else {
                Animated::Constant { value }
            }
        };
        let mut clip = audio_clip("c1", "tone", 0, MediaTime::from_seconds(1).ticks());
        clip.audio.gain = animate(0.8);
        clip.audio.pan = animate(0.3);
        clip.audio.fade_in = MediaTime(1_234);
        clip.audio.fade_out = MediaTime(5_678);
        let mut project = project(vec![track("a1", vec![clip])], vec!["tone"]);
        project.timeline.tracks[0].audio.gain = animate(1.7);
        project.timeline.tracks[0].audio.pan = animate(-0.6);
        project.audio.gain = animate(0.9);
        EngineProject::new(project).expect("valid project")
    };
    let provider = Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: SpeakerLayout::MONO,
        channels: vec![
            (0..RATE)
                .map(|index| f32::from(u16::try_from(index % 101).expect("small")) / 100.0)
                .collect(),
        ],
    });
    let total = RATE as usize;
    let reference = render_all(&build(true), &provider, total, 512);
    for block in [512, 37] {
        let hoisted = render_all(&build(false), &provider, total, block);
        let first_difference = hoisted
            .iter()
            .zip(&reference)
            .position(|(left, right)| left.to_bits() != right.to_bits());
        assert_eq!(first_difference, None, "blocks of {block}");
    }
}

/// Both channels of `total` samples from project sample `start`, mixed in
/// blocks of `block` by a fresh renderer.
fn mix_both(
    snapshot: &EngineProject,
    provider: &dyn AudioBlockProvider,
    start: i64,
    total: usize,
    block: usize,
) -> [Vec<f32>; 2] {
    let mut renderer = AudioRenderer::new(snapshot, 2, block).expect("renderer");
    let mut output = [Vec::with_capacity(total), Vec::with_capacity(total)];
    while output[0].len() < total {
        let frames = block.min(total - output[0].len());
        let at = start + i64::try_from(output[0].len()).expect("small");
        let status = renderer
            .render(snapshot, at, frames, provider, TransportState::Playing)
            .expect("render");
        assert!(status.is_complete());
        for (channel, output) in output.iter_mut().enumerate() {
            output.extend_from_slice(&renderer.channel(channel)[..frames]);
        }
    }
    output
}

/// A clip with eased gain and pan, equal-power fades, and a held step,
/// on a track with eased gain and pan, under an eased master gain.
fn automated_everywhere() -> EngineProject {
    let ease = Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.42, 0.0, 0.58, 1.0));
    let ms = |value: i64| value * TICKS_PER_SECOND / 1_000;
    let mut clip = audio_clip("c1", "tone", ms(3) + 7, ms(900));
    clip.audio.gain = keyframed(&[0, ms(40), ms(90), ms(130)], &[0.3, 1.4], ease);
    if let Animated::Keyframes { keyframes } = &mut clip.audio.gain {
        keyframes[2].interpolation = Interpolation::Hold;
    }
    clip.audio.pan = keyframed(&[ms(10), ms(70), ms(120)], &[-0.9, 0.8], ease);
    clip.audio.fade_in = MediaTime(ms(20) + 3);
    clip.audio.fade_out = MediaTime(ms(30) - 11);
    clip.audio.fade_shape = alpha2_model::FadeShape::EqualPower;
    let mut project = project(vec![track("a1", vec![clip])], vec!["tone"]);
    project.timeline.tracks[0].audio.gain = keyframed(&[ms(5), ms(100)], &[1.2, 0.6], ease);
    project.timeline.tracks[0].audio.pan = keyframed(&[0, ms(150)], &[0.4, -0.4], ease);
    project.audio.gain = keyframed(&[ms(25), ms(85)], &[0.7, 1.1], ease);
    EngineProject::new(project).expect("valid project")
}

#[test]
fn automation_is_evaluated_at_control_points_not_at_every_sample() {
    // A clip, its track, and the master each automate their gain (and the two pan) along a curve.
    let ease = Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.25, 0.1, 0.25, 1.0));
    let second = MediaTime::from_seconds(1).ticks();
    let mut clip = audio_clip("c1", "tone", 0, second);
    clip.audio.gain = keyframed(&[0, second], &[0.25, 1.0], ease);
    clip.audio.pan = keyframed(&[0, second], &[-0.5, 0.5], ease);
    let mut project = project(vec![track("a1", vec![clip])], vec!["tone"]);
    project.timeline.tracks[0].audio.gain = keyframed(&[0, second], &[1.0, 0.5], ease);
    project.timeline.tracks[0].audio.pan = keyframed(&[0, second], &[0.2, -0.2], ease);
    project.audio.gain = keyframed(&[0, second], &[0.9, 0.6], ease);
    let snapshot = EngineProject::new(project).expect("valid project");
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    let before = ENVELOPE_POINTS.get();
    renderer
        .render(&snapshot, 0, 4_096, &fixture, TransportState::Playing)
        .expect("render");
    let points = ENVELOPE_POINTS.get() - before;
    // Three envelopes (clip, track, master), one point every 32 samples and one more.
    assert!(
        points <= 3 * (4_096 / 32 + 1),
        "{points} evaluations for 3 envelopes of 4,096 samples"
    );
}

#[test]
fn automation_between_control_points_follows_its_curve_closely() {
    // Fast moves: gain between 0.25 and 1.5 every 50 ms, pan from side to side every 70 ms,
    // both eased. A constant source shows each channel's multiplier as it is.
    let ease_in_out =
        Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.42, 0.0, 0.58, 1.0));
    let ease = Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.25, 0.1, 0.25, 1.0));
    let ms = |value: i64| value * TICKS_PER_SECOND / 1_000;
    let mut clip = audio_clip("c1", "tone", 0, MediaTime::from_seconds(1).ticks());
    let gain_times: Vec<i64> = (0..=10).map(|index| ms(50 * index)).collect();
    let pan_times: Vec<i64> = (0..=8).map(|index| ms(70 * index)).collect();
    clip.audio.gain = keyframed(&gain_times, &[0.25, 1.5], ease_in_out);
    clip.audio.pan = keyframed(&pan_times, &[-1.0, 1.0], ease);
    let gain = clip.audio.gain.clone();
    let pan = clip.audio.pan.clone();
    let snapshot =
        EngineProject::new(project(vec![track("a1", vec![clip])], vec!["tone"])).expect("valid");
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    let total = RATE as usize / 2;
    let mixed = mix_both(&snapshot, &fixture, 0, total, 512);
    let mut worst = 0.0_f32;
    let mut exact_samples = 0;
    for (index, mixed) in mixed[0].iter().zip(&mixed[1]).enumerate() {
        let time = MediaTime::from_ticks(ticks_at(i64::try_from(index).expect("small"), RATE));
        let gain = sample_scalar(&gain, time).clamp(0.0, MAX_GAIN);
        let weights = pan_gains(sample_scalar(&pan, time), 2);
        for (mixed, weight) in [mixed.0, mixed.1].into_iter().zip(weights) {
            let error = (mixed - gain * weight).abs();
            worst = worst.max(error);
            exact_samples += usize::from(error == 0.0);
        }
        if gain_times.contains(&time.ticks()) {
            assert_eq!(
                (gain * weights[0]).to_bits(),
                mixed.0.to_bits(),
                "sample {index}"
            );
        }
    }
    // The multiplier reaches 2.1 (1.5 hard left, raised 3 dB). Between control points it
    // stays within 0.001 of the curve evaluated on every sample, 66 dB below that peak; the
    // samples evaluated themselves (around each keyframe) match it exactly.
    println!("worst {worst}, {exact_samples} of {} exact", 2 * total);
    assert!(worst < 1.0e-3, "worst error {worst}");
}

#[test]
#[allow(clippy::float_cmp)]
fn held_keyframes_and_fade_edges_take_effect_on_their_own_sample() {
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    let second = MediaTime::from_seconds(1).ticks();
    // A held keyframe one tick after sample 1,000: sample 1,001 is the first to hear it.
    let step = ticks_at(1_000, RATE) + 1;
    let mut clip = audio_clip("c1", "tone", 0, second);
    clip.audio.gain = keyframed(&[0, step], &[0.5, 1.0], Interpolation::Hold);
    let snapshot =
        EngineProject::new(project(vec![track("a1", vec![clip])], vec!["tone"])).expect("valid");
    let [left, _] = mix_both(&snapshot, &fixture, 0, 2_048, 512);
    assert!(left[..=1_000].iter().all(|value| *value == 0.5));
    assert!(left[1_001..].iter().all(|value| *value == 1.0));

    // A clip starting on sample 2,006 (on a tick, so its source is not interpolated) fades in over
    // 100 samples and out over 98.
    let start = ticks_at(2_006, RATE);
    let end = ticks_at(4_006, RATE);
    let mut clip = audio_clip("c1", "tone", start, end);
    clip.audio.fade_in = MediaTime(ticks_at(100, RATE));
    clip.audio.fade_out = MediaTime(ticks_at(98, RATE));
    let snapshot =
        EngineProject::new(project(vec![track("a1", vec![clip])], vec!["tone"])).expect("valid");
    let [left, _] = mix_both(&snapshot, &fixture, 0, 4_096, 512);
    assert!(left[..=2_006].iter().all(|value| *value == 0.0));
    assert!(left[2_106..=3_908].iter().all(|value| *value == 1.0));
    assert!(left[4_006..].iter().all(|value| *value == 0.0));
    // An odd sample's time is half a tick late (2.5 ticks a sample), which moves the fade as
    // evaluated on it by up to 0.002 from the line through the even ones.
    for (index, value) in left.iter().enumerate().take(4_006).skip(2_006) {
        #[allow(clippy::cast_precision_loss)]
        let expected = ((index - 2_006) as f32 / 100.0)
            .min((4_006 - index) as f32 / 98.0)
            .min(1.0);
        assert!(
            (value - expected).abs() <= 0.5 / 245.0 + 1.0e-6,
            "sample {index}: {value}"
        );
    }
}

#[test]
fn automation_mixes_the_same_bits_in_any_block_and_after_a_seek() {
    let snapshot = automated_everywhere();
    let fixture = Fixture::ramp("tone", 2);
    let total = 9_000;
    let reference = mix_both(&snapshot, &fixture, 0, total, 4_096);
    for block in [512, 37, 1] {
        assert_eq!(
            mix_both(&snapshot, &fixture, 0, total, block),
            reference,
            "blocks of {block}"
        );
    }
    // A renderer that starts mid-interval mixes what playing from zero did there.
    for (start, block) in [(3_001, 100), (4_127, 64), (31, 1_000)] {
        let first = usize::try_from(start).expect("small");
        let seeked = mix_both(&snapshot, &fixture, start, 2_000, block);
        for channel in 0..2 {
            assert_eq!(
                seeked[channel],
                reference[channel][first..first + 2_000],
                "from {start} in blocks of {block}"
            );
        }
    }
}

#[test]
fn the_position_cursor_walks_exactly_like_dividing_every_sample() {
    let second = MediaTime::from_seconds(1).ticks();
    // Unity, half, double, and coprime speeds; clips starting off the sample grid; sources
    // starting mid-file.
    for (start, timeline, source_start, source) in [
        (0, second, 0, second),
        (0, 2 * second, 0, second),
        (12_345, second, 777, 2 * second),
        (777, 3 * second - 1, 33, 4 * second + 7),
    ] {
        let mut clip = audio_clip("c1", "tone", start, start + timeline);
        clip.source_range =
            TimeRange::new(MediaTime(source_start), MediaTime(source_start + source))
                .expect("range");
        let snapshot = rate_project(44_100, vec![clip]);
        let span = snapshot
            .evaluate_audio(
                TimeRange::new(MediaTime(0), MediaTime(start + timeline)).expect("range"),
            )
            .expect("span");
        let map = SourceMap::stretched(&span.clips[0], false, 44_100).expect("map");
        // From before timeline zero, as a pre-roll reads.
        let first = -5_000;
        let mut cursor = map.cursor(first).expect("cursor");
        let mut ticks = tick_cursor(first, 44_100);
        for sample in first..first + 50_000 {
            let (whole, fraction) = map.fraction(&cursor);
            let expected = map.position(sample).expect("position");
            assert_eq!(whole, expected.0, "sample {sample}");
            assert_eq!(fraction.to_bits(), expected.1.to_bits(), "sample {sample}");
            assert_eq!(ticks.whole, ticks_at(sample, 44_100), "sample {sample}");
            cursor.advance();
            ticks.advance();
        }
    }
}
