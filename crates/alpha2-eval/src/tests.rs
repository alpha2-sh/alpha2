use alpha2_media::{IdError, SourceId};
use alpha2_model::{
    Animated, AnimatedPoint, Clip, ClipId, Interpolation, Keyframe, MediaKind, MediaSource,
    Project, ProjectId, ProjectSettings, Timeline, Track, TrackId, TrackKind, Vec2,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use std::collections::BTreeMap;

use crate::*;

fn id<T>(make: impl FnOnce(String) -> Result<T, IdError>, value: &str) -> T {
    make(value.to_owned()).expect("test ID is non-empty")
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime(start), MediaTime(end)).expect("ordered range")
}

fn project() -> Project {
    let video = id(SourceId::new, "video");
    let audio = id(SourceId::new, "audio");
    let mut video_track = Track::new(id(TrackId::new, "v1"), "video", TrackKind::Video);
    video_track.clips.push(Clip::new(
        id(ClipId::new, "clip-v"),
        video.clone(),
        range(100, 200),
        range(1_000, 1_200),
    ));
    let mut audio_track = Track::new(id(TrackId::new, "a1"), "audio", TrackKind::Audio);
    audio_track.clips.push(Clip::new(
        id(ClipId::new, "clip-a"),
        audio.clone(),
        range(150, 250),
        range(2_000, 2_100),
    ));
    Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: id(ProjectId::new, "project"),
        name: "test".into(),
        audio: alpha2_model::MasterAudio::default(),
        settings: ProjectSettings::new(
            1,
            1,
            FrameRate::new(24, 1).expect("valid frame rate"),
            48_000,
        )
        .expect("valid settings"),
        sources: vec![
            MediaSource::new(video, "asset://video", MediaKind::Video),
            MediaSource::new(audio, "asset://audio", MediaKind::Audio),
        ],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline {
            tracks: vec![video_track, audio_track],
        },
    }
}

#[test]
fn index_finds_half_open_active_intervals() {
    let project = project();
    let index = ActiveIndex::build(&project).expect("valid project");
    assert_eq!(index.len(), 2);
    assert_eq!(
        evaluate(&project, &index, MediaTime(99))
            .expect("matching index")
            .visual,
        [] as [frame::ActiveClip<'_>; 0]
    );
    let at_start = evaluate(&project, &index, MediaTime(100)).expect("matching index");
    assert_eq!(at_start.visual[0].source_time, MediaTime(1_000));
    assert_eq!(
        evaluate(&project, &index, MediaTime(200))
            .expect("matching index")
            .visual,
        [] as [frame::ActiveClip<'_>; 0]
    );
}

#[test]
fn retime_map_is_integer_and_deterministic() {
    let project = project();
    let index = ActiveIndex::build(&project).expect("valid project");
    let frame = evaluate(&project, &index, MediaTime(175)).expect("matching index");
    assert_eq!(frame.visual[0].source_time, MediaTime(1_150));
    assert_eq!(frame.audio[0].source_time, MediaTime(2_025));
}

#[test]
fn frame_needs_are_deduplicated() {
    let mut project = project();
    let mut copied_track = project.timeline.tracks[0].clone();
    copied_track.id = id(TrackId::new, "v2");
    copied_track.clips[0].id = id(ClipId::new, "clip-v-copy");
    project.timeline.tracks.push(copied_track);
    let index = ActiveIndex::build(&project).expect("valid project");
    let needs = frame_needs(&project, &index, MediaTime(175)).expect("matching index");
    assert_eq!(needs.len(), 2);
    assert_eq!(needs[0].clip_id.as_str(), "clip-v");
}

#[test]
fn frame_needs_preserve_consumers_of_distinct_source_times() {
    let mut project = project();
    let mut copied_track = project.timeline.tracks[0].clone();
    copied_track.id = id(TrackId::new, "v2");
    copied_track.clips[0].id = id(ClipId::new, "clip-v-copy");
    copied_track.clips[0].source_range = range(3_000, 3_200);
    project.timeline.tracks.push(copied_track);
    let index = ActiveIndex::build(&project).expect("valid project");
    let needs = frame_needs(&project, &index, MediaTime(175)).expect("matching index");
    let visual = &needs[..2];
    assert_eq!(visual[0].clip_id.as_str(), "clip-v");
    assert_eq!(visual[0].media_time, MediaTime(1_150));
    assert_eq!(visual[1].clip_id.as_str(), "clip-v-copy");
    assert_eq!(visual[1].media_time, MediaTime(3_150));
}

fn keyframes<T>(points: [(i64, T, Interpolation); 2]) -> Animated<T> {
    Animated::Keyframes {
        keyframes: points
            .into_iter()
            .map(|(time, value, interpolation)| Keyframe {
                time: MediaTime(time),
                value,
                interpolation,
            })
            .collect(),
    }
}

fn visual_at(project: &Project, time: i64) -> EvaluatedVisual {
    let index = ActiveIndex::build(project).expect("valid project");
    evaluate(project, &index, MediaTime(time))
        .expect("matching index")
        .visual[0]
        .visual
        .expect("visual track")
}

#[test]
fn visual_animation_interpolates_numerically_including_rotation() {
    let mut project = project();
    let visual = &mut project.timeline.tracks[0].clips[0].visual;
    visual.position = AnimatedPoint::from(keyframes([
        (100, Vec2::new(0.0, 0.0), Interpolation::Linear),
        (200, Vec2::new(1.0, 1.0), Interpolation::Hold),
    ]));
    // Two turns, not the shortest path: 0 -> 720 is 360 halfway.
    visual.rotation_degrees = keyframes([
        (100, 0.0, Interpolation::Linear),
        (200, 720.0, Interpolation::Hold),
    ]);
    let halfway = visual_at(&project, 150);
    assert_eq!(halfway.position, Vec2::new(0.5, 0.5));
    assert!((halfway.rotation_degrees - 360.0).abs() < f32::EPSILON);

    // 350 -> 10 turns back through 180, as After Effects, Lottie, and CSS do.
    project.timeline.tracks[0].clips[0].visual.rotation_degrees = keyframes([
        (100, 350.0, Interpolation::Linear),
        (200, 10.0, Interpolation::Hold),
    ]);
    assert!((visual_at(&project, 150).rotation_degrees - 180.0).abs() < f32::EPSILON);
}

#[test]
fn a_cubic_bezier_segment_eases_between_its_keyframes() {
    let ease = Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.25, 0.1, 0.25, 1.0));
    let mut project = project();
    project.timeline.tracks[0].clips[0].visual.scale = keyframes([
        (100, Vec2::new(1.0, 2.0), ease),
        (200, Vec2::new(3.0, 2.0), Interpolation::Hold),
    ]);
    // CSS `ease` at x = 0.5 is 0.802403...; 1 + 2 * 0.802403 = 2.604807.
    let scale = visual_at(&project, 150).scale;
    assert!((scale.x - 2.604_807).abs() < 1e-5, "{scale:?}");
    assert!((scale.y - 2.0).abs() < f32::EPSILON);
    assert_eq!(visual_at(&project, 100).scale, Vec2::new(1.0, 2.0));
    assert_eq!(visual_at(&project, 199).scale.x.to_bits(), {
        #[allow(clippy::cast_possible_truncation)]
        let expected =
            (1.0 + 2.0 * alpha2_model::CubicBezier::new(0.25, 0.1, 0.25, 1.0).apply(0.99)) as f32;
        expected.to_bits()
    });
}

