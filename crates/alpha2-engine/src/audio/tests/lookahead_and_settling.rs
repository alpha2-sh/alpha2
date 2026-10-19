//! Lookahead, and the stages that settle after a seek.

use super::*;

fn peak_index(samples: &[f32]) -> usize {
    samples
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(index, _)| index)
        .expect("samples")
}

#[test]
fn a_lookahead_anywhere_in_the_graph_does_not_delay_the_mix() {
    let seconds = MediaTime::from_seconds(1).ticks();
    for place in ["master", "track", "clip"] {
        let mut clip = audio_clip("c1", "tone", 0, seconds);
        let mut project = project(vec![track("a1", Vec::new())], vec!["tone"]);
        match place {
            "master" => project.audio.effects = vec![lookahead()],
            "track" => project.timeline.tracks[0].audio.effects = vec![lookahead()],
            _ => clip.audio.effects = vec![lookahead()],
        }
        // A second, plain track has to stay aligned with the first.
        let mut plain = track("a2", vec![audio_clip("c2", "tone", 0, seconds)]);
        plain.audio.gain = Animated::Constant { value: 0.0 };
        project.timeline.tracks[0].clips.push(clip);
        project.timeline.tracks.push(plain);
        let snapshot = EngineProject::new(project).expect("valid project");
        let output = render_all(&snapshot, &impulse_at(1_000, RATE as usize), 4_096, 512);
        assert_eq!(peak_index(&output), 1_000, "lookahead on the {place}");
        let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
        assert_eq!(renderer.required_lookahead_samples(), 480, "{place}");
    }
}

#[test]
fn a_clip_lookahead_tail_plays_out_after_the_clip_ends() {
    // The impulse is ten samples before the clip's end: it leaves the compressor 480 samples
    // after it went in, when the clip is over.
    let end = sample_ticks_floor(1_010, RATE);
    let mut clip = audio_clip("c1", "tone", 0, end);
    clip.audio.effects = vec![lookahead()];
    let snapshot = EngineProject::new(project(vec![track("a1", vec![clip])], vec!["tone"]))
        .expect("valid project");
    let output = render_all(&snapshot, &impulse_at(1_000, RATE as usize), 4_096, 256);
    assert_eq!(peak_index(&output), 1_000);
    assert!((output[1_000] - 0.5).abs() < 1e-6, "{}", output[1_000]);
    assert!(output[1_010..].iter().all(|sample| *sample == 0.0));
}

#[test]
fn a_seek_pre_rolls_the_lookahead_to_match_continuous_playback() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    project.audio.effects = vec![lookahead()];
    let snapshot = EngineProject::new(project).expect("valid project");
    #[allow(clippy::cast_precision_loss)]
    let ramp = (0..RATE as usize)
        .map(|index| (index % 997) as f32 / 997.0 * 0.1)
        .collect::<Vec<_>>();
    let provider = Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: alpha2_media::SpeakerLayout::STEREO,
        channels: vec![ramp.clone(), ramp],
    });
    let continuous = render_all(&snapshot, &provider, 8_192, 512);
    let mut seeked = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    seeked
        .render(&snapshot, 4_096, 512, &provider, TransportState::Playing)
        .expect("render");
    assert_eq!(&seeked.channel(0)[..512], &continuous[4_096..4_608]);
}

#[test]
fn seeking_restores_real_compression_attack_and_release_at_every_level() {
    let seconds = MediaTime::from_seconds(7).ticks();
    let mut signal = vec![0.5; RATE as usize * 7];
    signal[RATE as usize * 5..].fill(0.02);
    let provider = Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: alpha2_media::SpeakerLayout::STEREO,
        channels: vec![signal.clone(), signal],
    });
    for place in ["master", "track", "clip", "serial"] {
        for lookahead_ms in [0.0, 10.0] {
            let mut project = project(
                vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
                vec!["tone"],
            );
            let effect = alpha2_model::AudioEffect::Compressor {
                threshold_db: -18.0,
                ratio: 4.0,
                attack_ms: 50.0,
                release_ms: 100.0,
                lookahead_ms,
            };
            match place {
                "master" => project.audio.effects.push(effect),
                "track" => project.timeline.tracks[0].audio.effects.push(effect),
                "serial" => {
                    project.audio.effects.push(effect.clone());
                    project.timeline.tracks[0]
                        .audio
                        .effects
                        .push(effect.clone());
                    project.timeline.tracks[0].clips[0]
                        .audio
                        .effects
                        .push(effect);
                }
                _ => project.timeline.tracks[0].clips[0]
                    .audio
                    .effects
                    .push(effect),
            }
            let snapshot = EngineProject::new(project).expect("project");
            let continuous = render_all(&snapshot, &provider, RATE as usize * 6, 512);
            assert!(continuous[8_192] < 0.25, "fixture must actually compress");
            let mut sought = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
            // Early attack, settled envelope, release, and reverse seek; discontinuities must
            // reset the previous position's state.
            for target in [8_192, 4 * i64::from(RATE), 5 * i64::from(RATE) + 111, 8_192] {
                let (first, end) = needed_samples(
                    &sought.audio_needs(&snapshot, target, 512).expect("needs"),
                    RATE,
                );
                let window = Plane(AudioBlock {
                    start_sample: i64::try_from(first).expect("start"),
                    sample_rate: RATE,
                    speakers: provider.0.speakers,
                    channels: provider
                        .0
                        .channels
                        .iter()
                        .map(|channel| channel[first..end].to_vec())
                        .collect(),
                });
                let status = sought
                    .render(&snapshot, target, 512, &window, TransportState::Playing)
                    .expect("seek");
                assert!(status.is_complete());
                let offset = usize::try_from(target).expect("target");
                for (actual, expected) in sought.channel(0)[..512]
                    .iter()
                    .zip(&continuous[offset..offset + 512])
                {
                    assert!(
                        (actual - expected).abs() < 1e-6,
                        "{place}, lookahead {lookahead_ms}, target {target}: {actual} != {expected}"
                    );
                }
            }
        }
    }
}

#[test]
fn stages_with_different_settling_times_all_settle_after_a_seek() {
    // Each stage joins a seek's replay at its own lead. 8 kHz keeps 26 seconds cheap.
    let rate = i64::from(RATE_LOW);
    let total = RATE_LOW as usize * 27;
    let sources = settling_sources(total);
    let snapshot = settling_project();

    let mut continuous = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    assert!(
        continuous.required_history_samples() > 10 * rate,
        "the reverb must dominate the history"
    );
    let whole = low_rate_window(&sources, 0, total);
    let mut reference = Vec::new();
    for start in (0..26 * rate).step_by(512) {
        continuous
            .render(&snapshot, start, 512, &whole, TransportState::Playing)
            .expect("render");
        reference.extend_from_slice(&continuous.channel(0)[..512]);
    }

    let mut sought = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    for target in [20 * rate, 25 * rate + 111, 21 * rate] {
        let (first, end) = needed_samples(
            &sought.audio_needs(&snapshot, target, 512).expect("needs"),
            RATE_LOW,
        );
        let status = sought
            .render(
                &snapshot,
                target,
                512,
                &low_rate_window(&sources, first, end),
                TransportState::Playing,
            )
            .expect("seek");
        assert!(status.is_complete());
        let offset = usize::try_from(target).expect("target");
        let worst = sought.channel(0)[..512]
            .iter()
            .zip(&reference[offset..offset + 512])
            .map(|(actual, expected)| (actual - expected).abs())
            .fold(0.0_f32, f32::max);
        assert!(worst < 1e-6, "target {target}: off by {worst}");
    }
}
