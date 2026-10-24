use super::*;
use crate::{ActiveIndex, EvalError, FrameNeed, evaluate, frame_needs};
use alpha2_model::{
    ClipComposition, CompositionNode, MediaSource, MergeLayer, NodeOp, Project, ProjectId,
    ProjectSettings, TimeMap, TimeScale, Track, TrackId, TrackKind,
};
use alpha2_time::{FrameRate, TimeRange};
use std::collections::BTreeMap;

fn node_id(value: &str) -> NodeId {
    NodeId::new(value).expect("non-empty")
}

fn node(id: &str, op: NodeOp) -> CompositionNode {
    CompositionNode::new(node_id(id), op)
}

fn effect(input: &str) -> NodeOp {
    NodeOp::Effect {
        input: Some(node_id(input)),
        inputs: BTreeMap::new(),
        effect: ClipEffect {
            id: "alpha2.effect.gaussian-blur".into(),
            version: 1,
            params: BTreeMap::new(),
            resources: BTreeMap::new(),
        },
    }
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime(start), MediaTime(end)).expect("ordered range")
}

fn composition(nodes: Vec<CompositionNode>, output: &str) -> Composition {
    Composition {
        id: CompositionId::new("g").expect("non-empty"),
        nodes,
        output: Some(node_id(output)),
        inputs: Vec::new(),
        controls: Vec::new(),
    }
}

/// One video clip over 1000..2000 reading its source from 5000, shown
/// through `composition`, with a second video and a generated source.
fn project(composition: Composition) -> Project {
    let source = |id: &str| SourceId::new(id).expect("non-empty");
    let mut track = Track::new(TrackId::new("v").expect("id"), "v", TrackKind::Video);
    let mut clip = alpha2_model::Clip::new(
        alpha2_model::ClipId::new("clip").expect("id"),
        source("main"),
        range(1_000, 2_000),
        range(5_000, 6_000),
    );
    clip.composition = Some(ClipComposition::new(composition.id.clone()));
    track.clips.push(clip);
    let mut project = Project::new(
        ProjectId::new("p").expect("id"),
        "p",
        ProjectSettings::new(16, 16, FrameRate::new(24, 1).expect("rate"), 48_000)
            .expect("settings"),
    );
    project.sources = vec![
        MediaSource::new(source("main"), "asset://main", MediaKind::Video),
        MediaSource::new(source("other"), "asset://other", MediaKind::Video),
        MediaSource::generated(
            source("fill"),
            Generator::Fill {
                paint: alpha2_model::Paint::Solid {
                    color: alpha2_model::Rgba::new(1.0, 0.0, 0.0, 1.0),
                },
            },
        ),
    ];
    project.compositions = vec![composition];
    project.timeline.tracks.push(track);
    project.validate().expect("valid project");
    project
}

fn plan(project: &Project) -> Result<CompositionPlan, PlanError> {
    plan_composition(
        &project.compositions[0],
        &project.compositions,
        &project.sources,
        (project.settings.width, project.settings.height),
    )
}

fn step_nodes(plan: &CompositionPlan) -> Vec<&str> {
    plan.steps.iter().map(|step| step.node.as_str()).collect()
}

#[test]
fn steps_follow_dependencies_draw_shared_inputs_once_and_skip_the_unreached() {
    let project = project(composition(
        vec![
            node(
                "merge",
                NodeOp::Merge {
                    layers: vec![
                        MergeLayer::new(Some(node_id("blur"))),
                        MergeLayer::new(Some(node_id("sharp"))),
                    ],
                },
            ),
            node("sharp", effect("clip")),
            node("blur", effect("clip")),
            node("clip", NodeOp::Clip {}),
            node("loose", effect("nowhere-near")),
        ]
        .into_iter()
        // The loose node's input must name a node; point it at the merge.
        .map(|mut node| {
            if node.id.as_str() == "loose" {
                node.op = effect("merge");
            }
            node
        })
        .collect(),
        "merge",
    ));
    let plan = plan(&project).expect("a complete graph");
    assert_eq!(step_nodes(&plan), ["clip", "blur", "sharp", "merge"]);
    assert_eq!(plan.output, 3);
    let StepOp::Merge { layers } = &plan.steps[3].op else {
        panic!("a merge");
    };
    assert_eq!(
        layers.iter().map(|layer| layer.input).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(plan.uses_clip());
}

#[test]
fn a_bypassed_node_is_its_pass_through_input_and_needs_nothing_else() {
    let mut matte = node(
        "matte",
        NodeOp::Matte {
            input: Some(node_id("blur")),
            matte: None,
            mode: MatteMode::Alpha,
        },
    );
    matte.bypass = true;
    let mut blur = node("blur", effect("clip"));
    blur.bypass = true;
    let project = project(composition(
        vec![node("clip", NodeOp::Clip {}), blur, matte],
        "matte",
    ));
    let plan = plan(&project).expect("the unconnected matte input is not needed");
    assert_eq!(step_nodes(&plan), ["clip"]);
    assert_eq!(plan.output, 0);
}

#[test]
fn an_incomplete_output_fails_only_the_frames_that_show_it() {
    let mut project = project(composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node(
                "wipe",
                NodeOp::Transition {
                    from: Some(node_id("clip")),
                    to: None,
                    effect: ClipEffect {
                        id: "alpha2.transition.fade".into(),
                        version: 1,
                        params: BTreeMap::new(),
                        resources: BTreeMap::new(),
                    },
                    progress: alpha2_model::Animated::Constant { value: 0.5 },
                },
            ),
        ],
        "wipe",
    ));
    assert_eq!(
        plan(&project),
        Err(PlanError::MissingInput {
            node: node_id("wipe"),
            input: "to".into(),
        })
    );
    let index = ActiveIndex::build(&project).expect("an incomplete graph still indexes");
    evaluate(&project, &index, MediaTime(999)).expect("the clip is not shown yet");
    assert!(matches!(
        evaluate(&project, &index, MediaTime(1_000)),
        Err(EvalError::IncompleteComposition { .. })
    ));

    project.compositions[0].output = None;
    let index = ActiveIndex::build(&project).expect("indexes");
    assert!(matches!(
        evaluate(&project, &index, MediaTime(1_500)),
        Err(EvalError::IncompleteComposition {
            reason: PlanError::NoOutput,
            ..
        })
    ));
}