#[test]
fn an_overshooting_curve_keeps_opacity_in_its_domain_only() {
    let back = Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.34, 1.56, 0.64, 1.0));
    let mut project = project();
    let visual = &mut project.timeline.tracks[0].clips[0].visual;
    visual.opacity = keyframes([(100, 0.0, back), (200, 1.0, Interpolation::Hold)]);
    visual.position = AnimatedPoint::from(keyframes([
        (100, Vec2::new(0.0, 0.0), back),
        (200, Vec2::new(1.0, 1.0), Interpolation::Hold),
    ]));
    let evaluated = visual_at(&project, 170);
    assert!(evaluated.position.x > 1.0, "position keeps the overshoot");
    assert!(
        evaluated.opacity.to_bits() == 1.0_f32.to_bits(),
        "opacity is clamped"
    );
}

/// `project`'s first clip shown through one effect node.
fn with_effect(project: &mut Project, effect: alpha2_model::ClipEffect) {
    use alpha2_model::{
        ClipComposition, Composition, CompositionId, CompositionNode, NodeId, NodeOp,
    };
    let node = |value: &str| id(NodeId::new, value);
    let composition = id(CompositionId::new, "look");
    project.compositions = vec![Composition {
        id: composition.clone(),
        nodes: vec![
            CompositionNode::new(node("clip"), NodeOp::Clip {}),
            CompositionNode::new(
                node("fx"),
                NodeOp::Effect {
                    input: Some(node("clip")),
                    inputs: BTreeMap::new(),
                    effect,
                },
            ),
        ],
        output: Some(node("fx")),
        inputs: Vec::new(),
        controls: Vec::new(),
    }];
    project.timeline.tracks[0].clips[0].composition = Some(ClipComposition::new(composition));
}

/// The effect the output step of a clip's composition runs, at its
/// composition time.
fn output_effect<'a>(clip: &ActiveClip<'a>) -> ActiveEffect<'a> {
    let composition = clip.composition.expect("a composition");
    let plan = composition.plan();
    let StepOp::Effect { effect, .. } = &plan.steps[plan.output].op else {
        panic!("an effect output");
    };
    ActiveEffect::new(effect, composition.time())
}

#[test]
fn evaluation_preserves_effect_order_parameters_and_blend_mode() {
    let mut project = project();
    let effects = [alpha2_model::ClipEffect {
        id: "test.tint".into(),
        version: 2,
        resources: BTreeMap::new(),
        params: BTreeMap::from([(
            "tint".into(),
            Animated::Constant {
                value: alpha2_model::EffectValue::Color([0.5, 1.0, 1.0, 1.0]),
            },
        )]),
    }];
    project.timeline.tracks[0].clips[0].blend_mode = alpha2_model::ClipBlendMode::Screen;
    with_effect(&mut project, effects[0].clone());
    let index = ActiveIndex::build(&project).expect("valid project");
    let active = &evaluate(&project, &index, MediaTime(150))
        .expect("matching index")
        .visual[0];
    assert_eq!(active.blend_mode, alpha2_model::ClipBlendMode::Screen);
    let effect = output_effect(active);
    assert_eq!(effect.authored(), &effects[0]);
    assert_eq!((effect.id(), effect.version()), ("test.tint", 2));
    assert_eq!(
        effect.param("tint"),
        Some(EffectParamValue {
            value: alpha2_model::EffectValue::Color([0.5, 1.0, 1.0, 1.0]),
            interpolated: false,
        })
    );
}

