use super::*;
use crate::tests::project;
use crate::{ModelError, Project};
use std::collections::BTreeMap;

fn node_id(value: &str) -> NodeId {
    NodeId::new(value).expect("non-empty")
}

fn effect(id: &str) -> ClipEffect {
    ClipEffect {
        id: id.into(),
        version: 1,
        params: BTreeMap::new(),
        resources: BTreeMap::new(),
    }
}

fn node(id: &str, op: NodeOp) -> CompositionNode {
    CompositionNode::new(node_id(id), op)
}

/// A connected input.
#[allow(clippy::unnecessary_wraps)]
fn input(id: &str) -> Option<NodeId> {
    Some(node_id(id))
}

/// A clip split into a blurred and a graded branch, merged back together.
fn branched() -> Project {
    let mut project = project();
    let id = CompositionId::new("branches").expect("non-empty");
    project.compositions = vec![Composition {
        id: id.clone(),
        // Listed out of order on purpose: order carries no meaning.
        nodes: vec![
            node(
                "merge",
                NodeOp::Merge {
                    layers: vec![MergeLayer::new(input("blur")), {
                        let mut layer = MergeLayer::new(input("grade"));
                        layer.blend_mode = ClipBlendMode::Screen;
                        layer.opacity = Animated::Constant { value: 0.5 };
                        layer
                    }],
                },
            ),
            node(
                "blur",
                NodeOp::Effect {
                    input: input("clip"),
                    inputs: BTreeMap::new(),
                    effect: effect("alpha2.effect.gaussian-blur"),
                },
            ),
            node(
                "grade",
                NodeOp::Effect {
                    input: input("clip"),
                    inputs: BTreeMap::new(),
                    effect: effect("alpha2.effect.brightness"),
                },
            ),
            node("clip", NodeOp::Clip {}),
            node(
                "spare",
                NodeOp::Transform {
                    input: None,
                    transform: crate::VisualProperties::default(),
                },
            ),
        ],
        output: input("merge"),
        inputs: Vec::new(),
        controls: Vec::new(),
    }];
    project.timeline.tracks[0].clips[0].composition = Some(ClipComposition::new(id));
    project
}

#[test]
fn a_branched_graph_with_loose_nodes_validates_and_round_trips() {
    let project = branched();
    project.validate().expect("valid graph");
    let json = serde_json::to_string(&project).expect("serializes");
    assert_eq!(Project::from_json(&json).expect("round trip"), project);
}