#[test]
fn composition_time_moves_with_the_clip_and_follows_offset_and_scale() {
    let mut project = project(composition(
        vec![node("clip", NodeOp::Clip {}), node("blur", effect("clip"))],
        "blur",
    ));
    let local = |project: &Project, time: i64| {
        let index = ActiveIndex::build(project).expect("indexes");
        evaluate(project, &index, MediaTime(time))
            .expect("evaluates")
            .visual[0]
            .composition
            .expect("a composition")
            .time()
    };
    assert_eq!(local(&project, 1_250), MediaTime(250));
    project.timeline.tracks[0].clips[0].timeline_range = range(3_000, 4_000);
    assert_eq!(local(&project, 3_250), MediaTime(250));
    let reference = project.timeline.tracks[0].clips[0]
        .composition
        .as_mut()
        .expect("a reference");
    reference.offset = MediaTime(100);
    reference.scale = TimeScale { num: 1, den: 2 };
    assert_eq!(local(&project, 3_251), MediaTime(225));
    for time_map in [
        TimeMap::Reverse,
        TimeMap::Freeze {
            source_time: MediaTime(5_500),
        },
    ] {
        project.timeline.tracks[0].clips[0].time_map = time_map;
        assert_eq!(local(&project, 3_000), MediaTime(100));
    }
}

#[test]
fn frame_needs_follow_what_the_output_draws() {
    let media = |id: &str, source: &str, start: i64| {
        node(
            id,
            NodeOp::Media {
                source_id: SourceId::new(source).expect("id"),
                start: MediaTime(start),
            },
        )
    };
    let merge = |inputs: &[&str]| NodeOp::Merge {
        layers: inputs
            .iter()
            .map(|input| MergeLayer::new(Some(node_id(input))))
            .collect(),
    };
    // The same source twice, at two times, over a generated source; the clip's own source is not
    // drawn.
    let project = project(composition(
        vec![
            media("early", "other", 0),
            media("late", "other", 40_000),
            media("fill", "fill", 0),
            node("clip", NodeOp::Clip {}),
            node("out", merge(&["fill", "early", "late"])),
        ],
        "out",
    ));
    let plan = plan(&project).expect("complete");
    assert!(matches!(plan.steps[0].op, StepOp::Generator { .. }));
    assert!(!plan.uses_clip());
    let index = ActiveIndex::build(&project).expect("indexes");
    let needs = frame_needs(&project, &index, MediaTime(1_250)).expect("needs");
    let clip = alpha2_model::ClipId::new("clip").expect("id");
    let other = SourceId::new("other").expect("id");
    // Each media step reads with its own cursor, so the two times never make one decoder seek
    // between them.
    assert_eq!(
        needs,
        [
            FrameNeed {
                clip_id: clip.clone(),
                reader: Some(node_id("early")),
                source_id: other.clone(),
                media_time: MediaTime(250),
            },
            FrameNeed {
                clip_id: clip,
                reader: Some(node_id("late")),
                source_id: other,
                media_time: MediaTime(40_250),
            },
        ]
    );
    let frame = evaluate(&project, &index, MediaTime(1_250)).expect("evaluates");
    assert!(!frame.visual[0].needs_source());
}