#[test]
fn colors_interpolate_premultiplied() {
    use alpha2_model::EffectValue;

    let ramp = |start: [f32; 4], end: [f32; 4]| Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime(0),
                value: EffectValue::Color(start),
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                time: MediaTime(100),
                value: EffectValue::Color(end),
                interpolation: Interpolation::Hold,
            },
        ],
    };
    // Straight interpolation would give a half-dark red, [0.5, 0, 0, 0.5]; premultiplied, the red
    // keeps its value while its alpha rises.
    let fade = ramp([0.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(
        sample_effect_param(&fade, MediaTime(50)).value,
        EffectValue::Color([1.0, 0.0, 0.0, 0.5])
    );
    let opaque = ramp([0.0, 0.2, 1.0, 1.0], [1.0, 0.6, 0.0, 1.0]);
    let EffectValue::Color(mid) = sample_effect_param(&opaque, MediaTime(25)).value else {
        panic!("a color");
    };
    for (actual, expected) in mid.into_iter().zip([0.25, 0.3, 0.75, 1.0]) {
        assert!((actual - expected).abs() < 1e-6, "{mid:?}");
    }
}

#[test]
fn vec4_parameters_interpolate_component_by_component() {
    use alpha2_model::EffectValue;

    // color-phase's default `from_step` to its `to_step`: a fourth component of 0 is a threshold,
    // not a transparent color, so the first three must not jump to their end values.
    let steps = Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime(0),
                value: EffectValue::Vec4([0.0, 0.2, 0.4, 0.0]),
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                time: MediaTime(100),
                value: EffectValue::Vec4([0.6, 0.8, 1.0, 1.0]),
                interpolation: Interpolation::Hold,
            },
        ],
    };
    for (time, expected) in [
        (1, [0.006, 0.206, 0.406, 0.01]),
        (25, [0.15, 0.35, 0.55, 0.25]),
        (50, [0.3, 0.5, 0.7, 0.5]),
    ] {
        let EffectValue::Vec4(value) = sample_effect_param(&steps, MediaTime(time)).value else {
            panic!("a vec4");
        };
        for (actual, expected) in value.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-6, "{time}: {value:?}");
        }
    }
}

#[test]
fn effect_parameters_are_sampled_at_the_composition_time() {
    use alpha2_model::{CubicBezier, EffectValue};

    // The clip starts at 100, so composition time is the timeline time less 100.
    let animated =
        |start: EffectValue, end: EffectValue, curve: Interpolation| Animated::Keyframes {
            keyframes: vec![
                Keyframe {
                    time: MediaTime(0),
                    value: start,
                    interpolation: curve,
                },
                Keyframe {
                    time: MediaTime(100),
                    value: end,
                    interpolation: Interpolation::Hold,
                },
            ],
        };
    let back = Interpolation::CubicBezier(CubicBezier::new(0.34, 1.56, 0.64, 1.0));
    let mut project = project();
    with_effect(
        &mut project,
        alpha2_model::ClipEffect {
            id: "alpha2.effect.gaussian-blur".into(),
            version: 1,
            resources: BTreeMap::new(),
            params: BTreeMap::from([
                (
                    "radius".into(),
                    animated(
                        EffectValue::Float(0.0),
                        EffectValue::Float(0.5),
                        Interpolation::Linear,
                    ),
                ),
                (
                    "tint".into(),
                    animated(
                        EffectValue::Color([0.0, 0.0, 0.0, 1.0]),
                        EffectValue::Color([1.0, 1.0, 1.0, 1.0]),
                        back,
                    ),
                ),
                (
                    "mode".into(),
                    animated(
                        EffectValue::Enum(0),
                        EffectValue::Enum(3),
                        Interpolation::Hold,
                    ),
                ),
            ]),
        },
    );
    let index = ActiveIndex::build(&project).expect("valid project");
    let param = |time: i64, name: &str| {
        output_effect(
            &evaluate(&project, &index, MediaTime(time))
                .expect("matching index")
                .visual[0],
        )
        .param(name)
        .expect("authored parameter")
    };
    assert_eq!(
        param(100, "radius"),
        EffectParamValue {
            value: EffectValue::Float(0.0),
            interpolated: false
        }
    );
    assert_eq!(
        param(150, "radius"),
        EffectParamValue {
            value: EffectValue::Float(0.25),
            interpolated: true
        }
    );
    assert_eq!(param(199, "radius").value, EffectValue::Float(0.495));
    let EffectValue::Color(tint) = param(170, "tint").value else {
        panic!("a color");
    };
    assert!(back.amount(0.7).expect("a curve") > 1.0);
    assert_eq!(tint.map(f32::to_bits), [1.0_f32.to_bits(); 4]);
    // Enum keyframes hold. The clip ends at 200, so the last keyframe is sampled directly.
    assert_eq!(param(199, "mode").value, EffectValue::Enum(0));
    let alpha2_model::NodeOp::Effect { effect, .. } = &project.compositions[0].nodes[1].op else {
        panic!("an effect node");
    };
    assert_eq!(
        sample_effect_param(&effect.params["mode"], MediaTime(100)).value,
        EffectValue::Enum(3)
    );
}

/// A project whose single video track holds `frames` of transition between
/// two clips, starting on frame 3 at `rate`.
fn transition_project(rate: FrameRate, frames: i64) -> (Project, i64) {
    let mut project = project();
    project.settings = ProjectSettings::new(1, 1, rate, 48_000).expect("valid settings");
    let frame = |index: i64| rate.frame_start(index).expect("frame").ticks();
    let (start, end) = (frame(3), frame(3 + frames));
    let from = &mut project.timeline.tracks[0].clips[0];
    from.timeline_range = range(0, end);
    from.source_range = range(0, end);
    let mut to_clip = from.clone();
    to_clip.id = id(ClipId::new, "clip-to");
    to_clip.timeline_range = range(start, frame(60));
    to_clip.source_range = range(0, frame(60) - start);
    project.timeline.tracks[0].clips.push(to_clip);
    project.timeline.tracks[0].transitions = vec![alpha2_model::ClipTransition {
        from_clip: id(ClipId::new, "clip-v"),
        to_clip: id(ClipId::new, "clip-to"),
        timeline_range: range(start, end),
        easing: alpha2_model::Easing::Linear,
        effect: alpha2_model::ClipEffect {
            id: "alpha2.transition.fade".into(),
            version: 1,
            resources: BTreeMap::new(),
            params: BTreeMap::new(),
        },
    }];
    (project, start)
}

fn progress_at(project: &Project, time: i64) -> f32 {
    let index = ActiveIndex::build(project).expect("valid transition project");
    let frame = evaluate(project, &index, MediaTime(time)).expect("matching index");
    assert_eq!(frame.transitions.len(), 1, "at {time}");
    frame.transitions[0].progress
}

