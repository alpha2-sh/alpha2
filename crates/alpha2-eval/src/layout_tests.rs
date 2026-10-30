use crate::{
    EvaluatedLayout, EvaluatedText, GeneratorPlan, LayoutPlan, NodeContent, StepOp,
    plan_composition,
};
use alpha2_model::{Generator, LayoutLength, LayoutNode, Rgba, TextAlign};
use alpha2_time::MediaTime;
use std::sync::Arc;

/// A 1920x1080 canvas: 1080 pixels on its shorter side.
const CANVAS: (u32, u32) = (1920, 1080);

fn root(json: &str) -> LayoutNode {
    serde_json::from_str(json).expect("layout JSON")
}

fn plan(json: &str) -> LayoutPlan {
    LayoutPlan::new(&root(json), CANVAS)
}

fn child(layout: &EvaluatedLayout, index: usize) -> &crate::EvaluatedNode {
    children(&layout.root)
        .get(index)
        .expect("a child at that index")
}

fn children(node: &crate::EvaluatedNode) -> &[crate::EvaluatedNode] {
    match &node.content {
        NodeContent::Children(children) => children,
        other => panic!("children, not {other:?}"),
    }
}

fn text(node: &crate::EvaluatedNode) -> &EvaluatedText {
    match &node.content {
        NodeContent::Text(text) => text,
        other => panic!("text, not {other:?}"),
    }
}

const STILL: &str = r#"{"padding":{"mode":"constant","value":{"left":0.1}},"children":[
    {"text":{"mode":"constant","value":"Title"}},
    {"shape":{"items":[]}}]}"#;

const PADDED: &str = r#"{"children":[{"padding":{"mode":"keyframes","keyframes":[
    {"time":0,"value":{"left":0},"interpolation":{"type":"linear"}},
    {"time":1000,"value":{"left":0.2},"interpolation":{"type":"hold"}}]}}]}"#;

#[test]
fn a_tree_without_keyframes_is_one_shared_value_with_one_key() {
    let plan = plan(STILL);
    assert!(!plan.is_animated());
    let first = plan.evaluate(MediaTime::ZERO);
    let later = plan.evaluate(MediaTime(50_000));
    assert!(Arc::ptr_eq(&first, &later));
    assert_eq!(first.root.layout.padding.left, 0.1);
    // Another tree that lays out alike has the same key; one that does not, another.
    assert_eq!(
        LayoutPlan::new(&root(STILL), CANVAS)
            .evaluate(MediaTime(7))
            .key,
        first.key
    );
    let other = STILL.replace("Title", "Titles");
    assert_ne!(
        LayoutPlan::new(&root(&other), CANVAS)
            .evaluate(MediaTime::ZERO)
            .key,
        first.key
    );
}

#[test]
fn pixels_are_fractions_of_the_canvass_shorter_side_once_planned() {
    let in_px = plan(
        r#"{"padding":{"mode":"constant","value":"108px 54px"},"gap":{"mode":"constant","value":"21.6px"},
            "children":[{"width":{"mode":"constant","value":"540px"},"translate":{"mode":"constant","value":"10.8px"}}]}"#,
    );
    let in_fractions = plan(
        r#"{"padding":{"mode":"constant","value":"0.1 0.05"},"gap":{"mode":"constant","value":0.02},
            "children":[{"width":{"mode":"constant","value":0.5},"translate":{"mode":"constant","value":"0.01"}}]}"#,
    );
    let (px, fractions) = (
        in_px.evaluate(MediaTime::ZERO),
        in_fractions.evaluate(MediaTime::ZERO),
    );
    assert_eq!(px.root.layout.padding.top, 0.1);
    assert_eq!(px.root.layout.padding.left, 0.05);
    assert_eq!(px.root.layout.row_gap, 0.02);
    assert_eq!(
        child(&px, 0).layout.width,
        Some(LayoutLength::Fraction(0.5))
    );
    // The same picture, so the same key: the resolver and its caches see no pixels.
    assert_eq!(px.key, fractions.key);
    // On a canvas twice as large, the same pixels are half the fraction.
    let large = LayoutPlan::new(
        &root(r#"{"children":[{"width":{"mode":"constant","value":"540px"}}]}"#),
        (3840, 2160),
    );
    assert_eq!(
        child(&large.evaluate(MediaTime::ZERO), 0).layout.width,
        Some(LayoutLength::Fraction(0.25))
    );
}

#[test]
fn a_gap_on_one_axis_overrides_gap() {
    let layout = plan(
        r#"{"gap":{"mode":"constant","value":0.01},"row_gap":{"mode":"constant","value":"54px"}}"#,
    )
    .evaluate(MediaTime::ZERO);
    assert_eq!(layout.root.layout.row_gap, 0.05);
    assert_eq!(layout.root.layout.column_gap, 0.01);
}