#[test]
fn frame_needs_past_the_scanned_ones_are_found_through_the_set() {
    use crate::evaluate::{COMPARED, SCANNED_NEEDS};
    // Four times as many reads as the needs a frame scans, of two sources at twice as many times,
    // so repeats of needs on both sides of the scanned ones follow the first of them.
    let reads = 4 * SCANNED_NEEDS;
    let mut nodes = (0..reads)
        .map(|read| {
            let frame = read % (2 * SCANNED_NEEDS);
            let source = if frame.is_multiple_of(3) {
                "main"
            } else {
                "other"
            };
            let start = i64::try_from(frame).expect("small") * 1_000;
            node(
                &format!("read{read}"),
                NodeOp::Media {
                    source_id: SourceId::new(source).expect("id"),
                    start: MediaTime(start),
                },
            )
        })
        .collect::<Vec<_>>();
    // A merge takes at most `MAX_MERGE_LAYERS` layers, so the reads merge in groups first.
    let merge = |inputs: Vec<String>| NodeOp::Merge {
        layers: inputs
            .iter()
            .map(|input| MergeLayer::new(Some(node_id(input))))
            .collect(),
    };
    let groups = (0..reads)
        .collect::<Vec<_>>()
        .chunks(alpha2_model::MAX_MERGE_LAYERS)
        .map(|reads| reads.iter().map(|read| format!("read{read}")).collect())
        .collect::<Vec<Vec<_>>>();
    for (group, inputs) in groups.iter().enumerate() {
        nodes.push(node(&format!("group{group}"), merge(inputs.clone())));
    }
    nodes.push(node(
        "out",
        merge(
            (0..groups.len())
                .map(|group| format!("group{group}"))
                .collect(),
        ),
    ));
    let project = project(composition(nodes, "out"));
    let index = ActiveIndex::build(&project).expect("indexes");
    let time = MediaTime(1_250);

    // What a scan of every earlier need keeps: the first reader of each source frame.
    let frame = evaluate(&project, &index, time).expect("evaluates");
    let composition = frame.visual[0].composition.as_ref().expect("a composition");
    let mut expected: Vec<FrameNeed> = Vec::new();
    for (step, source_id, media_time) in composition.media() {
        if !expected
            .iter()
            .any(|need| need.source_id == *source_id && need.media_time == media_time)
        {
            expected.push(FrameNeed {
                clip_id: frame.visual[0].clip_id.clone(),
                reader: Some(composition.plan().steps[step].node.clone()),
                source_id: source_id.clone(),
                media_time,
            });
        }
    }
    assert_eq!(expected.len(), 2 * SCANNED_NEEDS);

    COMPARED.with(|compared| compared.set(0));
    let needs = frame_needs(&project, &index, time).expect("needs");
    assert_eq!(needs, expected);
    let compared = COMPARED.with(std::cell::Cell::get);
    assert!(
        compared <= reads * SCANNED_NEEDS,
        "{compared} comparisons for {reads} reads"
    );
}

/// A "soften" definition: its one group input blurred, the radius exposed.
fn soften() -> Composition {
    let mut definition = Composition::new(CompositionId::new("soften").expect("non-empty"));
    definition.nodes = vec![
        node("in", NodeOp::GroupInput { index: 0 }),
        node("blur", effect("in")),
    ];
    definition.output = Some(node_id("blur"));
    definition.inputs = vec!["image".into()];
    definition.controls = vec![alpha2_model::CompositionControl {
        name: "softness".into(),
        node: node_id("blur"),
        param: "radius".into(),
    }];
    definition
}

fn radius(value: f32) -> alpha2_model::Animated<EffectValue> {
    alpha2_model::Animated::Constant {
        value: EffectValue::Float(value),
    }
}