#[test]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn a_transitions_frames_sample_progress_at_their_centres() {
    // Frame i shows (i + 0.5) / N; played backwards it shows 1 - p.
    for rate in [
        FrameRate::new(30, 1).expect("rate"),
        FrameRate::new(24, 1).expect("rate"),
        FrameRate::new(30_000, 1_001).expect("rate"),
    ] {
        for frames in [1_i64, 2, 4, 30] {
            let (project, _) = transition_project(rate, frames);
            let progress = (0..frames)
                .map(|offset| {
                    progress_at(
                        &project,
                        rate.frame_start(3 + offset).expect("frame").ticks(),
                    )
                })
                .collect::<Vec<_>>();
            for (offset, value) in progress.iter().enumerate() {
                let expected = ((offset as f64 + 0.5) / frames as f64) as f32;
                assert_eq!(
                    value.to_bits(),
                    expected.to_bits(),
                    "{rate:?} N={frames} frame {offset}: {value} != {expected}"
                );
            }
            for (forward, backward) in progress.iter().zip(progress.iter().rev()) {
                assert!(
                    (forward + backward - 1.0).abs() < 1e-6,
                    "{rate:?} N={frames}"
                );
            }
        }
    }
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn progress_is_continuous_between_frames_and_eased_after_centring() {
    let rate = FrameRate::new(30, 1).expect("rate");
    let (mut project, start) = transition_project(rate, 4);
    let duration = 4 * 4_000;
    let mut previous = progress_at(&project, start);
    assert!((previous - 2_000.0 / duration as f32).abs() < 1e-6);
    for time in (start + 1..start + duration).step_by(97) {
        let value = progress_at(&project, time);
        let expected = ((time - start + 2_000) as f32 / duration as f32).min(1.0);
        assert!(
            (value - expected).abs() < 1e-6,
            "at {time}: {value} != {expected}"
        );
        assert!(value >= previous);
        previous = value;
    }
    assert_eq!(
        progress_at(&project, start + duration - 1).to_bits(),
        1.0_f32.to_bits()
    );

    for (x1, y1, x2, y2) in [
        (0.42, 0.0, 1.0, 1.0),
        (0.0, 0.0, 0.58, 1.0),
        (0.42, 0.0, 0.58, 1.0),
    ] {
        let easing =
            alpha2_model::Easing::CubicBezier(alpha2_model::CubicBezier::new(x1, y1, x2, y2));
        project.timeline.tracks[0].transitions[0].easing = easing;
        #[allow(clippy::cast_possible_truncation)]
        let expected = easing.apply(0.375) as f32;
        assert_eq!(
            progress_at(&project, start + 4_000).to_bits(),
            expected.to_bits(),
            "{easing:?}"
        );
    }

    // Transition shaders are defined on 0..=1, so an overshoot is clamped.
    let back = alpha2_model::CubicBezier::new(0.34, 1.56, 0.64, 1.0);
    assert!(back.apply(0.875) > 1.0);
    project.timeline.tracks[0].transitions[0].easing = alpha2_model::Easing::CubicBezier(back);
    assert_eq!(
        progress_at(&project, start + 3 * 4_000).to_bits(),
        1.0_f32.to_bits()
    );
}

#[test]
fn extreme_keyframe_times_and_values_remain_finite() {
    let mut project = project();
    let clip = &mut project.timeline.tracks[0].clips[0];
    clip.timeline_range = range(0, 1);
    clip.source_range = range(0, 1);
    clip.visual.rotation_degrees = Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime(i64::MIN),
                value: -3.0e38,
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                time: MediaTime(i64::MAX),
                value: 3.0e38,
                interpolation: Interpolation::Hold,
            },
        ],
    };
    let index = ActiveIndex::build(&project).expect("finite keyframes are valid");
    let rotation = evaluate(&project, &index, MediaTime::ZERO)
        .expect("extreme timeline interpolation")
        .visual[0]
        .visual
        .expect("video visual")
        .rotation_degrees;
    assert!(rotation.is_finite());
}

