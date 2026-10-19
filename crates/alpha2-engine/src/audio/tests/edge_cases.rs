//! Extreme positions, unsupported layouts, and fades at the end of the tick range.

use super::*;

#[test]
fn extreme_positions_return_errors_instead_of_overflowing() {
    // `renderAudio` takes its start from JavaScript as a BigInt, so any i64 reaches here, and
    // an overflow panics natively and traps in wasm.
    let end = i64::MAX;
    let late = end - MediaTime::from_seconds(1).ticks();
    for rate in [8_000, RATE, 96_000] {
        let mut project = project(
            vec![track(
                "a1",
                vec![
                    audio_clip("c1", "tone", 0, 1_000),
                    audio_clip("c2", "tone", late, end),
                ],
            )],
            vec!["tone"],
        );
        project.settings = ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), rate)
            .expect("settings");
        let snapshot = EngineProject::new(project).expect("valid project");
        let mut renderer = AudioRenderer::new(&snapshot, 2, 64).expect("renderer");
        let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
        let last_sample = sample_floor(end, rate);
        for start in [
            4_000_000_000_000_000_000,
            last_sample - 64,
            last_sample - 32,
            last_sample,
            i64::MAX - 64,
            i64::MAX,
        ] {
            let _ = renderer.render(&snapshot, start, 64, &fixture, TransportState::Playing);
        }
    }
    let project = project(vec![track("a1", Vec::new())], Vec::new());
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 64).expect("renderer");
    assert_eq!(
        renderer
            .render(
                &snapshot,
                4_000_000_000_000_000_000,
                64,
                &Fixture::new(Vec::new()),
                TransportState::Playing
            )
            .err(),
        Some(EngineError::InvalidAudioConfiguration),
        "a start whose timeline position does not fit in ticks"
    );
}

#[test]
fn renderer_rejects_unsupported_layouts_and_blocks() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    assert_eq!(
        AudioRenderer::new(&snapshot, 3, 512).err(),
        Some(EngineError::InvalidAudioConfiguration)
    );
    assert_eq!(
        AudioRenderer::new(&snapshot, 2, MAX_AUDIO_BLOCK + 1).err(),
        Some(EngineError::InvalidAudioConfiguration)
    );
    let mut renderer = AudioRenderer::new(&snapshot, 2, 128).expect("renderer");
    let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
    assert_eq!(
        renderer
            .render(&snapshot, -1, 64, &fixture, TransportState::Playing)
            .err(),
        Some(EngineError::InvalidAudioConfiguration)
    );
    assert_eq!(
        renderer
            .render(&snapshot, 0, 129, &fixture, TransportState::Playing)
            .err(),
        Some(EngineError::InvalidAudioConfiguration)
    );
}

#[test]
fn a_fade_near_the_end_of_the_tick_range_does_not_overflow() {
    // A seek pre-roll can put a clip with a latent chain before its start, where the fade
    // arithmetic must not subtract past i64.
    let mut clip = audio_clip("c1", "tone", 0, i64::MAX);
    clip.source_range = TimeRange::new(MediaTime(0), MediaTime(120_000)).expect("range");
    clip.audio.fade_in = MediaTime(120_000);
    clip.audio.effects = vec![lookahead()];
    let mut faded_out = audio_clip("c2", "tone", 0, i64::MAX - 600);
    faded_out.source_range = TimeRange::new(MediaTime(0), MediaTime(120_000)).expect("range");
    faded_out.audio.fade_in = MediaTime(120_000);
    faded_out.audio.fade_out = MediaTime(120_000);
    faded_out.audio.effects = vec![lookahead()];
    let project = project(
        vec![track("a1", vec![clip]), track("a2", vec![faded_out])],
        vec!["tone"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let status = renderer
        .render(&snapshot, 0, 4_096, &fixture, TransportState::Playing)
        .expect("render");
    assert!(status.is_complete());
}