#[test]
fn a_group_compiles_to_the_steps_its_nodes_would_inline() {
    // The clip softened twice through the same definition, each instance with its own radius.
    let mut project = project(composition(vec![node("clip", NodeOp::Clip {})], "clip"));
    project.compositions[0] = composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node(
                "first",
                NodeOp::Group {
                    composition: CompositionId::new("soften").expect("non-empty"),
                    inputs: vec![Some(node_id("clip"))],
                    controls: BTreeMap::from([("softness".to_owned(), radius(2.0))]),
                },
            ),
            node(
                "second",
                NodeOp::Group {
                    composition: CompositionId::new("soften").expect("non-empty"),
                    inputs: vec![Some(node_id("first"))],
                    controls: BTreeMap::new(),
                },
            ),
        ],
        "second",
    );
    project.compositions.push(soften());
    project.validate().expect("valid");
    let plan = plan(&project).expect("a complete graph");
    assert_eq!(step_nodes(&plan), ["clip", "first/blur", "second/blur"]);
    let radii = plan
        .steps
        .iter()
        .filter_map(|step| match &step.op {
            StepOp::Effect { input, effect, .. } => {
                Some((*input, effect.params.get("radius").cloned()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    // The first instance sets the radius; the second keeps the definition's own (none: its
    // default).
    assert_eq!(radii, [(0, Some(radius(2.0))), (1, None)]);

    // Drawn as a clip's composition, a definition's group input has nothing to read.
    let mut alone = project.clone();
    alone.timeline.tracks[0].clips[0].composition = Some(ClipComposition::new(
        CompositionId::new("soften").expect("non-empty"),
    ));
    let index = ActiveIndex::build(&alone).expect("index");
    assert!(matches!(
        evaluate(&alone, &index, MediaTime(1_500)),
        Err(EvalError::IncompleteComposition {
            reason: PlanError::UnboundGroupInput { .. },
            ..
        })
    ));
}

#[test]
fn a_composition_source_draws_its_composition_at_the_source_time() {
    // "title" reads the other video from 100 in its own time; "wrapper" reads "title" from 1000.
    let mut project = project(composition(vec![node("clip", NodeOp::Clip {})], "clip"));
    let mut title = Composition::new(CompositionId::new("title").expect("non-empty"));
    title.nodes = vec![node(
        "video",
        NodeOp::Media {
            source_id: SourceId::new("other").expect("non-empty"),
            start: MediaTime(100),
        },
    )];
    title.output = Some(node_id("video"));
    let mut wrapper = Composition::new(CompositionId::new("wrapper").expect("non-empty"));
    wrapper.nodes = vec![
        node(
            "title",
            NodeOp::Media {
                source_id: SourceId::new("title").expect("non-empty"),
                start: MediaTime(1_000),
            },
        ),
        node("blur", effect("title")),
    ];
    wrapper.output = Some(node_id("blur"));
    project
        .compositions
        .extend([title.clone(), wrapper.clone()]);
    project.sources.push(MediaSource::composition(
        SourceId::new("title").expect("non-empty"),
        title.id.clone(),
    ));
    project.sources.push(MediaSource::composition(
        SourceId::new("wrapped").expect("non-empty"),
        wrapper.id.clone(),
    ));
    project.timeline.tracks[0].clips[0].source_id = SourceId::new("wrapped").expect("non-empty");
    project.validate().expect("valid");
    let index = ActiveIndex::build(&project).expect("index");
    // At timeline 1500 the clip reads source time 5500.
    let frame = evaluate(&project, &index, MediaTime(1_500)).expect("frame");
    let clip = &frame.visual[0];
    assert!(!clip.needs_source());
    let source = clip.source_composition.expect("a composition source");
    assert_eq!(source.time(), MediaTime(5_500));
    assert_eq!(step_nodes(source.plan()), ["title/video", "blur"]);
    assert_eq!(source.plan().steps[0].offset, MediaTime(1_000));
    // "other" is read at 100 + 1000 + 5500.
    assert_eq!(
        frame_needs(&project, &index, MediaTime(1_500)).expect("needs"),
        [FrameNeed {
            clip_id: alpha2_model::ClipId::new("clip").expect("id"),
            reader: Some(node_id("/title/video")),
            source_id: SourceId::new("other").expect("non-empty"),
            media_time: MediaTime(6_600),
        }]
    );
    let mut clipped = project.clone();
    clipped.compositions[2]
        .nodes
        .push(node("clip", NodeOp::Clip {}));
    clipped.compositions[2].output = Some(node_id("clip"));
    let index = ActiveIndex::build(&clipped).expect("index");
    assert!(matches!(
        evaluate(&clipped, &index, MediaTime(1_500)),
        Err(EvalError::IncompleteComposition {
            reason: PlanError::ClipWithoutClip { .. },
            ..
        })
    ));
}

/// What a render reads, by [`alpha2_model::Project::used_sources`], is what the
/// frames ask the host for, plus the clips' own sources (whose sound plays
/// even when a composition hides the picture).
#[test]
fn used_sources_are_what_the_frames_read() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/used-sources-corpus.json");
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("used-sources corpus"))
            .expect("JSON");
    for case in cases.as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let project = Project::from_json(&case["project"].to_string()).expect("valid");
        let index = ActiveIndex::build(&project).expect("index");
        let mut read = std::collections::BTreeSet::new();
        let mut complete = true;
        let clips = project
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips);
        for clip in clips {
            read.insert(clip.source_id.as_str().to_owned());
            match frame_needs(&project, &index, clip.timeline_range.start()) {
                Ok(needs) => {
                    read.extend(needs.iter().map(|need| need.source_id.as_str().to_owned()));
                }
                Err(EvalError::IncompleteComposition { .. }) => complete = false,
                Err(error) => panic!("{name}: {error}"),
            }
        }
        let host_supplied = |id: &String| {
            project
                .sources
                .iter()
                .any(|source| source.id.as_str() == id && source.is_host_supplied())
        };
        let read = read.into_iter().filter(host_supplied).collect::<Vec<_>>();
        let used = project
            .used_sources()
            .into_iter()
            .filter(|source| source.is_host_supplied())
            .map(|source| source.id.as_str().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        if complete {
            assert_eq!(used.into_iter().collect::<Vec<_>>(), read, "{name}");
        } else {
            // A frame that cannot draw reads nothing; what is connected is still used.
            assert!(read.iter().all(|id| used.contains(id)), "{name}");
        }
    }
}

/// Compositions `c0` ... `c{levels}`, unvalidated: each draws the next `uses`
/// times through group nodes merged together, and the last draws the clip.
fn nested(levels: usize, uses: usize) -> Vec<Composition> {
    let id = |level: usize| CompositionId::new(format!("c{level}")).expect("non-empty");
    (0..=levels)
        .map(|level| {
            let mut nodes: Vec<_> = if level == levels {
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
            let layers = nodes
                .iter()
                .map(|node| MergeLayer::new(Some(node.id.clone())))
                .collect();
            nodes.push(node("m", NodeOp::Merge { layers }));
            Composition {
                id: id(level),
                nodes,
                output: Some(node_id("m")),
                inputs: Vec::new(),
                controls: Vec::new(),
            }
        })
        .collect()
}

#[test]
fn the_planner_stops_at_the_nesting_and_size_validation_allows() {
    let plan_first = |compositions: &[Composition]| {
        plan_composition(&compositions[0], compositions, &[], (64, 36))
    };
    // Validation refuses both projects; a caller that skips it gets an error, not an exhausted
    // stack or memory.
    let deep = nested(alpha2_model::MAX_COMPOSITION_DEPTH - 1, 1);
    assert!(plan_first(&deep).is_ok());
    let deep = nested(alpha2_model::MAX_COMPOSITION_DEPTH, 1);
    let path = (0..alpha2_model::MAX_COMPOSITION_DEPTH)
        .map(|_| "g0")
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(
        plan_first(&deep),
        Err(PlanError::NestedTooDeep {
            node: node_id(&path)
        })
    );
    let wide = nested(20, 2);
    assert_eq!(plan_first(&wide), Err(PlanError::TooLarge));
    // Each level merges its two uses: 3 * 2^14 - 1 merges and clips.
    let within = nested(14, 2);
    assert_eq!(
        plan_first(&within).expect("within the limit").steps.len(),
        3 * (1 << 14) - 1
    );
}

/// An effect node `id` whose `amount` parameter has `keyframes` keyframes.
fn keyframed_effect(id: &str, input: &str, keyframes: usize) -> CompositionNode {
    let NodeOp::Effect {
        input,
        inputs,
        mut effect,
    } = effect(input)
    else {
        unreachable!("effect() is an effect node");
    };
    effect.params.insert(
        "amount".into(),
        Animated::Keyframes {
            keyframes: (0..keyframes)
                .map(|at| alpha2_model::Keyframe {
                    time: MediaTime(i64::try_from(at).expect("small")),
                    value: EffectValue::Float(0.5),
                    interpolation: alpha2_model::Interpolation::Linear,
                })
                .collect(),
        },
    );
    node(
        id,
        NodeOp::Effect {
            input,
            inputs,
            effect,
        },
    )
}

/// A composition `id` that draws composition `drawn` through `uses` group
/// nodes, merged.
fn drawing(id: &str, drawn: &str, uses: usize) -> Composition {
    let mut nodes = (0..uses)
        .map(|use_| {
            node(
                &format!("use{use_}"),
                NodeOp::Group {
                    composition: CompositionId::new(drawn).expect("id"),
                    inputs: Vec::new(),
                    controls: BTreeMap::new(),
                },
            )
        })
        .collect::<Vec<_>>();
    let layers = nodes
        .iter()
        .map(|node| MergeLayer::new(Some(node.id.clone())))
        .collect();
    nodes.push(node("merge", NodeOp::Merge { layers }));
    Composition {
        id: CompositionId::new(id).expect("id"),
        nodes,
        output: Some(node_id("merge")),
        inputs: Vec::new(),
        controls: Vec::new(),
    }
}

/// A composition `big` of one generator under an effect with `keyframes`
/// keyframes.
fn big(keyframes: usize) -> Composition {
    Composition {
        id: CompositionId::new("big").expect("id"),
        nodes: vec![
            node(
                "fill",
                NodeOp::Generator {
                    generator: Generator::Fill {
                        paint: alpha2_model::Paint::Solid {
                            color: alpha2_model::Rgba::new(1.0, 0.0, 0.0, 1.0),
                        },
                    },
                },
            ),
            keyframed_effect("fx", "fill", keyframes),
        ],
        output: Some(node_id("fx")),
        inputs: Vec::new(),
        controls: Vec::new(),
    }
}

#[test]
fn a_plan_holds_its_steps_settings_and_paths_and_stops_where_its_budget_does() {
    // Inlining copies a node's settings into every step that draws it, so the budget counts
    // each copy.
    let compositions = vec![drawing("wide", "big", 8), big(1_000)];
    let plan_within = |bytes: u64| {
        let mut budget = PlanBudget::new(bytes);
        let plan = plan_composition_within(
            &compositions[0],
            &Lookup::new(&compositions, &[], (64, 36)),
            &mut budget,
        );
        (plan, budget.exceeded())
    };
    let (plan, _) = plan_within(u64::MAX);
    let plan = plan.expect("plans");
    let held = plan.steps.iter().map(step_bytes).sum::<u64>();
    // Each of the eight effect steps holds its thousand keyframes and its path: use<i>/fx.
    let keyframe = u64::try_from(size_of::<alpha2_model::Keyframe<EffectValue>>()).expect("small");
    let effect_step = plan
        .steps
        .iter()
        .find(|step| step.node.as_str() == "use3/fx")
        .expect("an inlined effect");
    assert!(step_bytes(effect_step) >= 1_000 * keyframe + "use3/fx".len() as u64);
    assert!(held >= 8 * 1_000 * keyframe, "{held}");
    assert_eq!(plan_within(held), (Ok(plan), false));
    assert_eq!(plan_within(held - 1), (Err(PlanError::TooLarge), true));
}

#[test]
fn the_plans_of_one_project_hold_at_most_the_plan_budget_together() {
    // A composition drawn many times holds its keyframes once per use: each of these holds about
    // 150 MiB of steps, within the budget alone, past it together.
    let keyframe = size_of::<alpha2_model::Keyframe<EffectValue>>();
    let keyframes = 150 * 1024 * 1024 / 20 / keyframe;
    let mut project = project(composition(vec![node("clip", NodeOp::Clip {})], "clip"));
    project.compositions = vec![
        drawing("a", "big", 20),
        drawing("b", "big", 20),
        big(keyframes),
    ];
    project.timeline.tracks[0].clips[0].composition =
        Some(ClipComposition::new(CompositionId::new("a").expect("id")));
    let alone = plan_composition(
        &project.compositions[0],
        &project.compositions,
        &project.sources,
        (project.settings.width, project.settings.height),
    )
    .expect("one composition is within the budget");
    let held = alone.steps.iter().map(step_bytes).sum::<u64>();
    assert!(held < MAX_PLAN_BYTES && 2 * held > MAX_PLAN_BYTES, "{held}");
    drop(alone);
    project.validate().expect("valid project");
    let index = ActiveIndex::build(&project).expect("indexes");
    evaluate(&project, &index, MediaTime(1_500)).expect("only a is shown: it draws");

    // A second clip showing b: every composition is refused, not whichever was compiled last.
    let mut clip = project.timeline.tracks[0].clips[0].clone();
    clip.id = alpha2_model::ClipId::new("second").expect("id");
    clip.timeline_range = range(3_000, 4_000);
    clip.composition = Some(ClipComposition::new(CompositionId::new("b").expect("id")));
    project.timeline.tracks[0].clips.push(clip);
    project.validate().expect("valid project");
    let index = ActiveIndex::build(&project).expect("indexes");
    for time in [1_500, 3_500] {
        assert!(
            matches!(
                evaluate(&project, &index, MediaTime(time)),
                Err(EvalError::IncompleteComposition {
                    reason: PlanError::TooLarge,
                    ..
                })
            ),
            "at {time}"
        );
    }
}

/// The planner refuses exactly the incomplete compositions of
/// `tests/cases/composition-issues-corpus.json`, each for one of the reasons
/// the corpus lists, for all four reasons a composition is incomplete. A tool
/// that marks incomplete compositions before planning is held to the same
/// corpus.
#[test]
fn the_planner_refuses_what_the_editor_finds_incomplete() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/composition-issues-corpus.json");
    let corpus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("composition-issues corpus"))
            .expect("JSON");
    let cases = corpus["cases"].as_array().expect("cases");
    let mut reasons = std::collections::HashSet::new();
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let compositions: Vec<Composition> =
            serde_json::from_value(case["compositions"].clone()).expect(name);
        let sources: Vec<MediaSource> =
            serde_json::from_value(case["sources"].clone()).expect(name);
        let target = compositions
            .iter()
            .find(|composition| composition.id.as_str() == case["composition"])
            .expect(name);
        let planned = match case["use"].as_str() {
            Some("clip") => plan_composition(target, &compositions, &sources, (64, 36)),
            Some("source") => plan_source_composition(target, &compositions, &sources, (64, 36)),
            other => panic!("{name}: use {other:?}"),
        };
        let issues = case["issues"].as_array().expect(name);
        let Err(reason) = planned else {
            assert!(issues.is_empty(), "{name}: the planner draws it");
            continue;
        };
        let matches = |issue: &serde_json::Value| {
            let node = |id: &NodeId| issue["node"] == id.as_str();
            match (&reason, issue["kind"].as_str()) {
                (PlanError::NoOutput, Some("no_output")) => true,
                (PlanError::MissingInput { node: id, input }, Some("missing_input")) => {
                    node(id)
                        && match &issue["input"] {
                            serde_json::Value::Number(index) => {
                                *input == format!("layers[{index}]")
                                    || *input == format!("inputs[{index}]")
                            }
                            named => named == input.as_str(),
                        }
                }
                (PlanError::UnboundGroupInput { node: id }, Some("unbound_group_input"))
                | (PlanError::ClipWithoutClip { node: id }, Some("clip_without_clip")) => node(id),
                _ => false,
            }
        };
        assert!(
            issues.iter().any(matches),
            "{name}: the planner refuses it with {reason:?}, which the editor does not list in {issues:?}"
        );
        reasons.insert(std::mem::discriminant(&reason));
    }
    assert_eq!(reasons.len(), 4);
}

