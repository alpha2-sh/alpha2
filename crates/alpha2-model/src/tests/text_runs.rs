use serde_json::{Value, json};

use crate::{FontStyle, ModelError, Project, TextRunStyle, TextStyle};

fn with_text(text: &Value) -> Value {
    let mut project = serde_json::to_value(super::project()).expect("serializes");
    project["sources"]
        .as_array_mut()
        .expect("sources")
        .push(json!({
        "id": "title", "locator": "", "kind": "generated",
        "generator": { "type": "text", "text": text, "style": { "font_size": 0.1 } } }));
    project
}

fn with_cue(text: &Value) -> Value {
    let mut project = serde_json::to_value(super::project()).expect("serializes");
    project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .push(
            json!({ "id": "subs", "name": "Subs", "kind": "caption", "captions": {
            "style": { "font_size": 0.05 },
            "cues": [{ "timeline_range": { "start": 0, "end": 4004 }, "text": text }] } }),
        );
    project
}

fn with_layout_text(text: &Value) -> Value {
    let mut project = serde_json::to_value(super::project()).expect("serializes");
    project["sources"]
        .as_array_mut()
        .expect("sources")
        .push(json!({
        "id": "card", "locator": "", "kind": "generated",
        "generator": { "type": "layout", "root": {
            "text": { "mode": "constant", "value": text } } } }));
    project
}

fn load(project: &Value) -> Result<Project, ModelError> {
    match Project::from_json(&project.to_string()) {
        Ok(project) => Ok(project),
        Err(crate::ProjectLoadError::InvalidProject(error)) => Err(error),
        Err(other) => panic!("not a model error: {other:?}"),
    }
}

#[test]
fn a_run_style_patches_the_holder_style() {
    let base = TextStyle {
        font_family: Some("Inter".into()),
        font_weight: 300,
        letter_spacing: 0.1,
        ..TextStyle::default()
    };
    let style: TextRunStyle = serde_json::from_value(json!({
        "font_weight": 700, "font_style": "italic", "underline": true,
        "font_scale": { "mode": "constant", "value": 2.0 } }))
    .expect("a run style");
    let applied = style.apply(&base);
    assert_eq!(applied.font_weight, 700);
    assert_eq!(applied.font_style, FontStyle::Italic);
    assert!(applied.underline && !applied.line_through);
    assert_eq!(applied.font_family.as_deref(), Some("Inter"));
    assert_eq!(applied.letter_spacing.to_bits(), 0.1_f32.to_bits());
    assert_eq!(applied.font_size.to_bits(), base.font_size.to_bits());
    let cleared: TextRunStyle =
        serde_json::from_value(json!({ "font_family": null })).expect("a run style");
    assert_eq!(cleared.apply(&base).font_family, None);
    let runs = json!([{ "text": "big ", "style": { "font_scale": { "mode": "constant", "value": 2 },
                         "background_color": "#ff000080", "line_through": true } },
                      { "text": "word" }]);
    let loaded = load(&with_text(&runs)).expect("a styled run");
    let written = serde_json::to_value(&loaded).expect("writes");
    assert_eq!(load(&written).expect("round trips"), loaded);
}

#[test]
fn a_merged_run_style_out_of_range_is_refused() {
    let scaled = |value: f64| {
        json!([{ "text": "a" }, { "text": "b", "style": {
            "font_scale": { "mode": "constant", "value": value } } }])
    };
    load(&with_text(&scaled(10.0))).expect("the largest scale");
    assert!(matches!(
        load(&with_text(&scaled(10.5))),
        Err(ModelError::InvalidTextRunStyle { run: 1, .. })
    ));
    assert!(matches!(
        load(&with_text(&scaled(0.05))),
        Err(ModelError::InvalidTextRunStyle { run: 1, .. })
    ));
    let strokes = json!([{ "text": "a", "style": { "strokes": [
        { "color": "#000000", "width": 0.3 }, { "color": "#ffffff", "width": 0.3 }] } }]);
    assert!(matches!(
        load(&with_cue(&strokes)),
        Err(ModelError::InvalidCaptionCueStyle { cue: 0, .. })
    ));
    let spacing = json!([{ "text": "a", "style": { "letter_spacing": 2 } }]);
    assert!(matches!(
        load(&with_cue(&spacing)),
        Err(ModelError::InvalidCaptionCueStyle { cue: 0, .. })
    ));
    let data = json!([{ "text": "a", "style": {
        "font_scale": { "mode": "data", "data": "d", "channel": "c" } } }]);
    assert!(matches!(
        load(&with_text(&data)),
        Err(ModelError::InvalidTextRunStyle { run: 0, .. })
    ));
}

#[test]
fn too_many_font_selections_are_refused() {
    let runs = |count: u16| {
        Value::Array(
            (0..count)
                .map(|index| json!({ "text": "a", "style": { "font_weight": 100 + index * 50 } }))
                .collect(),
        )
    };
    let mut repeated = runs(16).as_array().expect("runs").clone();
    repeated.extend(runs(16).as_array().expect("runs").iter().cloned());
    repeated.push(json!({ "text": "b", "style": { "font_family": "INTER" } }));
    repeated.push(json!({ "text": "c", "style": { "font_family": "inter" } }));
    assert!(matches!(
        load(&with_text(&Value::Array(repeated))),
        Err(ModelError::InvalidTextRunStyle { run: 32, .. })
    ));
    load(&with_text(&runs(16))).expect("sixteen selections");
    assert!(matches!(
        load(&with_text(&runs(17))),
        Err(ModelError::InvalidTextRunStyle { run: 16, .. })
    ));
    assert!(matches!(
        load(&with_cue(&runs(17))),
        Err(ModelError::InvalidCaptionCueStyle { cue: 0, .. })
    ));
}

#[test]
fn layout_runs_with_class_time_or_keyframes_are_refused() {
    let styled = json!([{ "text": "big", "style": {
        "font_scale": { "mode": "constant", "value": 1.5 }, "font_weight": 700 } }, { "text": " end" }]);
    load(&with_layout_text(&styled)).expect("a static run style");
    let refused = |text: Value| match load(&with_layout_text(&text)) {
        Err(ModelError::InvalidLayout { at, problem, .. }) => (at, problem),
        other => panic!("expected a layout refusal, got {other:?}"),
    };
    for text in [
        json!([{ "text": "a", "class": ["key"] }]),
        json!([{ "text": "a", "time": { "start": 0, "end": 10 } }]),
        json!([{ "text": "a", "style": { "font_scale": { "mode": "keyframes", "keyframes": [
            { "time": 0, "value": 1, "interpolation": { "type": "linear" } },
            { "time": 10, "value": 2, "interpolation": { "type": "linear" } }] } } }]),
    ] {
        assert_eq!(
            refused(text),
            (
                "root.text".to_owned(),
                crate::LayoutProblem::TextRunNotStatic
            )
        );
    }
    assert_eq!(
        refused(json!([{ "text": "a", "style": { "font_weight": 0 } }])),
        ("root.text".to_owned(), crate::LayoutProblem::OutOfRange)
    );
}
