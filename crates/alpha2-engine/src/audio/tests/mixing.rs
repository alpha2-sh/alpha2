//! Mixing: unity clips, mute and solo, fades, block size, missing sources, panning, speed, ducking, and overlapping clips.

use super::*;

#[test]
fn a_unity_clip_reaches_the_master_output_unchanged() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let (renderer, status) = render(project, 128, &fixture);

    assert!(status.is_complete());
    assert_eq!(status.frames, 128);
    // A centred clip passes through both pan stages unchanged.
    let expected = 0.5_f32;
    for channel in 0..2 {
        for &value in &renderer.channel(channel)[..128] {
            assert!((value - expected).abs() < 1.0e-5, "{value} != {expected}");
        }
    }
}

#[test]
fn muted_and_unsoloed_tracks_contribute_silence() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![
            track("a1", vec![audio_clip("c1", "tone", 0, seconds)]),
            track("a2", vec![audio_clip("c2", "tone", 0, seconds)]),
        ],
        vec!["tone"],
    );
    base.timeline.tracks[0].audio.muted = true;
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let (renderer, _) = render(base.clone(), 64, &fixture);
    let muted_sum: f32 = renderer.channel(0)[..64].iter().sum();

    let mut solo = base.clone();
    solo.timeline.tracks[0].audio.muted = false;
    solo.timeline.tracks[1].audio.solo = true;
    let (soloed, _) = render(solo, 64, &fixture);

    let solo_sum: f32 = soloed.channel(0)[..64].iter().sum();
    assert!((muted_sum - solo_sum).abs() < 1.0e-4);

    let mut clip_muted = base;
    clip_muted.timeline.tracks[0].audio.muted = true;
    clip_muted.timeline.tracks[1].clips[0].audio.muted = true;
    let (silent, _) = render(clip_muted, 64, &fixture);
    assert!(silent.channel(0)[..64].iter().all(|value| *value == 0.0));
}

#[test]
fn a_fade_and_the_same_ramp_as_gain_keyframes_mix_alike() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    base.timeline.tracks[0].clips[0].audio.fade_in = MediaTime::from_seconds(1);
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    let (renderer, _) = render(base.clone(), 480, &fixture);

    let channel = renderer.channel(0);
    assert!(channel[0].abs() < 1.0e-6);
    let quarter = channel[120];
    assert!((quarter - 120.0 / 48_000.0).abs() < 1.0e-4, "{quarter}");

    // The same ramp authored as keyframes produces the same envelope.
    let mut automated = base;
    automated.timeline.tracks[0].clips[0].audio.fade_in = MediaTime::ZERO;
    automated.timeline.tracks[0].clips[0].audio.gain = Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime::ZERO,
                value: 0.0,
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                time: MediaTime::from_seconds(1),
                value: 1.0,
                interpolation: Interpolation::Linear,
            },
        ],
    };
    let (keyframed, _) = render(automated, 480, &fixture);
    for (index, expected) in channel.iter().enumerate().take(480) {
        let difference = (keyframed.channel(0)[index] - expected).abs();
        assert!(
            difference < 1.0e-5,
            "sample {index} differs by {difference}"
        );
    }
}

#[test]
fn block_size_does_not_change_the_mixed_result() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    base.timeline.tracks[0].clips[0].audio.fade_in = MediaTime::from_seconds(1);
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);

    let snapshot = EngineProject::new(base).expect("valid project");
    let mut whole = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    whole
        .render(&snapshot, 0, 480, &fixture, TransportState::Playing)
        .expect("render");
    let reference: Vec<f32> = whole.channel(0)[..480].to_vec();

    let mut split = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let mut produced = Vec::new();
    for start in (0..480).step_by(37) {
        let frames = 37.min(480 - start);
        let status = split
            .render(
                &snapshot,
                i64::try_from(start).expect("start"),
                frames,
                &fixture,
                TransportState::Playing,
            )
            .expect("render");
        produced.extend_from_slice(&split.channel(0)[..status.frames]);
    }
    assert_eq!(produced.len(), reference.len());
    for (index, (left, right)) in produced.iter().zip(&reference).enumerate() {
        assert!((left - right).abs() < 1.0e-6, "sample {index} differs");
    }
}

