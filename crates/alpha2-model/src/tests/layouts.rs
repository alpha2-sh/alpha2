//! Layout generators: the box tree's wire form, its defaults, the problems
//! validation names, and lint of its shape nodes.

use super::*;
use crate::{
    Generator, GeneratorIssue, LayoutLength, LayoutNode, LayoutProblem, LayoutSize,
    MAX_LAYOUT_NODES, MAX_SHAPE_ITEMS, ModelError, ShapeProblem,
};

fn layout(json: &str) -> Generator {
    serde_json::from_str(&format!(r#"{{"type":"layout","root":{json}}}"#)).expect("layout JSON")
}

fn issue(issue: GeneratorIssue) -> (String, String) {
    match issue {
        GeneratorIssue::Layout(issue) => (issue.at, issue.problem.to_string()),
        GeneratorIssue::Shape(issue) => (issue.at, format!("shape: {}", issue.problem)),
        GeneratorIssue::Invalid => (String::new(), "invalid".into()),
    }
}

fn problem(json: &str) -> Result<(), (String, String)> {
    layout(json).validate().map_err(issue)
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

fn refused(problem: LayoutProblem, at: &str) -> Result<(), (String, String)> {
    Err((at.into(), problem.to_string()))
}

#[test]
fn a_layout_reads_with_css_defaults_and_writes_back_only_what_was_written() {
    let json = format!(
        r#"{{"type":"layout","root":{{"justify_content":"space_between","children":[{{"text":{}}},{{"flex_wrap":"nowrap"}}]}}}}"#,
        constant(r#""Title""#)
    );
    let generator: Generator = serde_json::from_str(&json).expect("reads");
    let Generator::Layout { root } = &generator else {
        panic!("a layout");
    };
    assert_eq!(
        root.children[0].text,
        Some(crate::Animated::Constant {
            value: "Title".into()
        })
    );
    // CSS's `text-align: start`, unlike a text generator's centred default.
    assert_eq!(
        crate::LAYOUT_TEXT_DEFAULTS.text_align,
        crate::TextAlign::Start
    );
    assert_eq!(crate::LAYOUT_TEXT_DEFAULTS.line_height, 1.2);
    assert_eq!(crate::LAYOUT_TEXT_DEFAULTS.font_size, 0.05);
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
fn lengths_colors_and_style_read_as_css_writes_them_and_write_back_canonically() {
    let generator = layout(&format!(
        r##"{{"display":"grid","grid_template_columns":"1fr repeat(2, 120px)","gap":{},
            "padding":{},"font_family":"Inter","color":"#f5f5f0","font_size":{},
            "background":{},
            "children":[{{"text":{},"grid_column":"1 / 3","width":{},"translate":{}}}]}}"##,
        constant(r#""24px""#),
        constant(r#""48px 64px""#),
        constant(r#""32px""#),
        constant(r##""#1e2230""##),
        constant(r#""Revenue""#),
        constant(r#""50%""#),
        constant(r#""10px 0.1""#),
    ));
    let Generator::Layout { root } = &generator else {
        panic!("a layout");
    };
    assert_eq!(
        root.gap,
        Some(crate::Animated::Constant {
            value: LayoutSize::Px(24.0)
        })
    );
    assert_eq!(
        root.children[0].width,
        Some(crate::Animated::Constant {
            value: LayoutLength::Percent(50.0)
        })
    );
    assert_eq!(
        root.background,
        Some(crate::Animated::Constant {
            value: crate::Paint::Solid {
                color: crate::Rgba::from_hex("#1e2230").expect("hex")
            }
        })
    );
    let written = serde_json::to_value(&generator).expect("writes");
    let root_json = &written["root"];
    assert_eq!(root_json["gap"]["value"], "24px");
    assert_eq!(
        root_json["padding"]["value"],
        serde_json::json!({ "top": "48px", "right": "64px", "bottom": "48px", "left": "64px" })
    );
    assert_eq!(root_json["background"]["value"]["type"], "solid");
    assert!(root_json["color"].is_object());
    assert_eq!(root_json["grid_template_columns"], "1fr repeat(2, 120px)");
    assert_eq!(root_json["children"][0]["grid_column"], "1 / 3");
    assert!(
        serde_json::to_string(&generator)
            .expect("text")
            .contains(r#""translate":{"mode":"constant","value":{"x":"10px","y":0.1}}"#)
    );
    let back: Generator = serde_json::from_value(written).expect("reads back");
    assert_eq!(back, generator);
    assert_eq!(generator.validate(), Ok(()));
}

#[test]
fn the_old_length_and_text_forms_are_refused() {
    for json in [
        // The tagged length objects.
        r#"{"type":"layout","root":{"children":[{"width":{"mode":"constant","value":{"type":"length","value":0.5}}}]}}"#,
        r#"{"type":"layout","root":{"gap":{"mode":"constant","value":{"type":"percent","value":5}}}}"#,
        // The text object.
        r#"{"type":"layout","root":{"children":[{"text":{"content":{"mode":"constant","value":"a"}}}]}}"#,
    ] {
        assert!(serde_json::from_str::<Generator>(json).is_err(), "{json}");
    }
}

#[test]
fn css_the_layout_does_not_take_is_refused_by_name() {
    for json in [
        r#"{"type":"layout","root":{"display":"block"}}"#,
        r#"{"type":"layout","root":{"justify_content":"flex-start"}}"#,
        r#"{"type":"layout","root":{"children":[{"text":{"mode":"constant","value":"a"},"font_weight":700}]}}"#,
        r#"{"type":"layout","root":{"margin":{"mode":"constant","value":{"left":"auto"}}}}"#,
        r#"{"type":"layout","root":{"display":"grid","grid_template_areas":"a b"}}"#,
    ] {
        assert!(serde_json::from_str::<Generator>(json).is_err(), "{json}");
    }
    let error = serde_json::from_str::<Generator>(
        r#"{"type":"layout","root":{"display":"grid","grid_template_areas":"a"}}"#,
    )
    .expect_err("unknown");
    assert!(
        error
            .to_string()
            .contains("unknown field `grid_template_areas`"),
        "{error}"
    );
}

#[test]
fn structure_problems_name_the_node() {
    assert_eq!(
        problem(&format!(r#"{{"width":{}}}"#, constant("0.5"))),
        refused(LayoutProblem::RootItemField, "root")
    );
    assert_eq!(
        problem(r#"{"grid_row":2}"#),
        refused(LayoutProblem::RootItemField, "root")
    );
    assert_eq!(
        problem(&format!(
            r#"{{"children":[{{}},{{"text":{},"align_items":"center"}}]}}"#,
            constant(r#""a""#)
        )),
        refused(LayoutProblem::ContainerFieldOnLeaf, "root.children[1]")
    );
    assert_eq!(
        problem(&format!(
            r#"{{"children":[{{"text":{},"display":"flex"}}]}}"#,
            constant(r#""a""#)
        )),
        refused(LayoutProblem::DisplayOnLeaf, "root.children[0]")
    );
    assert_eq!(
        problem(r#"{"children":[{"shape":{"items":[]},"children":[{}]}]}"#),
        refused(LayoutProblem::MixedContent, "root.children[0]")
    );
    let many = vec!["{}"; MAX_LAYOUT_NODES].join(",");
    assert_eq!(
        problem(&format!(r#"{{"children":[{many}]}}"#)),
        refused(
            LayoutProblem::TooManyNodes,
            &format!("root.children[{}]", MAX_LAYOUT_NODES - 1)
        )
    );
    // Absolute is an item field the root may not take; any node may hold an absolute child.
    assert!(problem(r#"{"children":[{"position":"absolute"}]}"#).is_ok());
}

#[test]
fn a_field_that_does_not_apply_is_refused_naming_it_and_the_parents_display() {
    assert_eq!(
        problem(r#"{"grid_template_columns":"1fr 1fr"}"#),
        refused(
            LayoutProblem::GridFieldOnFlex("grid_template_columns"),
            "root"
        )
    );
    assert_eq!(
        problem(r#"{"display":"grid","flex_direction":"column"}"#),
        refused(LayoutProblem::FlexFieldOnGrid("flex_direction"), "root")
    );
    assert_eq!(
        problem(&format!(
            r#"{{"display":"grid","children":[{{"flex_grow":{}}}]}}"#,
            constant("1")
        )),
        refused(
            LayoutProblem::FlexItemFieldInGrid("flex_grow"),
            "root.children[0]"
        )
    );
    assert_eq!(
        problem(r#"{"children":[{"grid_column":"span 2"}]}"#),
        refused(
            LayoutProblem::GridItemFieldInFlex("grid_column"),
            "root.children[0]"
        )
    );
    assert_eq!(
        LayoutProblem::GridItemFieldInFlex("justify_self").to_string(),
        "sets justify_self, a grid item field, in a flex container"
    );
    // Fields both take, and item fields a grid's child takes.
    assert!(
        problem(&format!(
            r#"{{"display":"grid","gap":{},"align_content":"center","justify_items":"end",
                "children":[{{"grid_row":"2 / span 2","justify_self":"center","align_self":"end"}}]}}"#,
            constant("0.01")
        ))
        .is_ok()
    );
    assert!(problem(r#"{"align_content":"space_between","flex_wrap":"wrap"}"#).is_ok());
}

#[test]
fn grid_tracks_and_lines_are_bounded() {
    let grid = |field: &str| format!(r#"{{"display":"grid",{field}}}"#);
    assert!(problem(&grid(r#""grid_template_columns":"repeat(64, 1fr)""#)).is_ok());
    assert_eq!(
        problem(&grid(
            r#""grid_template_columns":"repeat(32, 1fr 1fr) 1fr""#
        )),
        refused(LayoutProblem::TooManyTracks, "root.grid_template_columns")
    );
    assert_eq!(
        problem(&grid(r#""grid_template_rows":"repeat(0, 1fr)""#)),
        refused(LayoutProblem::RepeatCount, "root.grid_template_rows")
    );
    assert_eq!(
        problem(&grid(r#""grid_template_rows":"repeat(65, 1px)""#)),
        refused(LayoutProblem::RepeatCount, "root.grid_template_rows")
    );
    assert_eq!(
        problem(&grid(r#""grid_auto_rows":"repeat(2, 1fr)""#)),
        refused(LayoutProblem::RepeatInAutoTracks, "root.grid_auto_rows")
    );
    assert!(
        problem(&grid(
            r#""grid_template_columns":"repeat(auto-fill, minmax(160px, 1fr)) 20px""#
        ))
        .is_ok()
    );
    for tracks in [
        "repeat(auto-fill, 1fr)",
        "repeat(auto-fit, 10px) auto",
        "repeat(auto-fit, 10px) repeat(auto-fill, 10px)",
        "repeat(auto-fill, fit-content(10px))",
    ] {
        assert_eq!(
            problem(&grid(&format!(r#""grid_template_columns":"{tracks}""#))),
            refused(
                LayoutProblem::AutoRepeatNeedsFixedTracks,
                "root.grid_template_columns"
            ),
            "{tracks}"
        );
    }
    assert_eq!(
        problem(&grid(r#""grid_template_columns":"0fr""#)),
        refused(LayoutProblem::OutOfRange, "root.grid_template_columns")
    );
    assert_eq!(
        problem(&grid(r#""grid_template_columns":"-10px""#)),
        refused(LayoutProblem::OutOfRange, "root.grid_template_columns")
    );
    let item = |placement: &str| {
        problem(&format!(
            r#"{{"display":"grid","children":[{{"grid_column":{placement}}}]}}"#
        ))
    };
    assert!(item("65").is_ok() && item("-65").is_ok() && item(r#""span 64""#).is_ok());
    for placement in ["0", "66", r#""1 / -66""#, r#""span 65""#, r#""span 0""#] {
        assert_eq!(
            item(placement),
            refused(
                LayoutProblem::GridLineOutOfRange,
                "root.children[0].grid_column"
            ),
            "{placement}"
        );
    }
}

#[test]
fn text_style_is_checked_where_it_is_written_and_pixels_against_the_canvas() {
    let text = constant(r#""a""#);
    assert_eq!(
        problem(&format!(
            r#"{{"line_height":4,"children":[{{"text":{text}}}]}}"#
        )),
        refused(LayoutProblem::OutOfRange, "root.line_height")
    );
    assert_eq!(
        problem(&format!(
            r#"{{"children":[{{"font_family":"","children":[{{"text":{text}}}]}}]}}"#
        )),
        refused(LayoutProblem::OutOfRange, "root.children[0].font_family")
    );
    assert_eq!(
        problem(&format!(r#"{{"font_size":{}}}"#, constant("1.5"))),
        refused(LayoutProblem::OutOfRange, "root.font_size")
    );
    // 1200 canvas pixels: over the 1080-pixel shorter side of the project, unknown without it.
    let big = layout(&format!(r#"{{"font_size":{}}}"#, constant(r#""1200px""#)));
    assert_eq!(big.validate(), Ok(()));
    assert_eq!(
        big.validate_on(1920, 1080).map_err(issue),
        refused(LayoutProblem::OutOfRange, "root.font_size")
    );
    assert_eq!(big.validate_on(1920, 1200), Ok(()));
    assert_eq!(
        with_source(big).validate(),
        Err(ModelError::InvalidLayout {
            owner: crate::ShapeOwner::Source(id(SourceId::new, "source-a")),
            at: "root.font_size".into(),
            problem: LayoutProblem::OutOfRange,
        })
    );
    assert_eq!(
        problem(
            r#"{"children":[{"text":{"mode":"keyframes","keyframes":[{"time":0,"value":"a","interpolation":{"type":"linear"}}]}}]}"#
        ),
        refused(
            LayoutProblem::TextKeyframesInterpolate,
            "root.children[0].text"
        )
    );
}

#[test]
fn pixels_and_fractions_are_one_kind_of_length_in_keyframes() {
    let keyed = |first: &str, second: &str| {
        problem(&format!(
            r#"{{"children":[{{"width":{{"mode":"keyframes","keyframes":[
                {{"time":0,"value":{first},"interpolation":{{"type":"linear"}}}},
                {{"time":10,"value":{second},"interpolation":{{"type":"linear"}}}}]}}}}]}}"#
        ))
    };
    assert!(keyed(r#""12px""#, "0.5").is_ok());
    assert_eq!(
        keyed(r#""12px""#, r#""50%""#),
        refused(
            LayoutProblem::LengthKeyframesDiffer,
            "root.children[0].width"
        )
    );
}

#[test]
fn a_plan_tree_holds_fractions_only() {
    let Generator::Layout { root } = layout(&format!(
        r#"{{"display":"grid","grid_template_columns":"108px 1fr","padding":{},
            "children":[{{"width":{}}}]}}"#,
        constant(r#""54px 0.1""#),
        constant(r#""216px""#),
    )) else {
        unreachable!()
    };
    let planned = root.without_px(1080.0);
    assert_eq!(
        planned
            .grid_template_columns
            .as_ref()
            .map(ToString::to_string),
        Some("0.1 1fr".into())
    );
    assert_eq!(
        planned.padding,
        Some(crate::Animated::Constant {
            value: crate::Insets::from_sides([
                LayoutSize::Fraction(0.05),
                LayoutSize::Fraction(0.1),
                LayoutSize::Fraction(0.05),
                LayoutSize::Fraction(0.1),
            ])
        })
    );
    assert_eq!(
        planned.children[0].width,
        Some(crate::Animated::Constant {
            value: LayoutLength::Fraction(0.2)
        })
    );
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
    assert!(
        root(r#"{"font_size":{"mode":"keyframes","keyframes":[{"time":0,"value":"10px","interpolation":{"type":"hold"}}]}}"#)
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
