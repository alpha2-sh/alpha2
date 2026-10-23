//! Shapes generators: the content tree's wire form, limits, and the rules
//! between keyframes; spatial positions.

use super::*;
use crate::{
    Animated, AnimatedPoint, Generator, ModelError, ShapeItem, ShapeProblem, validate_shape_items,
};

fn items(json: &str) -> Vec<ShapeItem> {
    serde_json::from_str(json).expect("items JSON")
}

fn problem(json: &str) -> Option<(String, ShapeProblem)> {
    validate_shape_items(&items(json))
        .err()
        .map(|issue| (issue.at, issue.problem))
}

const RED: &str = r#"{"type":"solid","color":{"red":1,"green":0,"blue":0,"alpha":1}}"#;

fn constant(value: &str) -> String {
    format!(r#"{{"mode":"constant","value":{value}}}"#)
}

fn rectangle() -> String {
    format!(
        r#"{{"type":"rectangle","center":{},"size":{}}}"#,
        constant(r#"{"x":0.5,"y":0.5}"#),
        constant(r#"{"x":0.2,"y":0.2}"#)
    )
}

fn fill() -> String {
    format!(r#"{{"type":"fill","paint":{}}}"#, constant(RED))
}

fn path(vertices: usize, closed: bool) -> String {
    let vertex = r#"{"point":{"x":0.1,"y":0.2},"in_tangent":{"x":0.01,"y":0}}"#;
    let list = vec![vertex; vertices].join(",");
    format!(r#"{{"subpaths":[{{"closed":{closed},"vertices":[{list}]}}]}}"#)
}

#[test]
fn the_plan_example_reads_and_writes_back_unchanged() {
    let json = format!(
        r#"[{{"type":"group","transform":{{"position":{}}},"items":[
            {{"type":"path","path":{{"mode":"keyframes","keyframes":[
                {{"time":0,"interpolation":{{"type":"linear"}},"value":{}}},
                {{"time":60000,"interpolation":{{"type":"hold"}},"value":{}}}]}}}},
            {{"type":"stroke","width":{},"join":"round","paint":{}}},
            {{"type":"fill","paint":{}}}]}}]"#,
        constant(r#"{"x":0.5,"y":0.5}"#),
        path(5, true),
        path(5, true),
        constant("0.01"),
        constant(RED),
        constant(RED),
    );
    let tree = items(&json);
    validate_shape_items(&tree).expect("valid");
    let written = serde_json::to_string(&tree).expect("serializes");
    assert_eq!(items(&written), tree);
    // Defaults fill in what was left out.
    let ShapeItem::Group {
        items: inner,
        transform,
    } = &tree[0]
    else {
        panic!("a group");
    };
    assert_eq!(
        transform.scale,
        Animated::Constant {
            value: crate::Vec2::new(1.0, 1.0)
        }
    );
    let ShapeItem::Stroke(stroke) = &inner[1] else {
        panic!("a stroke");
    };
    assert!((stroke.miter_limit - 4.0).abs() < f32::EPSILON);
    assert_eq!(stroke.cap, crate::LineCap::Butt);
    assert!(matches!(
        inner[2],
        ShapeItem::Fill {
            rule: crate::FillRule::Nonzero,
            ..
        }
    ));
}

#[test]
fn unknown_fields_and_kinds_are_refused() {
    for json in [
        r#"[{"type":"rectangle","center":{"mode":"constant","value":{"x":0,"y":0}},"size":{"mode":"constant","value":{"x":1,"y":1}},"extra":1}]"#,
        r#"[{"type":"repeater"}]"#,
        r#"[{"type":"fill","paint":{"mode":"constant","value":{"type":"solid","color":{"red":1,"green":0,"blue":0,"alpha":1}}},"rule":"winding"}]"#,
    ] {
        assert!(
            serde_json::from_str::<Vec<ShapeItem>>(json).is_err(),
            "{json}"
        );
    }
}

#[test]
fn limits_are_enforced_with_where_they_are_broken() {
    // Items count over every nesting level.
    let many = vec![fill(); crate::MAX_SHAPE_ITEMS + 1].join(",");
    assert_eq!(
        problem(&format!("[{many}]")),
        Some((
            format!("items[{}]", crate::MAX_SHAPE_ITEMS),
            ShapeProblem::TooManyItems
        ))
    );
    let mut nested = fill();
    for _ in 0..crate::MAX_SHAPE_DEPTH {
        nested = format!(r#"{{"type":"group","items":[{nested}]}}"#);
    }
    assert_eq!(
        problem(&format!("[{nested}]")).map(|found| found.1),
        Some(ShapeProblem::TooDeep)
    );
    let too_many_vertices = format!(
        r#"[{{"type":"path","path":{}}}]"#,
        constant(&path(crate::MAX_PATH_VERTICES + 1, false))
    );
    assert_eq!(
        problem(&too_many_vertices),
        Some(("items[0].path".into(), ShapeProblem::PathTooLarge))
    );
    let points = |value: &str| {
        format!(
            r#"[{{"type":"polystar","kind":"polygon","center":{},"points":{},"outer_radius":{}}}]"#,
            constant(r#"{"x":0.5,"y":0.5}"#),
            constant(value),
            constant("0.2")
        )
    };
    assert_eq!(problem(&points("3")), None);
    assert_eq!(problem(&points("100")), None);
    assert_eq!(
        problem(&points("2.5")),
        Some(("items[0].points".into(), ShapeProblem::OutOfRange))
    );
    let dash = |pattern: &str| {
        format!(
            r#"[{},{{"type":"stroke","width":{},"paint":{},"dash":{{"pattern":[{pattern}]}}}}]"#,
            rectangle(),
            constant("0.01"),
            constant(RED)
        )
    };
    assert_eq!(problem(&dash(&constant("0.02"))), None);
    assert_eq!(
        problem(&dash(&constant("0"))),
        Some(("items[1].dash.pattern".into(), ShapeProblem::InvalidDash))
    );
    assert_eq!(
        problem(&dash(
            &vec![constant("0.01"); crate::MAX_DASH_ENTRIES + 1].join(",")
        )),
        Some(("items[1].dash.pattern".into(), ShapeProblem::InvalidDash))
    );
}

#[test]
fn values_out_of_range_are_named() {
    let trim = format!(
        r#"[{},{{"type":"trim","start":{},"end":{}}}]"#,
        rectangle(),
        constant("0.2"),
        constant("1.5")
    );
    assert_eq!(
        problem(&trim),
        Some(("items[1].end".into(), ShapeProblem::OutOfRange))
    );
    let opacity = format!(
        r#"[{{"type":"group","transform":{{"opacity":{}}},"items":[]}}]"#,
        constant("2")
    );
    assert_eq!(
        problem(&opacity),
        Some((
            "items[0].transform.opacity".into(),
            ShapeProblem::OutOfRange
        ))
    );
}

#[test]
fn path_keyframes_must_agree_in_structure() {
    let keyed = |first: &str, second: &str| {
        format!(
            r#"[{{"type":"group","items":[{{"type":"path","path":{{"mode":"keyframes","keyframes":[
                {{"time":0,"interpolation":{{"type":"linear"}},"value":{first}}},
                {{"time":1000,"interpolation":{{"type":"linear"}},"value":{second}}}]}}}}]}}]"#
        )
    };
    assert_eq!(problem(&keyed(&path(4, true), &path(4, true))), None);
    for (first, second) in [
        (path(4, true), path(5, true)),
        (path(4, true), path(4, false)),
        (path(4, true), r#"{"subpaths":[]}"#.to_owned()),
    ] {
        assert_eq!(
            problem(&keyed(&first, &second)),
            Some((
                "items[0].items[0].path".into(),
                ShapeProblem::PathKeyframesDiffer
            ))
        );
    }
}