#[test]
fn nodes_serialize_with_a_tagged_operation() {
    let value = serde_json::to_value(&branched().compositions[0].nodes[1]).expect("serializes");
    assert_eq!(value["id"], "blur");
    assert_eq!(value["op"]["type"], "effect");
    assert_eq!(value["op"]["input"], "clip");
    let clip: CompositionNode =
        serde_json::from_str(r#"{"id":"c","op":{"type":"clip"}}"#).expect("minimal node");
    assert_eq!(clip.op, NodeOp::Clip {});
    assert!(!clip.bypass);
    assert!(
        serde_json::from_str::<CompositionNode>(r#"{"id":"c","op":{"type":"clip","x":1}}"#)
            .is_err()
    );
    assert!(serde_json::from_str::<CompositionNode>(r#"{"id":"c","op":{"type":"lens"}}"#).is_err());
}

#[test]
fn an_input_naming_an_absent_node_is_rejected_with_its_names() {
    let mut project = branched();
    project.compositions[0].nodes[0].op = NodeOp::Merge {
        layers: vec![
            MergeLayer::new(input("blur")),
            MergeLayer::new(input("gone")),
        ],
    };
    assert_eq!(
        project.validate(),
        Err(ModelError::UnknownNodeInput {
            composition: project.compositions[0].id.clone(),
            node: node_id("merge"),
            input: "layers[1]".into(),
            target: node_id("gone"),
        })
    );
}

#[test]
fn cycles_are_rejected_wherever_they_are() {
    let mut project = branched();
    // clip <- blur <- merge <- spare <- blur: no longer a clip at the root.
    project.compositions[0].nodes[4].op = NodeOp::Transform {
        input: input("merge"),
        transform: crate::VisualProperties::default(),
    };
    project.compositions[0].nodes[1].op = NodeOp::Effect {
        input: input("spare"),
        inputs: BTreeMap::new(),
        effect: effect("alpha2.effect.gaussian-blur"),
    };
    assert!(matches!(
        project.validate(),
        Err(ModelError::CompositionCycle { .. })
    ));

    let mut own = branched();
    own.compositions[0].nodes[4].op = NodeOp::Transform {
        input: input("spare"),
        transform: crate::VisualProperties::default(),
    };
    assert_eq!(
        own.validate(),
        Err(ModelError::CompositionCycle {
            composition: own.compositions[0].id.clone(),
            node: node_id("spare"),
        })
    );
}

#[test]
fn identities_and_the_output_must_resolve() {
    let mut duplicate_node = branched();
    duplicate_node.compositions[0].nodes[4].id = node_id("blur");
    assert!(matches!(
        duplicate_node.validate(),
        Err(ModelError::DuplicateNodeId { .. })
    ));

    // A `/` joins the IDs of the nodes a drawn node was inlined through; a document cannot hold
    // one, and a project built in code is refused.
    let mut separated = branched();
    separated.compositions[0].nodes[4].id = node_id("look/blur");
    assert!(matches!(
        separated.validate(),
        Err(ModelError::InvalidNodeId { .. })
    ));

    let mut duplicate_composition = branched();
    duplicate_composition
        .compositions
        .push(duplicate_composition.compositions[0].clone());
    assert!(matches!(
        duplicate_composition.validate(),
        Err(ModelError::DuplicateCompositionId(_))
    ));

    let mut output = branched();
    output.compositions[0].output = input("nowhere");
    assert!(matches!(
        output.validate(),
        Err(ModelError::UnknownCompositionOutput { .. })
    ));

    let mut unknown = branched();
    unknown.timeline.tracks[0].clips[0].composition = Some(ClipComposition::new(
        CompositionId::new("other").expect("non-empty"),
    ));
    assert!(matches!(
        unknown.validate(),
        Err(ModelError::UnknownComposition { .. })
    ));
}

#[test]
fn node_settings_are_checked() {
    let with_node = |op: NodeOp, bypass: bool| {
        let mut project = branched();
        project.compositions[0].nodes[4] = CompositionNode {
            id: node_id("spare"),
            bypass,
            op,
        };
        project.validate()
    };
    let invalid = |result: Result<(), ModelError>| matches!(result, Err(ModelError::InvalidNode { ref node, .. }) if node.as_str() == "spare");
    // Nothing to pass through.
    assert!(invalid(with_node(NodeOp::Clip {}, true)));
    assert!(invalid(with_node(
        NodeOp::Merge {
            layers: vec![{
                let mut layer = MergeLayer::new(None);
                layer.opacity = Animated::Constant { value: 1.5 };
                layer
            }],
        },
        false,
    )));
    assert!(invalid(with_node(
        NodeOp::Merge {
            layers: vec![MergeLayer::new(None); MAX_MERGE_LAYERS + 1],
        },
        false,
    )));
    assert!(invalid(with_node(
        NodeOp::Transition {
            from: None,
            to: None,
            effect: effect("alpha2.transition.fade"),
            progress: Animated::Constant { value: -0.1 },
        },
        false,
    )));
    assert!(invalid(with_node(
        NodeOp::Effect {
            input: None,
            inputs: BTreeMap::new(),
            effect: effect(""),
        },
        false,
    )));
    with_node(
        NodeOp::Matte {
            input: None,
            matte: None,
            mode: crate::MatteMode::LumaInverted,
        },
        true,
    )
    .expect("a matte passes its input through");
}

#[test]
fn media_nodes_show_visual_sources_only() {
    let mut project = branched();
    project.sources.push(crate::MediaSource::new(
        SourceId::new("music").expect("non-empty"),
        "asset://music",
        crate::MediaKind::Audio,
    ));
    let media = |source: &str| NodeOp::Media {
        source_id: SourceId::new(source).expect("non-empty"),
        start: MediaTime(0),
    };
    project.compositions[0].nodes[4].op = media("source-a");
    project.validate().expect("a video source");
    for source in ["music", "absent"] {
        project.compositions[0].nodes[4].op = media(source);
        assert!(matches!(
            project.validate(),
            Err(ModelError::InvalidNodeSource { .. })
        ));
    }
}

#[test]
fn clip_references_need_a_positive_scale() {
    let mut project = branched();
    let reference = project.timeline.tracks[0].clips[0]
        .composition
        .as_mut()
        .expect("a composition");
    reference.scale = TimeScale { num: 0, den: 1 };
    assert!(matches!(
        project.validate(),
        Err(ModelError::InvalidClipComposition { .. })
    ));
    let mut offset = branched();
    offset.timeline.tracks[0].clips[0]
        .composition
        .as_mut()
        .expect("a composition")
        .offset = MediaTime(-1);
    offset
        .validate()
        .expect("an extended start starts before composition time zero");
}

#[test]
fn composition_time_is_offset_plus_scaled_elapsed_time_rounded_down() {
    let reference = ClipComposition {
        id: CompositionId::new("c").expect("non-empty"),
        offset: MediaTime(1_000),
        scale: TimeScale { num: 2, den: 3 },
    };
    let start = MediaTime(120_000);
    assert_eq!(reference.local_time(start, start), Some(MediaTime(1_000)));
    assert_eq!(
        reference.local_time(start, MediaTime(120_004)),
        Some(MediaTime(1_002))
    );
    assert_eq!(
        reference.local_time(start, MediaTime(120_005)),
        Some(MediaTime(1_003))
    );
    assert_eq!(
        ClipComposition::new(CompositionId::new("c").expect("non-empty"))
            .local_time(start, MediaTime(180_000)),
        Some(MediaTime(60_000))
    );
}

/// A scale with a zero term is no rate, and validation refuses it; a reference holding one, read
/// before or without validation, shows no composition time rather than dividing by zero.
#[test]
fn a_scale_with_a_zero_denominator_shows_no_composition_time() {
    let reference = ClipComposition {
        id: CompositionId::new("c").expect("non-empty"),
        offset: MediaTime(1_000),
        scale: TimeScale { num: 1, den: 0 },
    };
    let start = MediaTime(120_000);
    assert_eq!(reference.local_time(start, start), None);
    assert_eq!(reference.local_time(start, MediaTime(240_000)), None);
}

/// `tests/cases/composition-time-corpus.json`, which @alpha2-sh/editor's `compositionTime` and
/// `timelineTime` read too: an editor that places keyframes by composition time places them where
/// the engine evaluates them.
#[test]
fn composition_time_matches_the_shared_corpus() {
    let corpus: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/cases/composition-time-corpus.json"
    ))
    .expect("the corpus is JSON");
    let int = |value: &serde_json::Value| value.as_i64().expect("an integer");
    for case in corpus["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("a name");
        let scale = if case["scale"].is_null() {
            TimeScale::ONE
        } else {
            TimeScale {
                num: u32::try_from(int(&case["scale"]["num"])).expect("u32"),
                den: u32::try_from(int(&case["scale"]["den"])).expect("u32"),
            }
        };
        let reference = ClipComposition {
            id: CompositionId::new("c").expect("non-empty"),
            offset: MediaTime(int(&case["offset"])),
            scale,
        };
        let start = MediaTime(int(&case["start"]));
        let at = |tick: i64| {
            reference
                .local_time(start, MediaTime(tick))
                .expect("in range")
        };
        for pair in case["time"].as_array().expect("time") {
            assert_eq!(
                at(int(&pair[0])),
                MediaTime(int(&pair[1])),
                "{name}: {pair}"
            );
        }
        // The first tick that shows a composition time: it shows at least that time, and the tick
        // before it shows less.
        for pair in case["first"].as_array().expect("first") {
            let (time, first) = (MediaTime(int(&pair[0])), int(&pair[1]));
            assert!(at(first) >= time, "{name}: {pair}");
            assert!(at(first - 1) < time, "{name}: {pair}");
        }
    }
}

#[test]
fn inputs_and_the_bypass_input_are_listed_per_operation() {
    let merge = NodeOp::Merge {
        layers: vec![MergeLayer::new(input("a")), MergeLayer::new(None)],
    };
    assert_eq!(
        merge
            .inputs()
            .into_iter()
            .map(NodeInput::name)
            .collect::<Vec<_>>(),
        ["layers[0]", "layers[1]"]
    );
    assert_eq!(
        merge.bypass_input().and_then(NodeInput::node),
        Some(&node_id("a"))
    );
    let transition = NodeOp::Transition {
        from: input("x"),
        to: input("y"),
        effect: effect("t"),
        progress: Animated::Constant { value: 0.5 },
    };
    assert_eq!(
        transition.bypass_input().map(NodeInput::name).as_deref(),
        Some("from")
    );
    assert_eq!(NodeOp::Clip {}.bypass_input(), None);
    assert_eq!(NodeOp::Merge { layers: Vec::new() }.bypass_input(), None);
}

/// An effect node reading `input` and the named inputs `inputs`.
fn effect_with_inputs(input_id: Option<NodeId>, inputs: &[(&str, Option<NodeId>)]) -> NodeOp {
    NodeOp::Effect {
        input: input_id,
        inputs: inputs
            .iter()
            .map(|(name, node)| ((*name).to_owned(), node.clone()))
            .collect(),
        effect: effect("test.add-glow"),
    }
}

#[test]
fn named_effect_inputs_round_trip_in_name_order_and_are_omitted_when_empty() {
    let mut project = branched();
    project.compositions[0].nodes[4].op =
        effect_with_inputs(input("clip"), &[("map", input("blur")), ("glow", None)]);
    project.validate().expect("valid graph");
    let json = serde_json::to_value(&project.compositions[0].nodes[4]).expect("serializes");
    assert_eq!(
        json["op"]["inputs"],
        serde_json::json!({ "glow": null, "map": "blur" })
    );
    let text = serde_json::to_string(&project).expect("serializes");
    assert!(text.find("\"glow\"") < text.find("\"map\""), "{text}");
    assert_eq!(Project::from_json(&text).expect("round trip"), project);
    // An effect without named inputs writes none, as it always has.
    let plain = serde_json::to_value(&project.compositions[0].nodes[1]).expect("serializes");
    assert!(plain["op"].get("inputs").is_none(), "{plain}");
    assert_eq!(
        project.compositions[0].nodes[4]
            .op
            .inputs()
            .into_iter()
            .map(NodeInput::name)
            .collect::<Vec<_>>(),
        ["input", "inputs.glow", "inputs.map"]
    );
    assert_eq!(
        project.compositions[0].nodes[4]
            .op
            .bypass_input()
            .map(NodeInput::name)
            .as_deref(),
        Some("input")
    );
}

#[test]
fn a_named_input_naming_an_absent_node_or_closing_a_cycle_is_rejected() {
    let mut dangling = branched();
    dangling.compositions[0].nodes[4].op =
        effect_with_inputs(input("clip"), &[("map", input("gone"))]);
    assert_eq!(
        dangling.validate(),
        Err(ModelError::UnknownNodeInput {
            composition: dangling.compositions[0].id.clone(),
            node: node_id("spare"),
            input: "inputs.map".into(),
            target: node_id("gone"),
        })
    );
    // blur reads spare through a named input, and spare reads blur.
    let mut cycle = branched();
    cycle.compositions[0].nodes[4].op = effect_with_inputs(input("blur"), &[]);
    cycle.compositions[0].nodes[1].op =
        effect_with_inputs(input("clip"), &[("map", input("spare"))]);
    assert!(matches!(
        cycle.validate(),
        Err(ModelError::CompositionCycle { .. })
    ));
}

#[test]
fn named_inputs_are_well_named_and_at_most_eight() {
    let with = |inputs: &[(&str, Option<NodeId>)]| {
        let mut project = branched();
        project.compositions[0].nodes[4].op = effect_with_inputs(input("clip"), inputs);
        project.validate()
    };
    let invalid = |result: Result<(), ModelError>| matches!(result, Err(ModelError::InvalidNode { ref node, .. }) if node.as_str() == "spare");
    for bad in ["", "2map", "a-b", "map.x", "sample src"] {
        assert!(invalid(with(&[(bad, None)])), "{bad:?}");
    }
    let names: Vec<String> = (0..=MAX_EFFECT_INPUTS)
        .map(|index| format!("in{index}"))
        .collect();
    let inputs: Vec<(&str, Option<NodeId>)> =
        names.iter().map(|name| (name.as_str(), None)).collect();
    with(&inputs[..MAX_EFFECT_INPUTS]).expect("eight inputs");
    assert!(invalid(with(&inputs)));
}

#[test]
fn sources_read_through_a_shown_composition_are_used() {
    let mut project = branched();
    let still = SourceId::new("still").expect("non-empty");
    project.sources.push(crate::MediaSource::new(
        still.clone(),
        "asset://still",
        crate::MediaKind::Image,
    ));
    let unused = |project: &Project| {
        project
            .lint()
            .into_iter()
            .filter(|issue| matches!(issue, crate::LintIssue::UnusedSource { .. }))
            .count()
    };
    assert_eq!(unused(&project), 1, "nothing reads the still yet");
    let used = |project: &Project| {
        project
            .used_sources()
            .into_iter()
            .map(|source| source.id.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    // The spare node reads it, but nothing draws a loose node.
    project.compositions[0].nodes[4].op = NodeOp::Media {
        source_id: still.clone(),
        start: MediaTime(0),
    };
    project.validate().expect("valid");
    assert_eq!(used(&project), ["source-a"]);
    assert_eq!(unused(&project), 1, "{:?}", project.lint());
    let NodeOp::Merge { layers } = &mut project.compositions[0].nodes[0].op else {
        unreachable!("the output merges");
    };
    layers.push(MergeLayer::new(input("spare")));
    project.validate().expect("valid");
    assert_eq!(used(&project), ["source-a", "still"]);
    assert_eq!(unused(&project), 0, "{:?}", project.lint());
    project.timeline.tracks[0].clips[0].composition = None;
    assert_eq!(used(&project), ["source-a"]);
}

/// A "soften" definition (one input, a blur exposing its radius) used by a
/// group node in the clip's composition.
fn grouped() -> Project {
    let mut project = project();
    let soften = CompositionId::new("soften").expect("non-empty");
    let look = CompositionId::new("look").expect("non-empty");
    let mut definition = Composition::new(soften.clone());
    definition.nodes = vec![
        node("in", NodeOp::GroupInput { index: 0 }),
        node(
            "blur",
            NodeOp::Effect {
                input: input("in"),
                inputs: BTreeMap::new(),
                effect: effect("alpha2.effect.gaussian-blur"),
            },
        ),
    ];
    definition.output = input("blur");
    definition.inputs = vec!["image".into()];
    definition.controls = vec![CompositionControl {
        name: "softness".into(),
        node: node_id("blur"),
        param: "radius".into(),
    }];
    let mut clip_look = Composition::new(look.clone());
    clip_look.nodes = vec![
        node("clip", NodeOp::Clip {}),
        node(
            "soft",
            NodeOp::Group {
                composition: soften,
                inputs: vec![input("clip")],
                controls: BTreeMap::from([(
                    "softness".to_owned(),
                    Animated::Constant {
                        value: crate::EffectValue::Float(4.0),
                    },
                )]),
            },
        ),
    ];
    clip_look.output = input("soft");
    project.compositions = vec![definition, clip_look];
    project.timeline.tracks[0].clips[0].composition = Some(ClipComposition::new(look));
    project
}

/// The group node of [`grouped`]'s clip composition.
fn group(
    project: &mut Project,
) -> (
    &mut CompositionId,
    &mut Vec<Option<NodeId>>,
    &mut BTreeMap<String, Animated<crate::EffectValue>>,
) {
    match &mut project.compositions[1].nodes[1].op {
        NodeOp::Group {
            composition,
            inputs,
            controls,
        } => (composition, inputs, controls),
        _ => unreachable!(),
    }
}

#[test]
fn group_nodes_use_another_composition_with_its_ports_and_controls() {
    let project = grouped();
    project.validate().expect("a valid group");
    let json = serde_json::to_string(&project).expect("serialize");
    assert_eq!(Project::from_json(&json).expect("load"), project);
    let invalid_node = |project: &Project| {
        matches!(
            project.validate(),
            Err(ModelError::InvalidNode { ref node, .. }) if node.as_str() == "soft"
        )
    };
    // One input per port.
    let mut wrong = grouped();
    group(&mut wrong).1.push(None);
    assert!(invalid_node(&wrong));
    // Only exposed controls, with valid values.
    let mut wrong = grouped();
    group(&mut wrong).2.insert(
        "radius".into(),
        Animated::Constant {
            value: crate::EffectValue::Float(1.0),
        },
    );
    assert!(invalid_node(&wrong));
    let mut wrong = grouped();
    group(&mut wrong).2.insert(
        "softness".into(),
        Animated::Constant {
            value: crate::EffectValue::Float(f32::NAN),
        },
    );
    assert!(invalid_node(&wrong));
    let mut wrong = grouped();
    *group(&mut wrong).0 = CompositionId::new("gone").expect("non-empty");
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::UnknownGroupComposition { .. })
    ));
    // A group input past the ports.
    let mut wrong = grouped();
    wrong.compositions[0].nodes[0].op = NodeOp::GroupInput { index: 1 };
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::InvalidNode { .. })
    ));
    // A control of a node that runs no effect, and a repeated port name.
    let mut wrong = grouped();
    wrong.compositions[0].controls[0].node = node_id("in");
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::InvalidCompositionPorts { .. })
    ));
    let mut wrong = grouped();
    wrong.compositions[0].inputs.push("image".into());
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::InvalidCompositionPorts { .. })
    ));
}