/// A definition with two ports whose output reads only the first, blurred;
/// the second port's group input is drawn by nothing, or only by a
/// bypassed matte that passes the first through.
fn first_of_two(bypassed_matte: bool) -> Composition {
    let mut definition = Composition::new(CompositionId::new("first-of-two").expect("non-empty"));
    definition.nodes = vec![
        node("a", NodeOp::GroupInput { index: 0 }),
        node("b", NodeOp::GroupInput { index: 1 }),
        node("blur", effect("a")),
    ];
    definition.output = Some(node_id("blur"));
    if bypassed_matte {
        let mut matte = node(
            "keep",
            NodeOp::Matte {
                input: Some(node_id("blur")),
                matte: Some(node_id("b")),
                mode: alpha2_model::MatteMode::Alpha,
            },
        );
        matte.bypass = true;
        definition.nodes.push(matte);
        definition.output = Some(node_id("keep"));
    }
    definition.inputs = vec!["image".into(), "spare".into()];
    definition
}

#[test]
fn a_group_needs_only_the_ports_its_definition_reads() {
    for bypassed_matte in [false, true] {
        let group = |second: Option<&str>| {
            node(
                "g",
                NodeOp::Group {
                    composition: CompositionId::new("first-of-two").expect("non-empty"),
                    inputs: vec![Some(node_id("clip")), second.map(node_id)],
                    controls: BTreeMap::new(),
                },
            )
        };
        let other = node(
            "other",
            NodeOp::Media {
                source_id: SourceId::new("other").expect("non-empty"),
                start: MediaTime::ZERO,
            },
        );
        for second in [None, Some("other")] {
            let mut project = project(composition(vec![node("clip", NodeOp::Clip {})], "clip"));
            project.compositions[0] = composition(
                vec![node("clip", NodeOp::Clip {}), other.clone(), group(second)],
                "g",
            );
            project.compositions.push(first_of_two(bypassed_matte));
            project.validate().expect("valid");
            let plan = plan(&project).expect("the unread port is not needed");
            assert_eq!(step_nodes(&plan), ["clip", "g/blur"], "{second:?}");
            let index = ActiveIndex::build(&project).expect("index");
            let needs = frame_needs(&project, &index, MediaTime(1_500)).expect("needs");
            assert_eq!(
                needs
                    .iter()
                    .map(|need| need.source_id.as_str())
                    .collect::<Vec<_>>(),
                ["main"],
                "{second:?}: only the clip's own source is read"
            );
            assert_eq!(
                project
                    .used_sources()
                    .iter()
                    .map(|source| source.id.as_str())
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from(["main"]),
                "{second:?}"
            );
        }
    }
}

