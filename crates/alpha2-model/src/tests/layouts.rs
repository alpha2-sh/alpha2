//! Layout generators: the box tree's wire form, its defaults, the problems
//! validation names, and lint of its shape nodes.

use super::*;
use crate::{
    Generator, GeneratorIssue, LayoutNode, LayoutProblem, MAX_LAYOUT_NODES, MAX_SHAPE_ITEMS,
    ModelError, ShapeProblem,
};

fn layout(json: &str) -> Generator {
    serde_json::from_str(&format!(r#"{{"type":"layout","root":{json}}}"#)).expect("layout JSON")
}

fn problem(json: &str) -> Result<(), (String, String)> {
    layout(json).validate().map_err(|issue| match issue {
        GeneratorIssue::Layout(issue) => (issue.at, issue.problem.to_string()),
        GeneratorIssue::Shape(issue) => (issue.at, format!("shape: {}", issue.problem)),
        GeneratorIssue::Invalid => (String::new(), "invalid".into()),
    })
}

fn constant(value: &str) -> String {
    format!(r#"{{"mode":"constant","value":{value}}}"#)
}

fn with_source(generator: Generator) -> Project {
    let mut project = project();
    project.sources[0].kind = MediaKind::Generated;
    project.sources[0].locator = String::new();
    project.sources[0].generator = Some(generator);
    project
}

#[test]
fn a_layout_reads_with_css_defaults_and_writes_back_only_what_was_written() {
    let json = format!(
        r#"{{"type":"layout","root":{{"justify_content":"space_between","children":[{{"text":{{"content":{}}}}},{{"flex_wrap":"nowrap"}}]}}}}"#,
        constant(r#""Title""#)
    );
    let generator: Generator = serde_json::from_str(&json).expect("reads");
    let Generator::Layout { root } = &generator else {
        panic!("a layout");
    };
    let text = root.children[0].text.as_ref().expect("text");
    // CSS's `text-align: start`, unlike a text generator's centred default.
    assert_eq!(text.text_align, crate::TextAlign::Start);
    assert_eq!(text.line_height, 1.2);
    assert_eq!(text.font_size, crate::Animated::Constant { value: 0.05 });
    assert_eq!(root.children[1].flex_wrap, Some(crate::FlexWrap::NoWrap));
    let written = serde_json::to_value(&generator).expect("writes");
    assert_eq!(
        written["root"]["children"][1],
        serde_json::json!({ "flex_wrap": "nowrap" })
    );
    assert!(written["root"].get("padding").is_none());
    let back: Generator = serde_json::from_value(written).expect("reads back");
    assert_eq!(back, generator);
}

#[test]
fn css_the_layout_does_not_take_is_refused_by_name() {
    for json in [
        r#"{"type":"layout","root":{"display":"grid"}}"#,
        r#"{"type":"layout","root":{"justify_content":"flex-start"}}"#,
        r#"{"type":"layout","root":{"children":[{"text":{"content":{"mode":"constant","value":"a"},"font_weight":700}}]}}"#,
        r#"{"type":"layout","root":{"margin":{"mode":"constant","value":{"left":"auto"}}}}"#,
    ] {
        assert!(serde_json::from_str::<Generator>(json).is_err(), "{json}");
    }
    let error = serde_json::from_str::<Generator>(r#"{"type":"layout","root":{"display":"grid"}}"#)
        .expect_err("unknown");
    assert!(
        error.to_string().contains("unknown field `display`"),
        "{error}"
    );
}

#[test]
fn structure_problems_name_the_node() {
    assert_eq!(
        problem(r#"{"width":{"mode":"constant","value":{"type":"length","value":0.5}}}"#),
        Err(("root".into(), LayoutProblem::RootItemField.to_string()))
    );
    assert_eq!(
        problem(&format!(
            r#"{{"children":[{{}},{{"text":{{"content":{}}},"align_items":"center"}}]}}"#,
            constant(r#""a""#)
        )),
        Err((
            "root.children[1]".into(),
            LayoutProblem::ContainerFieldOnLeaf.to_string()
        ))
    );
    assert_eq!(
        problem(r#"{"children":[{"shape":{"items":[]},"children":[{}]}]}"#),
        Err((
            "root.children[0]".into(),
            LayoutProblem::MixedContent.to_string()
        ))
    );
    let many = vec!["{}"; MAX_LAYOUT_NODES].join(",");
    assert_eq!(
        problem(&format!(r#"{{"children":[{many}]}}"#)),
        Err((
            format!("root.children[{}]", MAX_LAYOUT_NODES - 1),
            LayoutProblem::TooManyNodes.to_string()
        ))
    );
    // Absolute is a flex item field the root may not take; any node may hold an absolute child.
    assert!(problem(r#"{"children":[{"position":"absolute"}]}"#).is_ok());
}

#[test]
fn shape_items_are_counted_over_every_shape_node_and_named_from_the_root() {
    let fill = r#"{"type":"fill","paint":{"mode":"constant","value":{"type":"solid","color":{"red":1,"green":0,"blue":0,"alpha":1}}}}"#;
    let half = vec![fill; MAX_SHAPE_ITEMS / 2].join(",");
    let node = format!(r#"{{"shape":{{"items":[{half}]}}}}"#);
    assert!(problem(&format!(r#"{{"children":[{node}]}}"#)).is_ok());
    assert_eq!(
        problem(&format!(r#"{{"children":[{node},{node},{node}]}}"#)),
        Err((
            "root.children[2].shape.items[0]".into(),
            format!("shape: {}", ShapeProblem::TooManyItems)
        ))
    );
    let negative = format!(
        r#"{{"children":[{{"shape":{{"items":[{{"type":"ellipse","center":{},"size":{}}}]}}}}]}}"#,
        constant(r#"{"x":0.5,"y":0.5}"#),
        constant(r#"{"x":-1,"y":1}"#)
    );
    let project = with_source(layout(&negative));
    assert_eq!(
        project.validate(),
        Err(ModelError::InvalidShape {
            owner: crate::ShapeOwner::Source(id(SourceId::new, "source-a")),
            at: "root.children[0].shape.items[0].size".into(),
            problem: ShapeProblem::OutOfRange,
        })
    );
}

#[test]
fn a_layout_problem_names_its_owner_in_the_project_error() {
    let project = with_source(layout(
        r#"{"children":[{"opacity":{"mode":"constant","value":2}}]}"#,
    ));
    let error = project.validate().expect_err("out of range");
    assert_eq!(
        error,
        ModelError::InvalidLayout {
            owner: crate::ShapeOwner::Source(id(SourceId::new, "source-a")),
            at: "root.children[0].opacity".into(),
            problem: LayoutProblem::OutOfRange,
        }
    );
    assert_eq!(
        error.to_string(),
        "source source-a has an invalid layout: root.children[0].opacity has a value out of range"
    );
}

#[test]
fn a_layout_draws_text_in_a_source_or_a_node_with_or_without_text() {
    let empty = layout("{}");
    assert!(with_source(empty.clone()).draws_text());
    let mut project = project();
    project.compositions = vec![crate::Composition {
        id: id(crate::CompositionId::new, "titles"),
        nodes: vec![crate::CompositionNode::new(
            id(crate::NodeId::new, "box"),
            crate::NodeOp::Generator { generator: empty },
        )],
        output: Some(id(crate::NodeId::new, "box")),
        inputs: Vec::new(),
        controls: Vec::new(),
    }];
    assert!(project.draws_text());
}

#[test]
fn a_layout_animates_when_any_value_or_shape_in_it_is_keyed() {
    let root = |json: &str| match layout(json) {
        Generator::Layout { root } => root,
        _ => unreachable!(),
    };
    assert!(!root(r#"{"children":[{"opacity":{"mode":"constant","value":0.5}}]}"#).is_animated());
    assert!(
        root(r#"{"children":[{"children":[{"gap":{"mode":"keyframes","keyframes":[{"time":0,"value":0,"interpolation":{"type":"hold"}}]}}]}]}"#)
            .is_animated()
    );
    let isolated: LayoutNode = serde_json::from_str(r#"{"overflow":"hidden"}"#).expect("node");
    assert!(isolated.is_isolated() && !isolated.has_visual());
}

#[test]
fn lint_reports_a_layout_shape_nodes_empty_subpaths() {
    let path = constant(r#"{"subpaths":[{"vertices":[]}]}"#);
    let generated = layout(&format!(
        r#"{{"children":[{{}},{{"shape":{{"items":[{{"type":"path","path":{path}}}]}}}}]}}"#
    ));
    let issues = with_source(generated).lint();
    let empty: Vec<_> = issues
        .iter()
        .filter_map(|issue| match issue {
            crate::LintIssue::EmptySubpath { at, .. } => Some(at.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(empty, ["root.children[1].shape.items[0].path.subpaths[0]"]);
}