#[test]
fn compositions_may_not_use_themselves() {
    let mut project = grouped();
    let inner = &mut project.compositions[0];
    inner.nodes.push(node(
        "again",
        NodeOp::Group {
            composition: inner.id.clone(),
            inputs: vec![input("in")],
            controls: BTreeMap::new(),
        },
    ));
    assert!(matches!(
        project.validate(),
        Err(ModelError::CompositionUsesItself { .. })
    ));
    // Two compositions that read each other through composition sources.
    let mut project = project_with_composition_sources();
    project.compositions[0].nodes.push(node(
        "back",
        NodeOp::Media {
            source_id: alpha2_media::SourceId::new("wrapped").expect("non-empty"),
            start: MediaTime::ZERO,
        },
    ));
    assert!(matches!(
        project.validate(),
        Err(ModelError::CompositionUsesItself { .. })
    ));
}

/// A "title" composition shown by a composition source, which a second
/// composition reads through a media node.
fn project_with_composition_sources() -> Project {
    let mut project = project();
    let title = CompositionId::new("title").expect("non-empty");
    let wrapper = CompositionId::new("wrapper").expect("non-empty");
    let mut drawn = Composition::new(title.clone());
    drawn.nodes = vec![node(
        "fill",
        NodeOp::Generator {
            generator: crate::Generator::Fill {
                paint: crate::Paint::Solid {
                    color: crate::Rgba::new(1.0, 0.0, 0.0, 1.0),
                },
            },
        },
    )];
    drawn.output = input("fill");
    let mut reads = Composition::new(wrapper.clone());
    reads.nodes = vec![node(
        "title",
        NodeOp::Media {
            source_id: alpha2_media::SourceId::new("title").expect("non-empty"),
            start: MediaTime::ZERO,
        },
    )];
    reads.output = input("title");
    project.compositions = vec![drawn, reads];
    project.sources.push(crate::MediaSource::composition(
        alpha2_media::SourceId::new("title").expect("non-empty"),
        title,
    ));
    project.sources.push(crate::MediaSource::composition(
        alpha2_media::SourceId::new("wrapped").expect("non-empty"),
        wrapper,
    ));
    project
}

