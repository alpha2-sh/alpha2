//! Exact and preview renderers, continuing a render, and what restarts from fresh state.

use super::*;

/// What a fresh renderer mixes at `start`.
fn fresh_block(
    snapshot: &EngineProject,
    provider: &dyn AudioBlockProvider,
    start: i64,
) -> Vec<f32> {
    render_steps(snapshot, provider, AudioContinuity::Exact, &[start], 512)
        .pop()
        .expect("one block")
        .0
}

#[test]
fn an_exact_renderer_mixes_any_step_as_a_fresh_renderer_does() {
    // The default policy: a one-sample nudge either way, a block's step back, and a jump a
    // sample longer than a block all reset and replay, as a seek does.
    let rate = i64::from(RATE_LOW);
    let total = RATE_LOW as usize * 14;
    let sources = settling_sources(total);
    let whole = low_rate_window(&sources, 0, total);
    let snapshot = settling_project();
    assert_eq!(
        AudioRenderer::new(&snapshot, 2, 512)
            .expect("renderer")
            .continuity(),
        AudioContinuity::Exact
    );
    let at = 12 * rate;
    for step in [1, -1, -512, 513] {
        let steps = render_steps(
            &snapshot,
            &whole,
            AudioContinuity::Exact,
            &[at - 512, at + step],
            512,
        );
        assert!(steps[1].1 > 1, "step {step} replays");
        let fresh = fresh_block(&snapshot, &whole, at + step);
        assert!(
            steps[1]
                .0
                .iter()
                .map(|value| value.to_bits())
                .eq(fresh.iter().map(|value| value.to_bits())),
            "step {step} mixes what a fresh renderer does"
        );
    }
}

#[test]
fn a_preview_renderer_continues_a_step_of_a_block_or_less_without_replaying() {
    let rate = i64::from(RATE_LOW);
    let total = RATE_LOW as usize * 14;
    let sources = settling_sources(total);
    let whole = low_rate_window(&sources, 0, total);
    let snapshot = settling_project();
    let at = 12 * rate;
    let lookahead = i64::from(
        AudioRenderer::new(&snapshot, 2, 512)
            .expect("renderer")
            .required_lookahead_samples(),
    );
    for step in [1, -1, 512, -512] {
        let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
        renderer.set_continuity(AudioContinuity::ContinueSmallGaps);
        renderer
            .render(&snapshot, at - 512, 512, &whole, TransportState::Playing)
            .expect("render");
        // It asks for no history: only the block and its lookahead.
        let needs = renderer
            .audio_needs(&snapshot, at + step, 512)
            .expect("needs");
        let earliest = needs
            .iter()
            .map(|need| need.source_range.start().ticks())
            .min()
            .expect("heard");
        assert!(
            earliest >= ticks_at(at + step - 1, RATE_LOW),
            "step {step} reads from tick {earliest}"
        );
        let latest = needs
            .iter()
            .map(|need| need.source_range.end().ticks())
            .max()
            .expect("heard");
        assert!(latest <= ticks_at(at + step + 512 + lookahead + 1, RATE_LOW));
        let (first, end) = needed_samples(&needs, RATE_LOW);
        let before = renderer.blocks_mixed;
        let status = renderer
            .render(
                &snapshot,
                at + step,
                512,
                &low_rate_window(&sources, first, end),
                TransportState::Playing,
            )
            .expect("render");
        assert!(status.is_complete(), "step {step}");
        assert_eq!(
            renderer.blocks_mixed - before,
            1,
            "step {step} replays nothing"
        );
        // Its state carried over: not what a fresh renderer mixes there.
        assert_ne!(
            renderer.channel(0)[..512].to_vec(),
            fresh_block(&snapshot, &whole, at + step),
            "step {step}"
        );
    }
    // A step longer than a block, and the first block after a reset, are a seek.
    for step in [513, -513] {
        let steps = render_steps(
            &snapshot,
            &whole,
            AudioContinuity::ContinueSmallGaps,
            &[at - 512, at + step],
            512,
        );
        assert!(steps[1].1 > 1, "step {step} replays");
        assert_eq!(
            steps[1].0,
            fresh_block(&snapshot, &whole, at + step),
            "step {step}"
        );
    }
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    renderer.set_continuity(AudioContinuity::ContinueSmallGaps);
    renderer
        .render(&snapshot, at - 512, 512, &whole, TransportState::Playing)
        .expect("render");
    renderer.reset();
    renderer
        .render(&snapshot, at, 512, &whole, TransportState::Playing)
        .expect("render");
    assert_eq!(
        renderer.channel(0)[..512],
        fresh_block(&snapshot, &whole, at)[..]
    );
}