#[test]
fn a_composition_source_the_clip_does_not_draw_is_neither_read_nor_required() {
    let mut project = project(composition(
        vec![node(
            "fill",
            NodeOp::Media {
                source_id: SourceId::new("fill").expect("non-empty"),
                start: MediaTime::ZERO,
            },
        )],
        "fill",
    ));
    // "reads" reads the other video; "unfinished" has no output.
    let mut reads = Composition::new(CompositionId::new("reads").expect("non-empty"));
    reads.nodes = vec![node(
        "video",
        NodeOp::Media {
            source_id: SourceId::new("other").expect("non-empty"),
            start: MediaTime::ZERO,
        },
    )];
    reads.output = Some(node_id("video"));
    let unfinished = Composition::new(CompositionId::new("unfinished").expect("non-empty"));
    project.compositions.extend([reads, unfinished]);
    for (source, composition) in [("reading", "reads"), ("unfinished-source", "unfinished")] {
        project.sources.push(MediaSource::composition(
            SourceId::new(source).expect("non-empty"),
            CompositionId::new(composition).expect("non-empty"),
        ));
    }
    for source in ["reading", "unfinished-source"] {
        let mut project = project.clone();
        project.timeline.tracks[0].clips[0].source_id = SourceId::new(source).expect("non-empty");
        project.validate().expect("valid");
        let index = ActiveIndex::build(&project).expect("index");
        let frame = evaluate(&project, &index, MediaTime(1_500)).expect(source);
        assert!(frame.visual[0].source_composition.is_none(), "{source}");
        assert!(!frame.visual[0].needs_source(), "{source}");
        assert_eq!(
            frame_needs(&project, &index, MediaTime(1_500)).expect(source),
            [],
            "{source}"
        );
        let all = TimeRange::new(MediaTime::ZERO, MediaTime(i64::MAX)).expect("range");
        assert_eq!(index.peak_source_reads(all, |_| true), 0, "{source}");
    }
}