#[test]
fn composition_sources_show_compositions_on_the_timeline() {
    let mut project = project_with_composition_sources();
    project.timeline.tracks[0].clips[0].source_id =
        alpha2_media::SourceId::new("wrapped").expect("non-empty");
    project.validate().expect("a composition source clip");
    let json = serde_json::to_string(&project).expect("serialize");
    assert_eq!(Project::from_json(&json).expect("load"), project);
    // It names an existing composition and nothing a host supplies.
    let mut wrong = project.clone();
    wrong.sources[2].composition = Some(CompositionId::new("gone").expect("non-empty"));
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::InvalidCompositionSource { .. })
    ));
    let mut wrong = project.clone();
    wrong.sources[2].locator = "file.mp4".into();
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::InvalidCompositionSource { .. })
    ));
    let mut wrong = project;
    wrong.sources[0].composition = Some(CompositionId::new("title").expect("non-empty"));
    assert!(matches!(
        wrong.validate(),
        Err(ModelError::InvalidCompositionSource { .. })
    ));
}

/// Compositions `c0` ... `c{levels}` that the clip shows from `c0`: each
/// draws the next `uses` times, through group nodes, and the last draws one
/// clip node. `c0` draws `uses^levels` nodes.
fn nested(levels: usize, uses: usize) -> Project {
    let mut project = project();
    let id = |level: usize| CompositionId::new(format!("c{level}")).expect("non-empty");
    project.compositions = (0..=levels)
        .map(|level| {
            let mut composition = Composition::new(id(level));
            composition.nodes = if level == levels {
                vec![node("clip", NodeOp::Clip {})]
            } else {
                (0..uses)
                    .map(|use_| {
                        node(
                            &format!("g{use_}"),
                            NodeOp::Group {
                                composition: id(level + 1),
                                inputs: Vec::new(),
                                controls: BTreeMap::new(),
                            },
                        )
                    })
                    .collect()
            };
            composition.output = Some(composition.nodes[0].id.clone());
            composition
        })
        .collect();
    project.timeline.tracks[0].clips[0].composition = Some(ClipComposition::new(id(0)));
    project
}