#[test]
fn audio_span_reports_overlap_and_ignores_visual_tracks() {
    let project = project();
    let index = ActiveIndex::build(&project).expect("valid project");

    // The audio clip occupies [150, 250); the video clip [100, 200) is excluded.
    let span = crate::evaluate_audio(&project, &index, range(100, 300)).expect("span");
    assert_eq!(span.clips.len(), 1);
    let clip = &span.clips[0];
    assert_eq!(clip.clip_id.as_str(), "clip-a");
    assert_eq!(clip.timeline_range, range(150, 250));
    assert_eq!(clip.active_range, range(150, 250));

    let span = crate::evaluate_audio(&project, &index, range(200, 220)).expect("span");
    assert_eq!(span.clips[0].active_range, range(200, 220));

    assert_eq!(
        crate::evaluate_audio(&project, &index, range(250, 300))
            .expect("span")
            .clips,
        [] as [audio::ActiveAudioClip<'_>; 0]
    );
    assert_eq!(
        crate::evaluate_audio(&project, &index, range(100, 150))
            .expect("span")
            .clips,
        [] as [audio::ActiveAudioClip<'_>; 0]
    );
}

#[test]
fn audio_span_reports_the_retime_ratio_and_source_mapping() {
    let project = project();
    let index = ActiveIndex::build(&project).expect("valid project");
    let span = crate::evaluate_audio(&project, &index, range(150, 250)).expect("span");
    let clip = &span.clips[0];

    assert!((clip.speed() - 1.0).abs() < f64::EPSILON);
    assert_eq!(clip.source_time_at(MediaTime(150)), Some(MediaTime(2_000)));
    assert_eq!(clip.source_time_at(MediaTime(200)), Some(MediaTime(2_050)));
}

#[test]
fn audio_span_rejects_an_index_built_for_another_project() {
    let project = project();
    let index = ActiveIndex::build(&project).expect("valid project");
    let mut other = project.clone();
    other.id = id(ProjectId::new, "other-project");
    assert!(matches!(
        crate::evaluate_audio(&other, &index, range(150, 250)),
        Err(crate::EvalError::IndexProjectMismatch { .. })
    ));
}

#[test]
fn a_visual_clip_joins_the_audio_span_when_its_source_has_audio() {
    let mut project = project();
    // The fixture's video clip occupies [100, 200) on a visual track.
    let index = ActiveIndex::build(&project).expect("valid project");
    let span = crate::evaluate_audio(&project, &index, range(100, 150)).expect("span");
    assert!(span.clips.is_empty(), "a silent video source stays out");

    project.sources[0].has_audio = true;
    let index = ActiveIndex::build(&project).expect("valid project");
    let span = crate::evaluate_audio(&project, &index, range(100, 150)).expect("span");
    assert_eq!(span.clips.len(), 1);
    assert_eq!(span.clips[0].clip_id.as_str(), "clip-v");
    assert!(span.clips[0].visual_track);

    let frame = crate::evaluate(&project, &index, MediaTime(120)).expect("frame");
    assert_eq!(frame.visual.len(), 1);
    assert!(
        frame.audio.is_empty(),
        "visual clips stay out of frame.audio"
    );
}

#[test]
fn overlapping_audio_clips_are_all_reported_in_mix_order() {
    let mut project = project();
    let mut second = project.timeline.tracks[1].clips[0].clone();
    second.id = id(ClipId::new, "clip-a2");
    second.timeline_range = range(200, 300);
    project.timeline.tracks[1].clips.push(second);
    let index = ActiveIndex::build(&project).expect("overlapping audio is valid");

    // [150,250) and [200,300) overlap on [200,250).
    let span = crate::evaluate_audio(&project, &index, range(200, 250)).expect("span");
    assert_eq!(span.clips.len(), 2);
    assert_eq!(span.clips[0].clip_id.as_str(), "clip-a");
    assert_eq!(span.clips[1].clip_id.as_str(), "clip-a2");
    assert!(
        span.clips
            .iter()
            .all(|clip| clip.active_range == range(200, 250))
    );
}

#[test]
fn the_index_knows_its_deepest_overlap_for_sizing_an_evaluation() {
    let mut project = project();
    let template = project.timeline.tracks[1].clips[0].clone();
    // Nested inside two others, then one butted against the last end.
    for (name, start, end) in [
        ("clip-a2", 200, 300),
        ("clip-a3", 220, 230),
        ("clip-a4", 300, 400),
    ] {
        let mut clip = template.clone();
        clip.id = id(ClipId::new, name);
        clip.timeline_range = range(start, end);
        project.timeline.tracks[1].clips.push(clip);
    }
    let index = ActiveIndex::build(&project).expect("overlapping audio is valid");
    let deepest = (0..500)
        .map(|time| {
            let frame = evaluate(&project, &index, MediaTime(time)).expect("frame");
            frame.visual.len() + frame.audio.len()
        })
        .max();
    assert_eq!(deepest, Some(3));
    assert_eq!(index.max_active, 3);
}

#[test]
fn a_reused_span_scratch_reports_what_evaluate_audio_reports() {
    let base = project();
    let mut edited = project();
    let mut second = edited.timeline.tracks[1].clips[0].clone();
    second.id = id(ClipId::new, "clip-a2");
    second.timeline_range = range(200, 300);
    edited.timeline.tracks[1].clips.push(second);
    edited.sources[0].has_audio = true;
    let base_index = ActiveIndex::build(&base).expect("valid");
    let edited_index = ActiveIndex::build(&edited).expect("valid");

    let mut scratch = AudioSpanScratch::default();
    for (project, index) in [
        (&edited, &edited_index),
        (&base, &base_index),
        (&edited, &edited_index),
    ] {
        for span in [
            range(0, 120),
            range(100, 300),
            range(210, 260),
            range(400, 500),
        ] {
            let expected = evaluate_audio(project, index, span).expect("span");
            let reused = evaluate_audio_in(project, index, span, &mut scratch).expect("span");
            assert_eq!(reused.range(), span);
            assert_eq!(reused.len(), expected.clips.len());
            assert_eq!(reused.iter().collect::<Vec<_>>(), expected.clips);
        }
    }
}

#[test]
fn direct_empty_keyframe_mutation_cannot_panic_the_scalar_sampler() {
    let animation = Animated::Keyframes {
        keyframes: Vec::<Keyframe<f32>>::new(),
    };
    assert!(sample_scalar(&animation, MediaTime::ZERO).abs() <= f32::EPSILON);
}

#[test]
fn every_time_map_picks_its_source_instant() {
    use alpha2_model::{LoopMode, TimeMap};

    // The project runs at 24 fps: a frame is 5000 ticks.
    let shown = |time_map: TimeMap, timeline: TimeRange, source: TimeRange, times: &[i64]| {
        let mut project = project();
        let clip = &mut project.timeline.tracks[0].clips[0];
        clip.time_map = time_map;
        clip.timeline_range = timeline;
        clip.source_range = source;
        let index = ActiveIndex::build(&project).expect("valid project");
        times
            .iter()
            .map(|&time| {
                evaluate(&project, &index, MediaTime(time))
                    .expect("frame")
                    .visual[0]
                    .source_time
                    .ticks()
            })
            .collect::<Vec<_>>()
    };
    // Ten frames over twenty frames of source: twice as fast.
    let (timeline, source) = (range(0, 50_000), range(0, 100_000));
    let times = [0, 2_000, 5_000, 45_000, 47_000];
    assert_eq!(
        shown(TimeMap::Forward, timeline, source, &times),
        [0, 4_000, 10_000, 90_000, 94_000]
    );
    assert_eq!(
        shown(TimeMap::Reverse, timeline, source, &times),
        [90_000, 86_000, 80_000, 0, 0]
    );
    assert_eq!(
        shown(
            TimeMap::Freeze {
                source_time: MediaTime(55_000)
            },
            timeline,
            source,
            &times
        ),
        [55_000; 5]
    );
    // Four frames of source looped over twelve, at their own speed.
    let (timeline, source) = (range(0, 60_000), range(0, 20_000));
    let times = [0, 15_000, 20_000, 25_000, 35_000, 39_000, 40_000, 45_000];
    assert_eq!(
        shown(
            TimeMap::Loop {
                mode: LoopMode::Repeat
            },
            timeline,
            source,
            &times
        ),
        [0, 15_000, 0, 5_000, 15_000, 19_000, 0, 5_000]
    );
    assert_eq!(
        shown(
            TimeMap::Loop {
                mode: LoopMode::PingPong
            },
            timeline,
            source,
            &times
        ),
        [0, 15_000, 15_000, 10_000, 0, 0, 0, 5_000]
    );
}

#[test]
fn each_caption_track_shows_its_cue_over_the_tracks_before_it() {
    use alpha2_model::{CaptionCue, Captions};

    let mut project = project();
    let mut captions = Track::new(id(TrackId::new, "c1"), "captions", TrackKind::Caption);
    captions.captions = Some(Captions {
        style: alpha2_model::TextStyle::default(),
        styles: std::collections::BTreeMap::new(),
        cues: vec![
            CaptionCue {
                timeline_range: range(100, 150),
                text: "first".into(),
                speaker: None,
                style: None,
            },
            CaptionCue {
                timeline_range: range(150, 180),
                text: "second".into(),
                speaker: None,
                style: None,
            },
        ],
    });
    // Over the video track, under the audio track.
    project.timeline.tracks.insert(1, captions);
    let index = ActiveIndex::build(&project).expect("valid project");
    let shown = |time| {
        evaluate(&project, &index, MediaTime(time))
            .expect("frame")
            .captions
            .iter()
            .map(|caption| (caption.text.text().into_owned(), caption.layers_below))
            .collect::<Vec<_>>()
    };
    assert_eq!(shown(99), []);
    assert_eq!(shown(100), [("first".to_owned(), 1)]);
    assert_eq!(shown(149), [("first".to_owned(), 1)]);
    assert_eq!(shown(150), [("second".to_owned(), 1)]);
    assert_eq!(shown(180), []);
}

#[test]
fn the_index_finds_each_clips_source_in_time_linear_in_the_clips() {
    // Not up to `MAX_CLIPS`: at 100,000 the index outgrows the caches and the timing stops being
    // linear while the work stays O(n log n).
    let with_clips = |count: usize| {
        let mut project = project();
        let frame = 4_004;
        let mut tracks = Vec::new();
        let mut sources = Vec::new();
        for index in 0..count {
            let at = index % alpha2_model::MAX_CLIPS_PER_TRACK;
            if at == 0 {
                tracks.push(alpha2_model::Track::new(
                    id(TrackId::new, &format!("t{}", tracks.len())),
                    "V",
                    alpha2_model::TrackKind::Video,
                ));
            }
            let source = id(SourceId::new, &format!("s{index}"));
            sources.push(alpha2_model::MediaSource::new(
                source.clone(),
                format!("{index}.mp4"),
                alpha2_model::MediaKind::Video,
            ));
            let start = i64::try_from(at).expect("index") * frame;
            let clip_range = range(start, start + frame);
            let lane: &mut alpha2_model::Track = tracks.last_mut().expect("a track");
            lane.clips.push(alpha2_model::Clip::new(
                id(ClipId::new, &format!("c{index}")),
                source,
                clip_range,
                clip_range,
            ));
        }
        project.sources = sources;
        project.timeline = alpha2_model::Timeline { tracks };
        project
    };
    alpha2_scaling_support::assert_linear(
        "the active index",
        10_000 / alpha2_scaling_support::GROWTH,
        with_clips,
        |project| {
            ActiveIndex::build(project).expect("a large valid project");
        },
    );
}

#[test]
fn an_overshooting_curve_stays_within_the_finite_values_a_visual_holds() {
    let mut project = project();
    let visual = &mut project.timeline.tracks[0].clips[0].visual;
    visual.scale = keyframes([
        (
            100,
            Vec2::new(0.0, 0.0),
            Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.5, 1.0e38, 0.5, 1.0)),
        ),
        (200, Vec2::new(10.0, 10.0), Interpolation::Hold),
    ]);
    visual.rotation_degrees = keyframes([
        (
            100,
            -3.0e38,
            Interpolation::CubicBezier(alpha2_model::CubicBezier::new(0.34, 1.56, 0.64, 1.0)),
        ),
        (200, 3.0e38, Interpolation::Hold),
    ]);
    for time in [130, 150, 170] {
        let visual = visual_at(&project, time);
        assert!(
            visual.scale.x.is_finite() && visual.scale.y.is_finite(),
            "{time}: {visual:?}"
        );
        assert!(visual.rotation_degrees.is_finite(), "{time}: {visual:?}");
    }
    assert_eq!(
        visual_at(&project, 150).scale.x.to_bits(),
        f32::MAX.to_bits()
    );
}

