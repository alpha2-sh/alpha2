//! Sample-exact paths: centred clips, fade-outs at the clip edge, and every path through the graph.

use super::*;

#[test]
fn a_centred_clip_and_track_pass_their_source_through_bit_for_bit() {
    // The centre weight must be exactly one, not 0.99999994 applied at both pan stages.
    let seconds = MediaTime::from_seconds(1).ticks();
    let project = project(
        vec![track("a1", vec![audio_clip("c1", "tone", 0, seconds)])],
        vec!["tone"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    let output = render_all(&snapshot, &distinct_plane(), 4_096, 512);
    let Plane(source) = distinct_plane();
    assert_eq!(
        output
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        source.channels[0][..4_096]
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    assert_eq!(pan_gains(0.0, 2).map(f32::to_bits), [1.0_f32.to_bits(); 2]);
}

#[test]
fn a_fade_out_reaches_silence_at_the_clip_edge_even_after_an_effect_tail() {
    let second = MediaTime::from_seconds(1).ticks();
    for effects in [Vec::new(), vec![lookahead()]] {
        for shape in [
            alpha2_model::FadeShape::Linear,
            alpha2_model::FadeShape::EqualPower,
        ] {
            let mut clip = audio_clip("c1", "tone", 0, second);
            clip.audio.fade_out = MediaTime(second / 2);
            clip.audio.fade_shape = shape;
            clip.audio.effects.clone_from(&effects);
            let mut base = project(vec![track("a1", vec![clip])], vec!["tone"]);
            base.audio.limiter = None;
            let snapshot = EngineProject::new(base).expect("valid project");
            let fixture = Fixture::new(vec![("tone", 1.0, 2)]);
            let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
            let last = i64::from(RATE) - 16;
            renderer
                .render(&snapshot, last, 16, &fixture, TransportState::Playing)
                .expect("render");
            let edge = renderer.channel(0)[15];
            assert!(edge.abs() < 1.0e-3, "{shape:?} {}: {edge}", effects.len());
            // Where the fade starts, the clip is at full level.
            let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
            renderer
                .render(
                    &snapshot,
                    i64::from(RATE) / 4,
                    16,
                    &fixture,
                    TransportState::Playing,
                )
                .expect("render");
            let start = renderer.channel(0)[0];
            assert!((start - 1.0).abs() < 1.0e-3, "{shape:?}: {start}");
        }
    }
}

#[test]
fn every_path_through_the_graph_lands_on_the_same_sample() {
    let seconds = MediaTime::from_seconds(1).ticks();
    for place in ["master", "track", "clip"] {
        let mut clip = audio_clip("c1", "tone", 0, seconds);
        let mut project = project(vec![track("a1", Vec::new())], vec!["tone"]);
        match place {
            "master" => project.audio.effects = vec![lookahead()],
            "track" => project.timeline.tracks[0].audio.effects = vec![lookahead()],
            _ => clip.audio.effects = vec![lookahead()],
        }
        // A plain track at half level, and a plain clip beside the latent one.
        let mut plain = track("a2", vec![audio_clip("c2", "tone", 0, seconds)]);
        plain.audio.gain = Animated::Constant { value: 0.5 };
        let mut beside = audio_clip("c3", "tone", 0, seconds);
        beside.audio.gain = Animated::Constant { value: 0.25 };
        project.timeline.tracks[0].clips.push(clip);
        project.timeline.tracks[0].clips.push(beside);
        project.timeline.tracks.push(plain);
        let snapshot = EngineProject::new(project).expect("valid project");
        let output = render_all(&snapshot, &impulse_at(1_000, RATE as usize), 4_096, 512);
        // 0.5 + 0.25 + 0.125 at one sample, and nothing anywhere else.
        assert!(
            (output[1_000] - 0.875).abs() < 1.0e-6,
            "{place}: {}",
            output[1_000]
        );
        let stray = output
            .iter()
            .enumerate()
            .filter(|&(index, sample)| index != 1_000 && sample.abs() > 1.0e-6)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(stray, Vec::<usize>::new(), "{place}");
    }
}