#[test]
fn compositions_nest_at_most_the_depth_drawing_one_recurses_through() {
    nested(MAX_COMPOSITION_DEPTH - 1, 1)
        .validate()
        .expect("a chain as deep as the limit");
    assert_eq!(
        nested(MAX_COMPOSITION_DEPTH, 1).validate(),
        Err(ModelError::CompositionNestedTooDeep {
            composition: CompositionId::new("c0").expect("non-empty"),
            maximum: MAX_COMPOSITION_DEPTH,
        })
    );
    // A composition source nests as a group does: c1 reads the last level through one.
    let mut project = nested(MAX_COMPOSITION_DEPTH, 1);
    let last = CompositionId::new(format!("c{MAX_COMPOSITION_DEPTH}")).expect("non-empty");
    project.sources.push(crate::MediaSource::composition(
        alpha2_media::SourceId::new("last").expect("non-empty"),
        last,
    ));
    project.compositions[MAX_COMPOSITION_DEPTH - 1].nodes[0].op = NodeOp::Media {
        source_id: alpha2_media::SourceId::new("last").expect("non-empty"),
        start: MediaTime::ZERO,
    };
    assert!(matches!(
        project.validate(),
        Err(ModelError::CompositionNestedTooDeep { .. })
    ));
}

#[test]
fn compositions_draw_at_most_the_inlined_nodes_their_uses_multiply_to() {
    // c0 draws 2^15 nodes, c1 2^14, ... c15 one: 2^16 - 1 in all, one under the limit.
    let mut project = nested(15, 2);
    project.validate().expect("one node under the limit");
    let mut spare = Composition::new(CompositionId::new("spare").expect("non-empty"));
    spare.nodes = vec![node("clip", NodeOp::Clip {})];
    project.compositions.push(spare.clone());
    project.validate().expect("exactly the limit");
    spare.id = CompositionId::new("another").expect("non-empty");
    project.compositions.push(spare);
    let over = Err(ModelError::TooManyInlinedNodes {
        maximum: MAX_INLINED_NODES,
    });
    assert_eq!(project.validate(), over);
    // A few kilobytes that would draw millions: two uses a level, or thirty-two.
    assert_eq!(nested(20, 2).validate(), over);
    assert_eq!(nested(4, 32).validate(), over);
    // So many levels of two that the count overflows any integer.
    assert!(nested(MAX_COMPOSITION_DEPTH - 1, 2).validate().is_err());
}

#[test]
fn validating_the_longest_chain_of_compositions_keeps_to_a_small_stack() {
    let project = nested(MAX_COMPOSITIONS - 1, 1);
    let result = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || project.validate())
        .expect("validation thread")
        .join()
        .expect("validation did not overflow its stack");
    assert!(matches!(
        result,
        Err(ModelError::CompositionNestedTooDeep { .. })
    ));
}
