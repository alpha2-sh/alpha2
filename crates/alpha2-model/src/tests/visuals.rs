//! Masks, mattes, generated sources, and frozen clips.

use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn masks_and_mattes_are_validated() {
    use crate::{
        Animated, AnimatedPoint, FillRule, MaskMode, Matte, MatteMode, RectangleGeometry,
        ShapeGeometry, ShapeMask, ShapeProblem, Vec2,
    };

    let mask = |feather: f32| ShapeMask {
        shape: ShapeGeometry::Rectangle(RectangleGeometry {
            center: AnimatedPoint::Constant {
                value: Vec2::new(0.5, 0.5),
            },
            size: Animated::Constant {
                value: Vec2::new(0.5, 0.5),
            },
            corner_radius: Animated::Constant { value: 0.1 },
            rotation_degrees: Animated::Constant { value: 30.0 },
        }),
        feather: Animated::Constant { value: feather },
        invert: false,
        mode: MaskMode::Add,
        fill_rule: FillRule::Nonzero,
    };
    let mut masked = project();
    masked.timeline.tracks[0].clips[0].masks = vec![mask(0.1); crate::MAX_CLIP_MASKS];
    masked.validate().expect("eight masks");
    masked.timeline.tracks[0].clips[0].masks.push(mask(0.1));
    assert!(matches!(
        masked.validate(),
        Err(crate::ModelError::InvalidClipMask { .. })
    ));
    masked.timeline.tracks[0].clips[0].masks = vec![mask(0.1), mask(-0.1)];
    assert_eq!(
        masked.validate().map_err(|error| error.to_string()),
        Err("clip clip-a has an invalid shape: masks[1].feather has a value out of range".into())
    );
    let mut negative = mask(0.1);
    let ShapeGeometry::Rectangle(rectangle) = &mut negative.shape else {
        unreachable!()
    };
    rectangle.size = Animated::Constant {
        value: Vec2::new(-0.5, 0.5),
    };
    masked.timeline.tracks[0].clips[0].masks = vec![negative];
    assert!(matches!(
        masked.validate(),
        Err(crate::ModelError::InvalidShape { at, problem: ShapeProblem::OutOfRange, .. })
            if at == "masks[0].shape.size"
    ));

    // A second visual track whose clip is the first clip's matte.
    let mut project = project();
    let mut second = project.timeline.tracks[0].clone();
    second.id = id(TrackId::new, "track-matte");
    second.clips[0].id = id(ClipId::new, "clip-matte");
    project.timeline.tracks.push(second);
    let matte = |source: &str| {
        Some(Matte {
            source: id(ClipId::new, source),
            mode: MatteMode::Luma,
        })
    };
    project.timeline.tracks[0].clips[0].matte = matte("clip-matte");
    project.validate().expect("a matte on another track");
    for invalid in ["clip-a", "missing"] {
        project.timeline.tracks[0].clips[0].matte = matte(invalid);
        assert!(
            matches!(
                project.validate(),
                Err(crate::ModelError::InvalidMatte { .. })
            ),
            "{invalid}"
        );
    }
    // No chains: the matte clip may not have a matte of its own.
    project.timeline.tracks[0].clips[0].matte = matte("clip-matte");
    project.timeline.tracks[1].clips[0].matte = matte("clip-a");
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidMatte { .. })
    ));

    let wire: crate::Clip = serde_json::from_str(
        r#"{"id":"c","source_id":"s","timeline_range":{"start":0,"end":1},
            "source_range":{"start":0,"end":1},
            "masks":[{"shape":{"type":"ellipse",
                      "center":{"mode":"constant","value":{"x":0.5,"y":0.5}},
                      "size":{"mode":"constant","value":{"x":1,"y":1}}}}],
            "matte":{"source":"m","mode":"alpha_inverted"}}"#,
    )
    .expect("mask and matte JSON");
    assert!(matches!(wire.masks[0].shape, ShapeGeometry::Ellipse(_)));
    assert_eq!(wire.masks[0].mode, MaskMode::Add);
    assert_eq!(wire.masks[0].fill_rule, FillRule::Nonzero);
    // An ellipse has no corner radius, and the geometry no longer sits beside the shape.
    assert!(
        serde_json::from_str::<ShapeGeometry>(
            r#"{"type":"ellipse","corner_radius":{"mode":"constant","value":1},
                "center":{"mode":"constant","value":{"x":0.5,"y":0.5}},
                "size":{"mode":"constant","value":{"x":1,"y":1}}}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<ShapeMask>(
            r#"{"shape":{"type":"ellipse"},
                "center":{"mode":"constant","value":{"x":0.5,"y":0.5}},
                "size":{"mode":"constant","value":{"x":1,"y":1}}}"#
        )
        .is_err()
    );
}

fn invalid_generator(project: &crate::Project) -> bool {
    matches!(
        project.validate(),
        Err(crate::ModelError::InvalidGenerator { .. })
    )
}

