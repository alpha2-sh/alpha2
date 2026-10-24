use crate::{EvaluatedLayout, GeneratorPlan, LayoutPlan, NodeContent, StepOp, plan_composition};
use alpha2_model::{Generator, LayoutLength, LayoutNode};
use alpha2_time::MediaTime;
use std::sync::Arc;

fn root(json: &str) -> LayoutNode {
    serde_json::from_str(json).expect("layout JSON")
}

fn child(layout: &EvaluatedLayout, index: usize) -> &crate::EvaluatedNode {
    match &layout.root.content {
        NodeContent::Children(children) => &children[index],
        other => panic!("children, not {other:?}"),
    }
}

const STILL: &str = r#"{"padding":{"mode":"constant","value":{"left":0.1}},"children":[
    {"text":{"content":{"mode":"constant","value":"Title"}}},
    {"shape":{"items":[]}}]}"#;

const PADDED: &str = r#"{"children":[{"padding":{"mode":"keyframes","keyframes":[
    {"time":0,"value":{"left":0},"interpolation":{"type":"linear"}},
    {"time":1000,"value":{"left":0.2},"interpolation":{"type":"hold"}}]}}]}"#;

#[test]
fn a_tree_without_keyframes_is_one_shared_value_with_one_key() {
    let plan = LayoutPlan::new(&root(STILL));
    assert!(!plan.is_animated());
    let first = plan.evaluate(MediaTime::ZERO);
    let later = plan.evaluate(MediaTime(50_000));
    assert!(Arc::ptr_eq(&first, &later));
    assert_eq!(first.root.layout.padding.left, 0.1);
    // Another tree that lays out alike has the same key; one that does not, another.
    assert_eq!(
        LayoutPlan::new(&root(STILL)).evaluate(MediaTime(7)).key,
        first.key
    );
    let other = STILL.replace("Title", "Titles");
    assert_ne!(
        LayoutPlan::new(&root(&other)).evaluate(MediaTime::ZERO).key,
        first.key
    );
}

#[test]
fn an_animated_value_changes_the_key_only_between_its_keyframes() {
    let plan = LayoutPlan::new(&root(PADDED));
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
    let plan = LayoutPlan::new(&root(
        r#"{"children":[{"text":{"content":{"mode":"keyframes","keyframes":[
            {"time":0,"value":"H","interpolation":{"type":"hold"}},
            {"time":100,"value":"He","interpolation":{"type":"hold"}},
            {"time":200,"value":"Hey","interpolation":{"type":"hold"}}]}}}]}"#,
    ));
    let text = |ticks| match &child(&plan.evaluate(MediaTime(ticks)), 0).content {
        NodeContent::Text(text) => text.content.clone(),
        other => panic!("text, not {other:?}"),
    };
    assert_eq!(
        [text(-1), text(0), text(99), text(100), text(250)],
        ["H", "H", "H", "He", "Hey"]
    );
}

#[test]
fn an_overshoot_is_clamped_to_each_values_domain() {
    let overshoot = r#"{"type":"cubic_bezier","x1":0.3,"y1":-2.0,"x2":0.7,"y2":3.0}"#;
    let plan = LayoutPlan::new(&root(&format!(
        r#"{{"children":[{{
          "opacity":{{"mode":"keyframes","keyframes":[
            {{"time":0,"value":0,"interpolation":{overshoot}}},
            {{"time":1000,"value":1,"interpolation":{{"type":"hold"}}}}]}},
          "width":{{"mode":"keyframes","keyframes":[
            {{"time":0,"value":{{"type":"length","value":0}},"interpolation":{overshoot}}},
            {{"time":1000,"value":{{"type":"length","value":0.5}},"interpolation":{{"type":"hold"}}}}]}},
          "children":[{{"text":{{"content":{{"mode":"constant","value":"a"}},
            "font_size":{{"mode":"keyframes","keyframes":[
              {{"time":0,"value":0.01,"interpolation":{overshoot}}},
              {{"time":1000,"value":0.02,"interpolation":{{"type":"hold"}}}}]}}}}}}]
        }}]}}"#
    )));
    for ticks in [100, 200, 800, 900] {
        let layout = plan.evaluate(MediaTime(ticks));
        let node = child(&layout, 0);
        let opacity = node.visual.expect("a visual").opacity;
        assert!((0.0..=1.0).contains(&opacity), "{opacity}");
        match node.layout.width {
            Some(LayoutLength::Length { value }) => assert!(value >= 0.0, "{value}"),
            other => panic!("a length, not {other:?}"),
        }
        let NodeContent::Children(children) = &node.content else {
            panic!("children");
        };
        let NodeContent::Text(text) = &children[0].content else {
            panic!("text");
        };
        assert!(text.font_size > 0.0);
    }
}

#[test]
fn a_shape_node_animating_puts_the_time_in_the_key() {
    let moving = r#"{"children":[{"shape":{"items":[{"type":"ellipse",
        "center":{"mode":"keyframes","keyframes":[
          {"time":0,"value":{"x":0,"y":0},"interpolation":{"type":"linear"}},
          {"time":1000,"value":{"x":1,"y":0},"interpolation":{"type":"hold"}}]},
        "size":{"mode":"constant","value":{"x":0.1,"y":0.1}}}]}}]}"#;
    let plan = LayoutPlan::new(&root(moving));
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
        GeneratorPlan::new(&generator),
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
    let plan = plan_composition(&composition, std::slice::from_ref(&composition), &[source])
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