#[test]
fn paint_keyframes_must_agree_in_kind_and_stops() {
    #[allow(clippy::cast_precision_loss)]
    let gradient = |stops: usize, spread: &str| {
        let list = (0..stops)
            .map(|index| {
                format!(
                    r#"{{"offset":{},"color":{{"red":1,"green":0,"blue":0,"alpha":1}}}}"#,
                    index as f32 / stops as f32
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"type":"linear_gradient","start":{{"x":0,"y":0}},"end":{{"x":1,"y":1}},"stops":[{list}],"spread":"{spread}"}}"#
        )
    };
    let keyed = |first: &str, second: &str| {
        format!(
            r#"[{},{{"type":"fill","paint":{{"mode":"keyframes","keyframes":[
                {{"time":0,"interpolation":{{"type":"linear"}},"value":{first}}},
                {{"time":1000,"interpolation":{{"type":"linear"}},"value":{second}}}]}}}}]"#,
            rectangle()
        )
    };
    assert_eq!(problem(&keyed(RED, RED)), None);
    assert_eq!(
        problem(&keyed(&gradient(2, "pad"), &gradient(2, "pad"))),
        None
    );
    for (first, second) in [
        (RED.to_owned(), gradient(2, "pad")),
        (gradient(2, "pad"), gradient(3, "pad")),
        (gradient(2, "pad"), gradient(2, "repeat")),
    ] {
        assert_eq!(
            problem(&keyed(&first, &second)),
            Some(("items[1].paint".into(), ShapeProblem::PaintKeyframesDiffer))
        );
    }
}