#[test]
fn a_clip_effect_carries_its_tail_across_a_small_step() {
    // An impulse into a clip's reverb, then silence: the reverb's tail after a step is the
    // tail uninterrupted playback hears, so the clip's chain neither reset nor replayed.
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut clip = audio_clip("c1", "tone", 0, seconds);
    clip.audio.effects = vec![alpha2_model::AudioEffect::Reverb {
        room_size: 0.8,
        damping: 0.2,
        mix: 1.0,
    }];
    let snapshot = rate_project(RATE, vec![clip]);
    let impulse = impulse_at(0, RATE as usize);
    let uninterrupted = render_steps(
        &snapshot,
        &impulse,
        AudioContinuity::Exact,
        &[0, 512, 1_024, 1_536, 2_048, 2_560, 3_072],
        512,
    );
    for step in [1, -1, 300, -512] {
        let stepped = render_steps(
            &snapshot,
            &impulse,
            AudioContinuity::ContinueSmallGaps,
            &[0, 512, 1_024, 1_536, 2_048, 2_560 + step],
            512,
        );
        let tail = &stepped[5];
        assert_eq!(tail.1, 1, "step {step} replays nothing");
        assert!(
            tail.0.iter().any(|value| value.abs() > 1e-4),
            "step {step}: the tail plays on"
        );
        assert_eq!(tail.0, uninterrupted[5].0, "step {step}");
    }
    // A seek there instead starts the reverb from the silence before it.
    let sought = fresh_block(&snapshot, &impulse, 2_561);
    assert_ne!(sought, uninterrupted[5].0);
}

#[test]
fn a_preview_renderer_mixes_the_same_bits_for_the_same_calls() {
    let rate = i64::from(RATE_LOW);
    let total = RATE_LOW as usize * 14;
    let sources = settling_sources(total);
    let whole = low_rate_window(&sources, 0, total);
    let snapshot = settling_project();
    let starts = [
        11 * rate,
        11 * rate + 512,
        11 * rate + 1_025,
        11 * rate + 1_400,
        11 * rate + 1_400,
        11 * rate + 1_911,
        12 * rate + 3,
    ];
    let first = render_steps(
        &snapshot,
        &whole,
        AudioContinuity::ContinueSmallGaps,
        &starts,
        512,
    );
    let second = render_steps(
        &snapshot,
        &whole,
        AudioContinuity::ContinueSmallGaps,
        &starts,
        512,
    );
    for ((a, a_blocks), (b, b_blocks)) in first.iter().zip(&second) {
        assert_eq!(a_blocks, b_blocks);
        assert!(
            a.iter()
                .map(|value| value.to_bits())
                .eq(b.iter().map(|value| value.to_bits()))
        );
    }
}

#[test]
fn choosing_another_continuity_resets_the_renderer() {
    let rate = i64::from(RATE_LOW);
    let total = RATE_LOW as usize * 14;
    let sources = settling_sources(total);
    let whole = low_rate_window(&sources, 0, total);
    let snapshot = settling_project();
    let at = 12 * rate;
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    renderer.set_continuity(AudioContinuity::ContinueSmallGaps);
    renderer
        .render(&snapshot, at - 513, 512, &whole, TransportState::Playing)
        .expect("render");
    renderer
        .render(&snapshot, at - 512, 512, &whole, TransportState::Playing)
        .expect("continue");
    // Exact from here: even the next sample is mixed as a fresh renderer mixes it.
    renderer.set_continuity(AudioContinuity::Exact);
    renderer
        .render(&snapshot, at, 512, &whole, TransportState::Playing)
        .expect("render");
    assert_eq!(
        renderer.channel(0)[..512],
        fresh_block(&snapshot, &whole, at)[..]
    );
    renderer.set_continuity(AudioContinuity::Exact);
    let before = renderer.blocks_mixed;
    renderer
        .render(&snapshot, at + 512, 512, &whole, TransportState::Playing)
        .expect("render");
    assert_eq!(renderer.blocks_mixed - before, 1);
}