#[test]
fn a_missing_source_reports_itself_and_mixes_silence() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]).without("tone");
    let (renderer, status) = render(project, 64, &fixture);

    assert!(!status.is_complete());
    assert_eq!(status.missing_sources.len(), 1);
    assert_eq!(status.missing_sources[0].as_str(), "tone");
    assert!(renderer.channel(0)[..64].iter().all(|value| *value == 0.0));
}

#[test]
fn panning_uses_constant_power_weights() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    base.timeline.tracks[0].clips[0].audio.pan = Animated::Constant { value: -1.0 };
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    let (renderer, _) = render(base, 32, &fixture);

    // Hard left keeps constant power: the live channel gains 3 dB.
    assert!((renderer.channel(0)[0] - core::f32::consts::SQRT_2).abs() < 1.0e-5);
    assert!(renderer.channel(1)[0].abs() < 1.0e-5);
}

#[test]
fn speed_resamples_the_source_and_keeps_video_alignment() {
    // A clip consuming two seconds of source over one timeline second.
    let mut clip = audio_clip("c1", "tone", 0, MediaTime::from_seconds(1).ticks());
    clip.source_range = TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(2)).expect("range");
    let project = project(vec![track("a1", vec![clip])], vec!["tone"]);
    let snapshot = EngineProject::new(project).expect("valid project");
    let span = snapshot
        .evaluate_audio(TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(1)).expect("range"))
        .expect("span");
    assert!((span.clips[0].speed() - 2.0).abs() < 1.0e-9);

    // A ramp names the source position each output sample read, so a wrong rate or offset
    // changes the values, not only the timing.
    let fixture = Fixture::ramp("tone", 2);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let first = i64::from(RATE) / 4;
    let status = renderer
        .render(&snapshot, first, 64, &fixture, TransportState::Playing)
        .expect("render");
    assert!(status.is_complete());
    let rate = i64::from(RATE);
    for (offset, sample) in renderer.channel(0)[..64].iter().enumerate() {
        let n = first + i64::try_from(offset).expect("offset");
        let position = 2 * n;
        // Where a sample's time falls on a tick, the source time video reads there is the same
        // position.
        if (n * TICKS_PER_SECOND) % rate == 0 {
            let at = MediaTime::from_ticks(n * TICKS_PER_SECOND / rate);
            let source = span.clips[0].source_time_at(at).expect("inside the clip");
            assert_eq!(
                source.ticks() * rate,
                position * TICKS_PER_SECOND,
                "video at sample {n}"
            );
        }
        #[allow(clippy::cast_precision_loss)]
        let expected = position as f32 / RAMP_SAMPLES as f32;
        assert!(
            (sample - expected).abs() < 1.0e-6,
            "sample {n} read {sample}, not source sample {position} ({expected})"
        );
    }
}

#[test]
fn ducking_attenuates_from_the_sidechain_track() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![
            track("music", vec![audio_clip("c1", "tone", 0, seconds)]),
            track("voice", vec![audio_clip("c2", "voice", 0, seconds)]),
        ],
        vec!["tone", "voice"],
    );
    base.timeline.tracks[0].audio.ducking = Some(Ducking {
        sidechain_track: TrackId::new("voice".to_owned()).expect("id"),
        threshold: 0.05,
        depth_db: -24.0,
        attack_ms: 1.0,
        release_ms: 50.0,
    });
    let loud = Fixture::new(vec![("tone", 0.5, 2), ("voice", 0.9, 2)]);
    let (ducked, _) = render(base.clone(), 512, &loud);

    let mut unducked_project = base;
    unducked_project.timeline.tracks[0].audio.ducking = None;
    let (unducked, _) = render(unducked_project, 512, &loud);

    assert!(ducked.channel(0)[511].abs() < unducked.channel(0)[511].abs());
}