#[test]
fn a_shapes_problem_names_its_owner_in_the_project_error() {
    let generated = MediaSource::generated(
        id(SourceId::new, "shapes"),
        Generator::Shapes {
            items: items(&format!(
                r#"[{},{{"type":"trim","start":{},"end":{}}}]"#,
                rectangle(),
                constant("-1"),
                constant("1")
            )),
        },
    );
    let error = with_source(generated).validate().expect_err("invalid trim");
    assert!(matches!(error, ModelError::InvalidShape { .. }));
    assert_eq!(
        error.to_string(),
        "source shapes has an invalid shape: items[1].start has a value out of range"
    );
}

#[test]
fn a_position_without_tangents_is_the_animated_vector_it_always_was() {
    let json = r#"{"mode":"keyframes","keyframes":[
        {"time":0,"value":{"x":0,"y":0},"interpolation":{"type":"linear"}},
        {"time":1000,"value":{"x":1,"y":1},"interpolation":{"type":"hold"}}]}"#;
    let point: AnimatedPoint = serde_json::from_str(json).expect("an old position");
    let vector: Animated<crate::Vec2> = serde_json::from_str(json).expect("an animated vector");
    assert_eq!(point, AnimatedPoint::from(vector.clone()));
    // Written back, zero tangents are left out, so the document is what it was.
    assert_eq!(
        serde_json::to_value(&point).expect("JSON"),
        serde_json::to_value(&vector).expect("JSON")
    );
    let curved = r#"{"mode":"keyframes","keyframes":[
        {"time":0,"value":{"x":0,"y":0},"interpolation":{"type":"linear"},"out_tangent":{"x":0.1,"y":0}},
        {"time":1000,"value":{"x":1,"y":1},"interpolation":{"type":"hold"},"in_tangent":{"x":0,"y":-0.1}}]}"#;
    let point: AnimatedPoint = serde_json::from_str(curved).expect("a spatial position");
    let written = serde_json::to_string(&point).expect("JSON");
    assert_eq!(
        serde_json::from_str::<AnimatedPoint>(&written).expect("again"),
        point
    );
    assert!(written.contains("out_tangent") && written.contains("in_tangent"));
}

#[test]
fn a_clip_position_with_a_non_finite_tangent_is_refused() {
    let mut project = project();
    project.timeline.tracks[0].clips[0].visual.position = AnimatedPoint::Keyframes {
        keyframes: vec![crate::PointKeyframe {
            time: MediaTime::ZERO,
            value: crate::Vec2::new(0.5, 0.5),
            interpolation: crate::Interpolation::Linear,
            out_tangent: crate::Vec2::new(f32::INFINITY, 0.0),
            in_tangent: crate::Vec2::default(),
        }],
    };
    assert!(matches!(
        project.validate(),
        Err(ModelError::InvalidVisualProperties { .. })
    ));
}

#[test]
fn lint_reports_empty_subpaths_and_paints_without_geometry() {
    let generated = MediaSource::generated(
        id(SourceId::new, "shapes"),
        Generator::Shapes {
            items: items(&format!(
                r#"[{},{{"type":"group","items":[{}]}},{},{{"type":"path","path":{}}}]"#,
                fill(),
                fill(),
                rectangle(),
                constant(r#"{"subpaths":[{"vertices":[]}]}"#)
            )),
        },
    );
    let issues = with_source(generated).lint();
    let shapes: Vec<_> = issues
        .iter()
        .filter_map(|issue| match issue {
            crate::LintIssue::PaintWithoutGeometry { at, .. } => Some(format!("paint {at}")),
            crate::LintIssue::EmptySubpath { at, .. } => Some(format!("empty {at}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        shapes,
        [
            "paint items[0]",
            "paint items[1].items[0]",
            "empty items[3].path.subpaths[0]"
        ]
    );
}