#[test]
fn a_snapshot_in_another_order_still_mixes_by_track_and_clip_identity() {
    // Tracks with distinct pans, gains, and a clip EQ, so a clip mixed into the wrong bus or
    // through the wrong chain changes the output.
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut first = track("a1", vec![audio_clip("c1", "tone", 0, seconds)]);
    first.audio.pan = Animated::Constant { value: -0.7 };
    first.clips[0].audio.effects = vec![alpha2_model::AudioEffect::Eq {
        bands: vec![alpha2_model::EqBand {
            kind: alpha2_model::EqBandKind::LowPass,
            frequency_hz: 500.0,
            q: 0.7,
            gain_db: 0.0,
        }],
    }];
    let mut second = track(
        "a2",
        vec![
            audio_clip("c2", "voice", 0, seconds / 2),
            audio_clip("c3", "tone", seconds / 4, seconds),
        ],
    );
    second.audio.gain = Animated::Constant { value: 0.4 };
    second.audio.pan = Animated::Constant { value: 0.9 };
    let built = project(vec![first, second], vec!["tone", "voice"]);
    let mut reordered = built.clone();
    reordered.timeline.tracks.reverse();
    reordered.timeline.tracks[0].clips.reverse();
    let fixture = Fixture::new(vec![("tone", 0.5, 2), ("voice", -0.25, 1)]);

    let snapshot = EngineProject::new(built).expect("valid project");
    let other = EngineProject::new(reordered).expect("valid project");
    let mut own = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let mut foreign = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    for start in (0..24_000).step_by(512) {
        own.render(&snapshot, start, 512, &fixture, TransportState::Playing)
            .expect("render");
        foreign
            .render(&other, start, 512, &fixture, TransportState::Playing)
            .expect("render");
        for channel in 0..2 {
            let bits = |renderer: &AudioRenderer| {
                renderer.channel(channel)[..512]
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>()
            };
            assert_eq!(bits(&foreign), bits(&own), "block at {start}");
        }
    }
}

#[test]
fn no_sample_inside_a_clip_is_dropped_at_a_block_boundary() {
    for rate in [44_100, 48_000, 96_000] {
        let seconds = MediaTime::from_seconds(1).ticks();
        let snapshot = rate_project(rate, vec![audio_clip("c1", "tone", 0, seconds)]);
        let provider = Plane(AudioBlock {
            start_sample: 0,
            sample_rate: rate,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![vec![0.5; rate as usize + 8]; 2],
        });
        for block in [4_096, 128, 37] {
            let output = render_all(&snapshot, &provider, rate as usize, block);
            let zeros = output.iter().filter(|sample| **sample == 0.0).count();
            assert_eq!(zeros, 0, "{rate} Hz in blocks of {block}");
        }
    }
}

#[test]
fn a_render_that_does_not_continue_the_last_starts_from_fresh_state() {
    // An export after preview: the renderer has played on with reverb state, then is asked
    // for the start again.
    let seconds = MediaTime::from_seconds(2).ticks();
    let mut project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    project.audio.effects = vec![alpha2_model::AudioEffect::Reverb {
        room_size: 0.8,
        damping: 0.2,
        mix: 0.5,
    }];
    let snapshot = EngineProject::new(project).expect("valid project");
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let render = |renderer: &mut AudioRenderer, start: i64| {
        renderer
            .render(&snapshot, start, 512, &fixture, TransportState::Playing)
            .expect("render");
        (0..2)
            .flat_map(|channel| renderer.channel(channel).to_vec())
            .map(f32::to_bits)
            .collect::<Vec<_>>()
    };
    let mut fresh = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let expected = [render(&mut fresh, 0), render(&mut fresh, 512)];

    let mut previewed = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    for block in 0..40 {
        render(&mut previewed, 24_000 + block * 512);
    }
    assert_eq!(render(&mut previewed, 0), expected[0], "a jump back resets");
    assert_eq!(render(&mut previewed, 512), expected[1], "then continues");
}