#[test]
fn a_clips_composition_time_is_held_by_a_tick_to_its_last_instant() {
    // A composition time past what a tick holds would fail every frame of the clip with
    // UnmappableClip, so validation refuses it instead.
    use alpha2_model::{ClipComposition, Composition, CompositionId, NodeId, NodeOp, TimeScale};
    let mut project = project();
    let mut composition = Composition::new(id(CompositionId::new, "look"));
    composition.nodes = vec![alpha2_model::CompositionNode::new(
        id(NodeId::new, "clip"),
        NodeOp::Clip {},
    )];
    composition.output = Some(id(NodeId::new, "clip"));
    project.compositions.push(composition);
    let clip = &mut project.timeline.tracks[0].clips[0];
    // The clip lasts 100 ticks: its last instant is 99 ticks in.
    let mut reference = ClipComposition::new(id(CompositionId::new, "look"));
    reference.offset = MediaTime(alpha2_time::MAX_SERIALIZED_TICKS - 99);
    clip.composition = Some(reference.clone());
    let index = ActiveIndex::build(&project).expect("the last instant fits");
    let frame = evaluate(&project, &index, MediaTime(199)).expect("the last instant");
    assert_eq!(
        frame.visual[0].composition.expect("composition").time(),
        MediaTime(alpha2_time::MAX_SERIALIZED_TICKS)
    );
    reference.offset = MediaTime(alpha2_time::MAX_SERIALIZED_TICKS - 98);
    project.timeline.tracks[0].clips[0].composition = Some(reference.clone());
    assert!(matches!(
        ActiveIndex::build(&project),
        Err(EvalError::InvalidProject(
            alpha2_model::ModelError::InvalidClipComposition { .. }
        ))
    ));
    reference.offset = MediaTime::ZERO;
    reference.scale = TimeScale {
        num: u32::MAX,
        den: 1,
    };
    project.timeline.tracks[0].clips[0].timeline_range = range(0, 1 << 40);
    project.timeline.tracks[0].clips[0].source_range = range(0, 1 << 40);
    project.timeline.tracks[0].clips[0].composition = Some(reference);
    assert!(matches!(
        ActiveIndex::build(&project),
        Err(EvalError::InvalidProject(
            alpha2_model::ModelError::InvalidClipComposition { .. }
        ))
    ));
}