#[test]
fn text_inherits_its_ancestors_style_and_overrides_it_where_it_sets_it() {
    let layout = plan(
        r##"{"font_family":"Inter","fill":"#ff0000","font_size":{"mode":"constant","value":"54px"},
            "shadows":[{"color":"#000000","offset":{"x":0,"y":0.1},"blur":0.2}],
            "children":[
              {"text":{"mode":"constant","value":"inherits"}},
              {"line_height":1.5,"text_align":"center","children":[
                {"text":{"mode":"constant","value":"overrides"},"fill":"#00ff00","shadows":[],
                 "strokes":[{"color":"#000000","width":0.1}],
                 "font_size":{"mode":"constant","value":0.1}}]}]}"##,
    )
    .evaluate(MediaTime::ZERO);
    let first = text(child(&layout, 0));
    assert_eq!(first.font_family.as_deref(), Some("Inter"));
    assert_eq!(first.font_size, 0.05);
    let solid = |color| alpha2_model::Paint::Solid { color };
    assert_eq!(first.fill, solid(Rgba::new(1.0, 0.0, 0.0, 1.0)));
    // Shadows inherit as a whole list, as CSS inherits `text-shadow`.
    assert_eq!(first.shadows.len(), 1);
    assert_eq!(first.strokes, []);
    assert_eq!(first.line_height, 1.2);
    assert_eq!(first.text_align, TextAlign::Start);
    let nested = text(&children(child(&layout, 1))[0]);
    assert_eq!(nested.font_family.as_deref(), Some("Inter"));
    assert_eq!(nested.font_size, 0.1);
    assert_eq!(nested.fill, solid(Rgba::new(0.0, 1.0, 0.0, 1.0)));
    assert!(
        nested.shadows.is_empty(),
        "an empty list replaces the inherited one"
    );
    assert_eq!(nested.strokes.len(), 1);
    assert_eq!(nested.line_height, 1.5);
    assert_eq!(nested.text_align, TextAlign::Center);
}

#[test]
fn a_text_without_style_draws_in_the_initial_style() {
    let layout = plan(r#"{"children":[{"text":{"mode":"constant","value":"a"}}]}"#)
        .evaluate(MediaTime::ZERO);
    let drawn = text(child(&layout, 0));
    let initial = alpha2_model::LAYOUT_TEXT_DEFAULTS;
    assert_eq!(drawn.font_family, initial.font_family);
    assert_eq!(drawn.font_size, initial.font_size);
    assert_eq!(drawn.fill, initial.fill);
    assert_eq!(drawn.line_height, initial.line_height);
    assert_eq!(drawn.text_align, initial.text_align);
    assert_eq!(drawn.strokes, []);
    assert_eq!(drawn.shadows, []);
}

#[test]
fn an_animated_container_font_size_animates_every_text_that_inherits_it() {
    let plan = plan(
        r#"{"font_size":{"mode":"keyframes","keyframes":[
              {"time":0,"value":"54px","interpolation":{"type":"linear"}},
              {"time":1000,"value":0.1,"interpolation":{"type":"hold"}}]},
            "children":[{"text":{"mode":"constant","value":"a"}},
              {"children":[{"text":{"mode":"constant","value":"b"}}]},
              {"text":{"mode":"constant","value":"c"},"font_size":{"mode":"constant","value":0.02}}]}"#,
    );
    assert!(plan.is_animated());
    let half = plan.evaluate(MediaTime(500));
    assert!((text(child(&half, 0)).font_size - 0.075).abs() < 1e-6);
    assert!((text(&children(child(&half, 1))[0]).font_size - 0.075).abs() < 1e-6);
    assert_eq!(text(child(&half, 2)).font_size, 0.02);
    assert_ne!(half.key, plan.evaluate(MediaTime::ZERO).key);
}

#[test]
fn grid_fields_are_evaluated_with_their_defaults_and_fractions() {
    let layout = plan(
        r#"{"display":"grid","grid_template_columns":"108px 1fr","grid_auto_flow":"column_dense",
            "children":[{"grid_column":"1 / span 2","justify_self":"center"}]}"#,
    )
    .evaluate(MediaTime::ZERO);
    let grid = &layout.root.layout;
    assert_eq!(grid.display, alpha2_model::Display::Grid);
    assert_eq!(
        grid.grid_template_columns
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("0.1 1fr")
    );
    assert_eq!(grid.grid_auto_flow, alpha2_model::GridAutoFlow::ColumnDense);
    let item = &child(&layout, 0).layout;
    assert_eq!(item.grid_column.to_string(), "1 / span 2");
    assert_eq!(item.grid_row, alpha2_model::GridPlacement::default());
    assert_eq!(item.justify_self, Some(alpha2_model::AlignItems::Center));
    let other = plan(
        r#"{"display":"grid","grid_template_columns":"108px 2fr",
            "children":[{"grid_column":"1 / span 2","justify_self":"center"}]}"#,
    );
    assert_ne!(other.evaluate(MediaTime::ZERO).key, layout.key);
}