/// A white-stroked rounded rectangle with no fill.
fn ring() -> crate::Generator {
    serde_json::from_str(
        r#"{"type":"shapes","items":[
            {"type":"rectangle",
             "center":{"mode":"constant","value":{"x":0.5,"y":0.5}},
             "size":{"mode":"constant","value":{"x":0.5,"y":0.25}},
             "corner_radius":{"mode":"constant","value":0.1},
             "rotation_degrees":{"mode":"constant","value":10}},
            {"type":"stroke","width":{"mode":"constant","value":0.02},
             "paint":{"mode":"constant","value":{"type":"solid",
                "color":{"red":1,"green":1,"blue":1,"alpha":1}}}}]}"#,
    )
    .expect("shapes JSON")
}

#[test]
fn generated_sources_are_validated() {
    use crate::{Generator, GradientStop, Paint, Rgba, Spread, Vec2};

    let stops = |count: usize, first: f32| {
        (0..count)
            .map(|index| GradientStop {
                #[allow(clippy::cast_precision_loss)]
                offset: if index == 0 {
                    first
                } else {
                    index as f32 / count as f32
                },
                color: Rgba::new(1.0, 0.5, 0.0, 1.0),
            })
            .collect::<Vec<_>>()
    };
    let linear = |stops| Paint::LinearGradient {
        start: Vec2::new(0.0, 0.0),
        end: Vec2::new(1.0, 1.0),
        stops,
        spread: Spread::Reflect,
    };
    let generated = |generator| {
        with_source(MediaSource::generated(
            id(SourceId::new, "generated"),
            generator,
        ))
    };

    generated(Generator::Fill {
        paint: linear(stops(crate::MAX_GRADIENT_STOPS, 0.0)),
    })
    .validate()
    .expect("a gradient fill on a video track");
    generated(ring()).validate().expect("a stroked shape");

    // Too many stops, too few, out of order, a color out of range, a degenerate line, a zero
    // radius.
    for paint in [
        linear(stops(crate::MAX_GRADIENT_STOPS + 1, 0.0)),
        linear(stops(1, 0.0)),
        linear(stops(3, 0.9)),
        Paint::Solid {
            color: Rgba::new(1.5, 0.0, 0.0, 1.0),
        },
        Paint::LinearGradient {
            start: Vec2::new(0.5, 0.5),
            end: Vec2::new(0.5, 0.5),
            stops: stops(2, 0.0),
            spread: Spread::Pad,
        },
        Paint::RadialGradient {
            center: Vec2::new(0.5, 0.5),
            radius: 0.0,
            stops: stops(2, 0.0),
            spread: Spread::Pad,
        },
    ] {
        assert!(invalid_generator(&generated(Generator::Fill { paint })));
    }
    // A shapes generator's problem is reported where it lies.
    let crate::Generator::Shapes { mut items } = ring() else {
        unreachable!()
    };
    let crate::ShapeItem::Stroke(stroke) = &mut items[1] else {
        unreachable!()
    };
    stroke.miter_limit = 0.5;
    assert_eq!(
        generated(Generator::Shapes { items })
            .validate()
            .map_err(|error| error.to_string()),
        Err(
            "source generated has an invalid shape: items[1].miter_limit has a value out of range"
                .into()
        )
    );
}

#[test]
fn a_generated_source_carries_nothing_a_host_supplies() {
    let generated = || MediaSource::generated(id(SourceId::new, "generated"), ring());
    let mut located = generated();
    located.locator = "asset://x".into();
    assert!(invalid_generator(&with_source(located)));
    let mut audible = generated();
    audible.has_audio = true;
    assert!(invalid_generator(&with_source(audible)));
    // And a host source carries no generator, nor a generated one none.
    let mut video = MediaSource::new(
        id(SourceId::new, "generated"),
        "asset://v",
        MediaKind::Video,
    );
    video.generator = Some(ring());
    assert!(invalid_generator(&with_source(video)));
    let bare = MediaSource::new(id(SourceId::new, "generated"), "", MediaKind::Generated);
    assert!(invalid_generator(&with_source(bare)));
    assert!(!generated().is_host_supplied());
}

#[test]
fn a_clip_freezes_only_inside_its_source_range() {
    use crate::{LoopMode, TimeMap};

    let mut project = project();
    let range = project.timeline.tracks[0].clips[0].source_range;
    for time_map in [
        TimeMap::Reverse,
        TimeMap::Loop {
            mode: LoopMode::PingPong,
        },
        TimeMap::Freeze {
            source_time: range.start(),
        },
        TimeMap::Freeze {
            source_time: MediaTime::from_ticks(range.end().ticks() - 1),
        },
    ] {
        project.timeline.tracks[0].clips[0].time_map = time_map;
        project.validate().expect("a valid time map");
    }
    project.timeline.tracks[0].clips[0].time_map = TimeMap::Freeze {
        source_time: range.end(),
    };
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidTimeMap { .. })
    ));
}