/// The instants a clip shows at each project frame starting inside it.
fn frame_instants(
    rate: FrameRate,
    timeline: TimeRange,
    source: TimeRange,
    time_map: alpha2_model::TimeMap,
) -> Vec<(i64, i64)> {
    let last = rate
        .frame_at_or_before(MediaTime(timeline.end().ticks() - 1))
        .expect("frame");
    let first = rate
        .frame_at_or_before(MediaTime(timeline.start().ticks() - 1))
        .expect("frame")
        + 1;
    (first..=last)
        .map(|frame| {
            let tick = rate.frame_start(frame).expect("frame");
            let shown =
                picture_source_time(timeline, source, time_map, rate, tick).expect("source time");
            (tick.ticks(), shown.ticks())
        })
        .collect()
}

#[test]
fn a_reversed_clip_shows_the_forward_clips_frame_instants_in_the_opposite_order() {
    // Covers frames of a fractional number of ticks and clips ending inside a frame: the mirror
    // must ask only for instants the forward clip asks for.
    use alpha2_model::{LoopMode, TimeMap};
    for (num, den) in [
        (24, 1),
        (30, 1),
        (30_000, 1_001),
        (90, 1),
        (72, 1),
        (144, 1),
        (48_000, 1_001),
    ] {
        let rate = FrameRate::new(num, den).expect("rate");
        for (start_shift, end_shift) in [(0, 0), (1, 0), (0, 700), (333, 1)] {
            for speed in [1, 2, 3] {
                let start = rate.frame_start(10).expect("frame").ticks() + start_shift;
                let end = rate.frame_start(40).expect("frame").ticks() + end_shift;
                let timeline = range(start, end);
                let source = range(7_000, 7_000 + (end - start) * speed);
                let label = format!("{num}/{den} +{start_shift} +{end_shift} x{speed}");
                let forward = frame_instants(rate, timeline, source, TimeMap::Forward);
                let reverse = frame_instants(rate, timeline, source, TimeMap::Reverse);
                let mirrored = forward
                    .iter()
                    .rev()
                    .map(|(_, shown)| *shown)
                    .collect::<Vec<_>>();
                let shown = reverse.iter().map(|(_, shown)| *shown).collect::<Vec<_>>();
                assert_eq!(shown, mirrored, "{label}");
            }
            let start = rate.frame_start(10).expect("frame").ticks() + start_shift;
            let length = rate.frame_start(20).expect("frame").ticks()
                - rate.frame_start(10).expect("frame").ticks()
                + end_shift;
            let timeline = range(start, start + 2 * length);
            let source = range(7_000, 7_000 + length);
            let pass = range(start + length, start + 2 * length);
            let backward = frame_instants(
                rate,
                pass,
                source,
                TimeMap::Loop {
                    mode: LoopMode::PingPong,
                },
            )
            .into_iter()
            .map(|(tick, _)| {
                picture_source_time(
                    timeline,
                    source,
                    TimeMap::Loop {
                        mode: LoopMode::PingPong,
                    },
                    rate,
                    MediaTime(tick),
                )
                .expect("source time")
                .ticks()
            })
            .collect::<Vec<_>>();
            let played = frame_instants(rate, pass, source, TimeMap::Forward)
                .into_iter()
                .rev()
                .map(|(tick, _)| 7_000 + tick - pass.start().ticks())
                .collect::<Vec<_>>();
            assert_eq!(
                backward, played,
                "ping-pong {num}/{den} +{start_shift} +{end_shift}"
            );
        }
    }
}