#[test]
fn an_animated_value_changes_the_key_only_between_its_keyframes() {
    let plan = plan(PADDED);
    assert!(plan.is_animated());
    let key = |ticks| plan.evaluate(MediaTime(ticks)).key;
    assert_eq!(key(-500), key(0));
    assert_ne!(key(0), key(500));
    assert_ne!(key(500), key(999));
    assert_eq!(key(1_000), key(9_000));
    let half = plan.evaluate(MediaTime(500));
    assert!((child(&half, 0).layout.padding.left - 0.1).abs() < 1e-6);
}

#[test]
fn text_holds_each_keyframe_until_the_next() {
    let plan = plan(
        r#"{"children":[{"text":{"mode":"keyframes","keyframes":[
            {"time":0,"value":"H","interpolation":{"type":"hold"}},
            {"time":100,"value":"He","interpolation":{"type":"hold"}},
            {"time":200,"value":"Hey","interpolation":{"type":"hold"}}]}}]}"#,
    );
    let content = |ticks| {
        text(child(&plan.evaluate(MediaTime(ticks)), 0))
            .content
            .clone()
    };
    assert_eq!(
        [
            content(-1),
            content(0),
            content(99),
            content(100),
            content(250)
        ],
        ["H", "H", "H", "He", "Hey"]
    );
}

#[test]
fn an_overshoot_is_clamped_to_each_values_domain() {
    let overshoot = r#"{"type":"cubic_bezier","x1":0.3,"y1":-2.0,"x2":0.7,"y2":3.0}"#;
    let plan = plan(&format!(
        r#"{{"children":[{{
          "opacity":{{"mode":"keyframes","keyframes":[
            {{"time":0,"value":0,"interpolation":{overshoot}}},
            {{"time":1000,"value":1,"interpolation":{{"type":"hold"}}}}]}},
          "width":{{"mode":"keyframes","keyframes":[
            {{"time":0,"value":0,"interpolation":{overshoot}}},
            {{"time":1000,"value":"540px","interpolation":{{"type":"hold"}}}}]}},
          "children":[{{"text":{{"mode":"constant","value":"a"}},
            "font_size":{{"mode":"keyframes","keyframes":[
              {{"time":0,"value":0.01,"interpolation":{overshoot}}},
              {{"time":1000,"value":0.02,"interpolation":{{"type":"hold"}}}}]}}}}]
        }}]}}"#
    ));
    for ticks in [100, 200, 800, 900] {
        let layout = plan.evaluate(MediaTime(ticks));
        let node = child(&layout, 0);
        let opacity = node.visual.expect("a visual").opacity;
        assert!((0.0..=1.0).contains(&opacity), "{opacity}");
        match node.layout.width {
            Some(LayoutLength::Fraction(value)) => assert!(value >= 0.0, "{value}"),
            other => panic!("a fraction, not {other:?}"),
        }
        assert!(text(&children(node)[0]).font_size > 0.0);
    }
}

#[test]
fn a_shape_node_animating_puts_the_time_in_the_key() {
    let moving = r#"{"children":[{"shape":{"items":[{"type":"ellipse",
        "center":{"mode":"keyframes","keyframes":[
          {"time":0,"value":{"x":0,"y":0},"interpolation":{"type":"linear"}},
          {"time":1000,"value":{"x":1,"y":0},"interpolation":{"type":"hold"}}]},
        "size":{"mode":"constant","value":{"x":0.1,"y":0.1}}}]}}]}"#;
    let plan = plan(moving);
    assert!(plan.is_animated());
    let at = |ticks| plan.evaluate(MediaTime(ticks));
    assert_ne!(at(0).key, at(500).key);
    assert_eq!(at(500).time, MediaTime(500));
}

#[test]
fn a_layout_is_planned_for_its_source_its_generator_node_and_a_media_node() {
    let generator = Generator::Layout {
        root: Box::new(root(PADDED)),
    };
    assert!(matches!(
        GeneratorPlan::new(&generator, CANVAS),
        Some(GeneratorPlan::Layout(_))
    ));
    let composition: alpha2_model::Composition = serde_json::from_value(serde_json::json!({
        "id": "g",
        "nodes": [
            { "id": "box", "op": { "type": "generator", "generator": generator } },
            { "id": "read", "op": { "type": "media", "source_id": "layout", "start": 600 } },
            { "id": "top", "op": { "type": "merge", "layers": [
                { "input": "box" }, { "input": "read" } ] } }
        ],
        "output": "top"
    }))
    .expect("composition JSON");
    let source = alpha2_model::MediaSource::generated(
        alpha2_media::SourceId::new("layout").expect("ID"),
        generator.clone(),
    );
    let plan = plan_composition(
        &composition,
        std::slice::from_ref(&composition),
        &[source],
        CANVAS,
    )
    .expect("plans");
    let starts = plan
        .steps
        .iter()
        .filter_map(|step| match &step.op {
            StepOp::Generator {
                generator: Generator::Layout { .. },
                start,
                plan,
            } => {
                assert!(plan.as_deref().and_then(GeneratorPlan::layout).is_some());
                Some(start.ticks())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, [0, 600]);
}
