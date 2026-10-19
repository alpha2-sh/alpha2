//! Fades, transition easing, and interpolation curves.

use super::*;

#[test]
fn fade_shapes_map_progress_to_amplitude() {
    use crate::FadeShape;
    assert_eq!(FadeShape::default(), FadeShape::Linear);
    for shape in [FadeShape::Linear, FadeShape::EqualPower] {
        assert!(shape.amplitude(0.0).abs() < 1.0e-6);
        assert!((shape.amplitude(1.0) - 1.0).abs() < 1.0e-6);
        // Values outside the unit interval clamp rather than extrapolate.
        assert!(shape.amplitude(-1.0).abs() < 1.0e-6);
        assert!((shape.amplitude(2.0) - 1.0).abs() < 1.0e-6);
    }
    // Halfway through, equal power sits above linear by design.
    assert!((FadeShape::Linear.amplitude(0.5) - 0.5).abs() < 1.0e-6);
    let equal = FadeShape::EqualPower.amplitude(0.5);
    assert!(
        (equal - core::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-6,
        "{equal}"
    );
}

#[test]
fn transition_easing_is_a_required_curve() {
    use crate::{ClipTransition, CubicBezier, Easing};

    let transition = |easing: &str| {
        format!(
            r#"{{
                "from_clip":"from",
                "to_clip":"to",
                "timeline_range":{{"start":0,"end":1}},
                {easing}
                "effect":{{"id":"alpha2.transition.fade","version":1}}
            }}"#
        )
    };
    assert!(serde_json::from_str::<ClipTransition>(&transition("")).is_err());
    let decoded: ClipTransition = serde_json::from_str(&transition(
        r#""easing":{"type":"cubic_bezier","x1":0.42,"y1":0,"x2":0.58,"y2":1},"#,
    ))
    .expect("a cubic-bezier transition");
    assert_eq!(
        decoded.easing,
        Easing::CubicBezier(CubicBezier::new(0.42, 0.0, 0.58, 1.0))
    );
    // A preset name is not an easing: hosts keep named presets and write their curves.
    assert!(
        serde_json::from_str::<ClipTransition>(&transition(r#""easing":"ease_in_out","#)).is_err()
    );
}

#[test]
fn interpolation_is_a_tagged_curve_and_validates_its_control_points() {
    use crate::{CubicBezier, Interpolation};

    let keyframe: crate::Keyframe<f32> = serde_json::from_str(
        r#"{"time":0,"value":1,"interpolation":{"type":"cubic_bezier","x1":0.25,"y1":0.1,"x2":0.25,"y2":1}}"#,
    )
    .expect("a cubic-bezier keyframe");
    assert_eq!(
        keyframe.interpolation,
        Interpolation::CubicBezier(CubicBezier::new(0.25, 0.1, 0.25, 1.0))
    );
    assert_eq!(Interpolation::Hold.amount(0.5), None);
    assert_eq!(Interpolation::Linear.amount(0.25), Some(0.25));
    assert_eq!(Interpolation::Linear.amount(3.0), Some(1.0));

    let mut project = audio_project();
    let gain = |interpolation| crate::Animated::Keyframes {
        keyframes: vec![
            crate::Keyframe {
                time: MediaTime(0),
                value: 0.0,
                interpolation,
            },
            crate::Keyframe {
                time: MediaTime(120_000),
                value: 1.0,
                interpolation: Interpolation::Hold,
            },
        ],
    };
    project.timeline.tracks[1].clips[0].audio.gain = gain(Interpolation::CubicBezier(
        CubicBezier::new(0.4, -0.5, 0.2, 1.5),
    ));
    project.validate().expect("y may overshoot");
    project.timeline.tracks[1].clips[0].audio.gain = gain(Interpolation::CubicBezier(
        CubicBezier::new(1.5, 0.0, 0.2, 1.0),
    ));
    assert!(project.validate().is_err(), "x must stay in 0..=1");
}