#[test]
fn the_planner_finds_each_composition_and_source_in_time_linear_in_the_plans() {
    use alpha2_model::{
        ClipComposition, Composition, CompositionId, CompositionNode, NodeId, NodeOp,
    };
    let with_clips = |clips: usize| {
        let mut project = project();
        let frame = 4_004;
        let mut lanes = Vec::new();
        let mut sources = Vec::new();
        let mut compositions = Vec::new();
        let node = |value: &str, op| CompositionNode::new(id(NodeId::new, value), op);
        for at in 0..clips {
            let source = id(SourceId::new, &format!("s{at}"));
            sources.push(MediaSource::new(
                source.clone(),
                format!("{at}.mp4"),
                MediaKind::Video,
            ));
            // Each clip's own look groups the shared grade, listed after every look, over a media
            // node of its own source.
            let mut look = Composition::new(id(CompositionId::new, &format!("look-{at}")));
            look.nodes = vec![
                node(
                    "m",
                    NodeOp::Media {
                        source_id: source.clone(),
                        start: MediaTime::ZERO,
                    },
                ),
                node(
                    "g",
                    NodeOp::Group {
                        composition: id(CompositionId::new, "grade"),
                        inputs: vec![Some(id(NodeId::new, "m"))],
                        controls: BTreeMap::new(),
                    },
                ),
            ];
            look.output = Some(id(NodeId::new, "g"));
            compositions.push(look);
            if at % alpha2_model::MAX_CLIPS_PER_TRACK == 0 {
                lanes.push(Track::new(
                    id(TrackId::new, &format!("v{at}")),
                    "V",
                    TrackKind::Video,
                ));
            }
            let start =
                i64::try_from(at % alpha2_model::MAX_CLIPS_PER_TRACK).expect("index") * frame;
            let mut clip = Clip::new(
                id(ClipId::new, &format!("c{at}")),
                source,
                range(start, start + frame),
                range(0, frame),
            );
            clip.composition = Some(ClipComposition::new(id(
                CompositionId::new,
                &format!("look-{at}"),
            )));
            lanes.last_mut().expect("a lane").clips.push(clip);
        }
        let mut grade = Composition::new(id(CompositionId::new, "grade"));
        grade.nodes = vec![node("in", NodeOp::GroupInput { index: 0 })];
        grade.output = Some(id(NodeId::new, "in"));
        grade.inputs = vec!["image".into()];
        compositions.push(grade);
        project.sources = sources;
        project.compositions = compositions;
        project.timeline = Timeline { tracks: lanes };
        project
    };
    // Up to as many clips as there can be compositions besides the shared grade.
    alpha2_scaling_support::assert_linear(
        "the active index's plans",
        (alpha2_model::MAX_COMPOSITIONS - 1) / alpha2_scaling_support::GROWTH,
        with_clips,
        |project| {
            ActiveIndex::build(project).expect("a large valid project");
        },
    );
}

#[test]
fn a_matte_clip_is_needed_only_while_a_clip_it_mattes_is_active() {
    let mut project = project();
    let matte_source = id(SourceId::new, "matte");
    project.sources.push(MediaSource::new(
        matte_source.clone(),
        "asset://matte",
        MediaKind::Video,
    ));
    // The matte spans [0, 400); the clip it mattes, [100, 200).
    let mut matte_track = Track::new(id(TrackId::new, "v2"), "matte", TrackKind::Video);
    matte_track.clips.push(Clip::new(
        id(ClipId::new, "matte-clip"),
        matte_source.clone(),
        range(0, 400),
        range(0, 400),
    ));
    project.timeline.tracks.push(matte_track);
    project.timeline.tracks[0].clips[0].matte = Some(alpha2_model::Matte {
        source: id(ClipId::new, "matte-clip"),
        mode: alpha2_model::MatteMode::Alpha,
    });
    let index = ActiveIndex::build(&project).expect("valid project");
    let reads_matte = |time| {
        frame_needs(&project, &index, MediaTime(time))
            .expect("needs")
            .iter()
            .any(|need| need.source_id == matte_source)
    };
    assert!(reads_matte(150), "the clip it mattes is active");
    assert!(!reads_matte(300), "nothing it mattes is active");
}

#[test]
fn a_speaker_style_applies_to_each_of_their_cues() {
    use alpha2_model::{CaptionCue, Captions, Paint, Rgba, TextStylePatch};

    let red = Paint::Solid {
        color: Rgba::new(1.0, 0.0, 0.0, 1.0),
    };
    let cue = |start, end, speaker: Option<&str>| CaptionCue {
        speaker: speaker.map(Into::into),
        ..CaptionCue::new(range(start, end), "words")
    };
    let mut project = project();
    let mut track = Track::new(id(TrackId::new, "c1"), "captions", TrackKind::Caption);
    track.captions = Some(Captions {
        style: alpha2_model::TextStyle::default(),
        styles: [(
            "Ann".to_owned(),
            TextStylePatch {
                fill: Some(red.clone()),
                ..TextStylePatch::default()
            },
        )]
        .into(),
        cues: vec![
            cue(0, 10, Some("Ann")),
            cue(10, 20, Some("Bob")),
            cue(20, 30, Some("Ann")),
        ],
    });
    project.timeline.tracks.insert(1, track);
    let index = ActiveIndex::build(&project).expect("valid project");
    let fill = |time| {
        evaluate(&project, &index, MediaTime(time))
            .expect("frame")
            .captions[0]
            .style
            .fill
            .clone()
    };
    assert_eq!(fill(5), red);
    assert_eq!(fill(15), alpha2_model::TextStyle::default().fill);
    assert_eq!(fill(25), red);
}

#[test]
fn animator_time_restarts_at_each_cue() {
    use alpha2_model::{CaptionCue, Captions};

    let mut project = project();
    let mut track = Track::new(id(TrackId::new, "c1"), "captions", TrackKind::Caption);
    track.captions = Some(Captions {
        style: alpha2_model::TextStyle::default(),
        styles: std::collections::BTreeMap::new(),
        cues: vec![
            CaptionCue::new(range(100, 150), "first"),
            CaptionCue::new(range(150, 180), "second"),
        ],
    });
    project.timeline.tracks.insert(1, track);
    let index = ActiveIndex::build(&project).expect("valid project");
    let local = |time| {
        evaluate(&project, &index, MediaTime(time))
            .expect("frame")
            .captions[0]
            .local_time
    };
    assert_eq!(local(120), MediaTime(20));
    assert_eq!(local(150), MediaTime(0));
    assert_eq!(local(179), MediaTime(29));
}