#[test]
fn a_clips_composition_and_its_composition_source_read_through_their_own_cursors() {
    // A node id present in both graphs must still give two readers, one per read.
    let mut project = project(composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node(
                "v",
                NodeOp::Media {
                    source_id: SourceId::new("other").expect("non-empty"),
                    start: MediaTime::ZERO,
                },
            ),
            node(
                "mix",
                NodeOp::Merge {
                    layers: vec![
                        MergeLayer::new(Some(node_id("v"))),
                        MergeLayer::new(Some(node_id("clip"))),
                    ],
                },
            ),
        ],
        "mix",
    ));
    let mut title = Composition::new(CompositionId::new("title").expect("non-empty"));
    title.nodes = vec![node(
        "v",
        NodeOp::Media {
            source_id: SourceId::new("other").expect("non-empty"),
            start: MediaTime(100),
        },
    )];
    title.output = Some(node_id("v"));
    project.compositions.push(title);
    project.sources.push(MediaSource::composition(
        SourceId::new("title").expect("non-empty"),
        CompositionId::new("title").expect("non-empty"),
    ));
    project.timeline.tracks[0].clips[0].source_id = SourceId::new("title").expect("non-empty");
    project.validate().expect("valid");
    let index = ActiveIndex::build(&project).expect("index");
    let needs = frame_needs(&project, &index, MediaTime(1_500)).expect("needs");
    assert_eq!(needs.len(), 2, "{needs:?}");
    assert_ne!(needs[0].media_time, needs[1].media_time);
    assert_ne!(needs[0].reader, needs[1].reader, "{needs:?}");
}

#[test]
fn an_unconnected_input_inside_a_group_is_named_by_its_path() {
    let mut project = project(composition(vec![node("clip", NodeOp::Clip {})], "clip"));
    project.compositions[0] = composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node("blur", effect("clip")),
            node(
                "first",
                NodeOp::Group {
                    composition: CompositionId::new("soften").expect("non-empty"),
                    inputs: vec![Some(node_id("blur"))],
                    controls: BTreeMap::new(),
                },
            ),
        ],
        "first",
    );
    let mut soften = soften();
    soften.nodes[1].op = NodeOp::Effect {
        input: None,
        inputs: BTreeMap::new(),
        effect: ClipEffect {
            id: "alpha2.effect.gaussian-blur".into(),
            version: 1,
            params: BTreeMap::new(),
            resources: BTreeMap::new(),
        },
    };
    soften.nodes.retain(|node| node.id.as_str() != "in");
    soften.inputs.clear();
    project.compositions[0].nodes[2].op = NodeOp::Group {
        composition: CompositionId::new("soften").expect("non-empty"),
        inputs: Vec::new(),
        controls: BTreeMap::new(),
    };
    project.compositions.push(soften);
    project.validate().expect("valid");
    let index = ActiveIndex::build(&project).expect("index");
    let error = evaluate(&project, &index, MediaTime(1_500)).expect_err("incomplete");
    assert!(
        matches!(
            &error,
            EvalError::IncompleteComposition {
                reason: PlanError::MissingInput { node, .. },
                ..
            } if node.as_str() == "first/blur"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "input input of node first/blur in composition g of clip clip is not connected"
    );
}