/// Builds a project whose single track may be visual, so a video clip with
/// an audio stream can be mixed without a second clip.
fn video_project(has_audio: bool) -> Project {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    project.sources[0].kind = MediaKind::Video;
    project.sources[0].has_audio = has_audio;
    project.timeline.tracks[0].kind = alpha2_model::TrackKind::Video;
    project
}

#[test]
fn a_video_clip_contributes_audio_when_its_source_declares_a_stream() {
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let (audible, status) = render(video_project(true), 64, &fixture);
    assert!(status.is_complete());
    assert!((audible.channel(0)[0] - 0.5).abs() < 1.0e-5);

    // Without the declaration the same clip stays silent and needs no media.
    let (silent, status) = render(video_project(false), 64, &fixture);
    assert!(status.is_complete());
    assert!(silent.channel(0)[..64].iter().all(|value| *value == 0.0));
}

#[test]
fn overlapping_clips_on_one_track_sum_into_its_bus() {
    let second = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![track(
            "a1",
            vec![
                audio_clip("c1", "tone", 0, second),
                audio_clip("c2", "tone", second / 2, second * 2),
            ],
        )],
        vec!["tone"],
    );
    // Overlapping audio needs no transition; the mixer sums it.
    EngineProject::new(base.clone()).expect("overlapping audio is valid");

    let fixture = Fixture::new(vec![("tone", 0.25, 2)]);
    let (renderer, _) = render(base.clone(), 64, &fixture);
    // The block starts before the overlap, so only the first clip is active.
    assert!((renderer.channel(0)[0] - 0.25).abs() < 1.0e-5);

    let snapshot = EngineProject::new(base.clone()).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let overlap_sample = i64::from(RATE) / 2 + 100;
    renderer
        .render(
            &snapshot,
            overlap_sample,
            32,
            &fixture,
            TransportState::Playing,
        )
        .expect("render");
    assert!((renderer.channel(0)[0] - 0.5).abs() < 1.0e-5);

    base.timeline.tracks[0].clips[1].audio.muted = true;
    let snapshot = EngineProject::new(base).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    renderer
        .render(
            &snapshot,
            overlap_sample,
            32,
            &fixture,
            TransportState::Playing,
        )
        .expect("render");
    assert!((renderer.channel(0)[0] - 0.25).abs() < 1.0e-5);
}

#[test]
fn equal_power_fades_hold_loudness_across_an_overlap() {
    let second = MediaTime::from_seconds(1).ticks();
    let overlap = MediaTime::from_ticks(second / 2);
    let build = |shape: alpha2_model::FadeShape| {
        let mut base = project(
            vec![track(
                "a1",
                vec![
                    audio_clip("c1", "tone", 0, second),
                    audio_clip("c2", "tone", second / 2, second + second / 2),
                ],
            )],
            vec!["tone"],
        );
        base.timeline.tracks[0].clips[0].audio.fade_out = overlap;
        base.timeline.tracks[0].clips[0].audio.fade_shape = shape;
        base.timeline.tracks[0].clips[1].audio.fade_in = overlap;
        base.timeline.tracks[0].clips[1].audio.fade_shape = shape;
        base
    };

    // Midpoint of the overlap: each clip sits at half its fade.
    let middle = i64::from(RATE) * 3 / 4;
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    let power = |project: Project| {
        let snapshot = EngineProject::new(project).expect("valid project");
        let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
        renderer
            .render(&snapshot, middle, 16, &fixture, TransportState::Playing)
            .expect("render");
        renderer.channel(0)[0]
    };

    // Uncorrelated material would sum in power, but this fixture is one constant source, so
    // the equal-power pair sums above unity and the linear pair lands exactly at unity.
    let linear = power(build(alpha2_model::FadeShape::Linear));
    let equal = power(build(alpha2_model::FadeShape::EqualPower));
    assert!((linear - 1.0).abs() < 1.0e-4, "{linear}");
    assert!(
        (equal - core::f32::consts::SQRT_2).abs() < 1.0e-3,
        "{equal}"
    );

    let edge = power(build(alpha2_model::FadeShape::EqualPower));
    assert!(edge > 0.0);
}