#[test]
fn reads_and_effects_of_a_shared_composition_are_counted_per_clip_and_walked_once() {
    let mut project = project(composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node("blur", effect("clip")),
            node(
                "other",
                NodeOp::Media {
                    source_id: SourceId::new("other").expect("non-empty"),
                    start: MediaTime::ZERO,
                },
            ),
            node(
                "merge",
                NodeOp::Merge {
                    layers: vec![
                        MergeLayer::new(Some(node_id("other"))),
                        MergeLayer::new(Some(node_id("blur"))),
                    ],
                },
            ),
        ],
        "merge",
    ));
    // Two more clips show it on tracks of their own: 1500..2500 and, meeting it, 2500..3000.
    for (index, span) in [range(1_500, 2_500), range(2_500, 3_000)]
        .into_iter()
        .enumerate()
    {
        let mut track = project.timeline.tracks[0].clone();
        track.id = TrackId::new(format!("v{index}")).expect("id");
        track.clips[0].id = alpha2_model::ClipId::new(format!("clip{index}")).expect("id");
        track.clips[0].timeline_range = span;
        project.timeline.tracks.push(track);
    }
    let index = ActiveIndex::build(&project).expect("index");
    // Each clip reads its own source and the other video: two clips overlap over 1500..2000, and
    // ranges are half-open, so the clip ending at 2500 never meets the one starting there.
    let all = range(0, 10_000);
    assert_eq!(index.peak_source_reads(all, |_| true), 4);
    let other = SourceId::new("other").expect("non-empty");
    assert_eq!(index.peak_source_reads(all, |source| source == &other), 2);
    assert_eq!(index.peak_source_reads(range(2_000, 10_000), |_| true), 2);
    assert_eq!(index.peak_source_reads(range(3_000, 10_000), |_| true), 0);
    // The three clips share one compiled composition, so its one blur is listed once.
    assert_eq!(
        index
            .effects()
            .map(|effect| effect.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha2.effect.gaussian-blur"]
    );
}

/// An effect node reading `input` and, by name, `inputs`.
fn effect_with_inputs(input: &str, inputs: &[(&str, Option<&str>)]) -> NodeOp {
    let NodeOp::Effect { effect, .. } = effect(input) else {
        unreachable!("effect() is an effect node");
    };
    NodeOp::Effect {
        input: Some(node_id(input)),
        inputs: inputs
            .iter()
            .map(|(name, node)| ((*name).to_owned(), node.map(node_id)))
            .collect(),
        effect,
    }
}

#[test]
fn named_inputs_are_planned_before_their_effect_in_name_order() {
    let project = project(composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node(
                "image",
                NodeOp::Media {
                    source_id: SourceId::new("other").expect("non-empty"),
                    start: MediaTime::ZERO,
                },
            ),
            node("blur", effect("clip")),
            node(
                "fx",
                effect_with_inputs("clip", &[("map", Some("image")), ("glow", Some("blur"))]),
            ),
        ],
        "fx",
    ));
    let plan = plan(&project).expect("a complete graph");
    let at = |id: &str| {
        plan.steps
            .iter()
            .position(|step| step.node.as_str() == id)
            .expect("planned")
    };
    let StepOp::Effect { input, inputs, .. } = &plan.steps[plan.output].op else {
        panic!("the output is the effect");
    };
    assert_eq!(*input, at("clip"));
    assert_eq!(
        inputs,
        &[
            ("glow".to_owned(), at("blur")),
            ("map".to_owned(), at("image"))
        ]
    );
    assert!(at("image") < plan.output && at("blur") < plan.output);
}

#[test]
fn a_bypassed_effect_plans_none_of_its_named_inputs() {
    let mut fx = node(
        "fx",
        effect_with_inputs("clip", &[("map", Some("image")), ("glow", None)]),
    );
    fx.bypass = true;
    let project = project(composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node(
                "image",
                NodeOp::Media {
                    source_id: SourceId::new("other").expect("non-empty"),
                    start: MediaTime::ZERO,
                },
            ),
            fx,
        ],
        "fx",
    ));
    let plan = plan(&project).expect("only the pass-through input is needed");
    assert_eq!(step_nodes(&plan), ["clip"]);
}

#[test]
fn an_unconnected_named_input_is_a_missing_input() {
    let project = project(composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node("fx", effect_with_inputs("clip", &[("map", None)])),
        ],
        "fx",
    ));
    assert_eq!(
        plan(&project),
        Err(PlanError::MissingInput {
            node: node_id("fx"),
            input: "inputs.map".into(),
        })
    );
}

#[test]
fn the_named_inputs_of_an_effect_in_a_group_read_the_group_inputs() {
    // A bloom-shaped definition: the effect reads its source and, by name, a blur of it, both
    // fed from the group node's input.
    let mut bloom = Composition::new(CompositionId::new("bloom").expect("non-empty"));
    bloom.nodes = vec![
        node("source", NodeOp::GroupInput { index: 0 }),
        node("blur", effect("source")),
        node(
            "combine",
            effect_with_inputs("source", &[("glow", Some("blur"))]),
        ),
    ];
    bloom.output = Some(node_id("combine"));
    bloom.inputs = vec!["Source".into()];
    let mut project = project(composition(vec![node("clip", NodeOp::Clip {})], "clip"));
    project.compositions[0] = composition(
        vec![
            node("clip", NodeOp::Clip {}),
            node(
                "bloom",
                NodeOp::Group {
                    composition: CompositionId::new("bloom").expect("non-empty"),
                    inputs: vec![Some(node_id("clip"))],
                    controls: BTreeMap::new(),
                },
            ),
        ],
        "bloom",
    );
    project.compositions.push(bloom);
    project.validate().expect("valid");
    let plan = plan(&project).expect("a complete graph");
    assert_eq!(step_nodes(&plan), ["clip", "bloom/blur", "bloom/combine"]);
    let StepOp::Effect { input, inputs, .. } = &plan.steps[plan.output].op else {
        panic!("the output is the effect");
    };
    assert_eq!(*input, 0);
    assert_eq!(inputs, &[("glow".to_owned(), 1)]);
}
