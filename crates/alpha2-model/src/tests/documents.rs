//! Project documents: JSON round trips, schema versions, defaults, effect values and resources, colour, and the limits validation enforces.

use super::*;

/// `project`'s first clip shown through one effect node.
pub(crate) fn with_effect(mut project: Project, effect: crate::ClipEffect) -> Project {
    let node = |value: &str| id(crate::NodeId::new, value);
    let composition = id(crate::CompositionId::new, "look");
    project.compositions = vec![crate::Composition {
        id: composition.clone(),
        nodes: vec![
            crate::CompositionNode::new(node("clip"), crate::NodeOp::Clip {}),
            crate::CompositionNode::new(
                node("effect"),
                crate::NodeOp::Effect {
                    input: Some(node("clip")),
                    inputs: std::collections::BTreeMap::new(),
                    effect,
                },
            ),
        ],
        output: Some(node("effect")),
        inputs: Vec::new(),
        controls: Vec::new(),
    }];
    project.timeline.tracks[0].clips[0].composition =
        Some(crate::ClipComposition::new(composition));
    project
}

#[test]
fn a_project_draws_text_through_a_caption_track_a_source_or_a_composition() {
    let text = || crate::Generator::Text {
        text: "Title".into(),
        style: crate::TextStyle::default(),
    };
    let fill = crate::Generator::Fill {
        paint: crate::Paint::Solid {
            color: crate::Rgba::new(1.0, 1.0, 1.0, 1.0),
        },
    };
    assert!(!project().draws_text());
    let mut captioned = project();
    captioned.timeline.tracks[0].captions = Some(crate::Captions::default());
    assert!(captioned.draws_text());
    let with_source = |generator: crate::Generator| {
        let mut project = project();
        project.sources[0].generator = Some(generator);
        project
    };
    assert!(with_source(text()).draws_text());
    assert!(!with_source(fill.clone()).draws_text());
    let with_node = |generator: crate::Generator| {
        let mut project = project();
        project.compositions = vec![crate::Composition {
            id: id(crate::CompositionId::new, "titles"),
            nodes: vec![crate::CompositionNode::new(
                id(crate::NodeId::new, "title"),
                crate::NodeOp::Generator { generator },
            )],
            output: Some(id(crate::NodeId::new, "title")),
            inputs: Vec::new(),
            controls: Vec::new(),
        }];
        project
    };
    assert!(with_node(text()).draws_text());
    assert!(!with_node(fill).draws_text());
}

#[test]
fn a_valid_project_round_trips_through_json() {
    let project = project();
    project.validate().expect("valid project");
    let json = serde_json::to_string(&project).expect("serialize project");
    let decoded: Project = serde_json::from_str(&json).expect("deserialize project");
    assert_eq!(decoded, project);
}

#[test]
fn validation_rejects_cross_domain_clips() {
    let mut project = project();
    project.sources[0].kind = MediaKind::Audio;
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::IncompatibleClipSource { .. })
    ));
}

#[test]
fn deserialization_rejects_reversed_ranges() {
    let json = r#"{"start":10,"end":9}"#;
    let result = serde_json::from_str::<TimeRange>(json);
    assert!(result.is_err());
}

#[test]
fn documents_omitting_optional_visual_fields_receive_defaults() {
    let value = serde_json::to_value(project()).expect("serialize project");
    let mut value = value;
    let clip = value["timeline"]["tracks"][0]["clips"][0]
        .as_object_mut()
        .expect("clip object");
    clip.remove("composition");
    clip.remove("blend_mode");
    let decoded: Project = serde_json::from_value(value).expect("optional fields are omittable");
    assert!(decoded.timeline.tracks[0].clips[0].composition.is_none());
    assert_eq!(
        decoded.timeline.tracks[0].clips[0].blend_mode,
        crate::ClipBlendMode::Normal
    );
}

#[test]
fn a_partial_visual_takes_the_default_of_each_field_it_leaves_out() {
    let visual: crate::VisualProperties =
        serde_json::from_str(r#"{"opacity": {"mode": "constant", "value": 0.25}}"#)
            .expect("a partial visual");
    assert_eq!(
        visual,
        crate::VisualProperties {
            opacity: crate::Animated::Constant { value: 0.25 },
            ..crate::VisualProperties::default()
        }
    );
    let written = serde_json::to_value(&visual).expect("serialize");
    let mut fields = written
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fields.sort();
    assert_eq!(
        fields,
        ["anchor", "opacity", "position", "rotation_degrees", "scale"]
    );
    let empty: crate::VisualProperties = serde_json::from_str("{}").expect("an empty visual");
    assert_eq!(empty, crate::VisualProperties::default());
}

#[test]
fn validation_rejects_invalid_effect_values() {
    let project = with_effect(
        project(),
        crate::ClipEffect {
            id: "test.tint".into(),
            version: 1,
            resources: BTreeMap::new(),
            params: BTreeMap::from([(
                "tint".into(),
                crate::Animated::Constant {
                    value: crate::EffectValue::Float(f32::NAN),
                },
            )]),
        },
    );
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidNode { .. })
    ));
}

#[test]
fn effect_parameters_animate_with_one_type_and_hold_only_discrete_values() {
    use crate::{Animated, EffectValue, Interpolation, Keyframe};

    let keyframes = |values: [(EffectValue, Interpolation); 2]| Animated::Keyframes {
        keyframes: values
            .into_iter()
            .zip([0, 60_000])
            .map(|((value, interpolation), time)| Keyframe {
                time: MediaTime(time),
                value,
                interpolation,
            })
            .collect(),
    };
    let with_param = |param: Animated<EffectValue>| {
        with_effect(
            project(),
            crate::ClipEffect {
                id: "alpha2.effect.gaussian-blur".into(),
                version: 1,
                resources: BTreeMap::new(),
                params: BTreeMap::from([("radius".into(), param)]),
            },
        )
        .validate()
    };
    let hold = Interpolation::Hold;
    let linear = Interpolation::Linear;

    with_param(keyframes([
        (EffectValue::Float(0.0), linear),
        (EffectValue::Float(0.5), hold),
    ]))
    .expect("a keyframed float");
    with_param(keyframes([
        (EffectValue::Color([0.0, 0.0, 0.0, 1.0]), linear),
        (EffectValue::Color([1.0, 0.5, 0.0, 1.0]), hold),
    ]))
    .expect("a keyframed color");
    with_param(keyframes([
        (EffectValue::Enum(0), hold),
        (EffectValue::Enum(2), hold),
    ]))
    .expect("held enum keyframes");

    for invalid in [
        // A float cannot become a vector halfway.
        keyframes([
            (EffectValue::Float(0.0), linear),
            (EffectValue::Vec2([0.0, 0.0]), hold),
        ]),
        // Nothing lies between two enum choices or two booleans.
        keyframes([(EffectValue::Enum(0), linear), (EffectValue::Enum(1), hold)]),
        keyframes([
            (EffectValue::Bool(false), linear),
            (EffectValue::Bool(true), hold),
        ]),
        // Each keyframe value is valid on its own.
        keyframes([
            (EffectValue::Color([0.0, 0.0, 0.0, 1.0]), linear),
            (EffectValue::Color([2.0, 0.0, 0.0, 1.0]), hold),
        ]),
        Animated::Keyframes {
            keyframes: Vec::new(),
        },
    ] {
        assert!(
            matches!(
                with_param(invalid.clone()),
                Err(crate::ModelError::InvalidNode { .. })
            ),
            "{invalid:?}"
        );
    }

    let wire: crate::ClipEffect = serde_json::from_str(
        r#"{"id":"e","version":1,"params":{"radius":{"mode":"keyframes","keyframes":[
            {"time":0,"value":{"type":"float","value":0},"interpolation":{"type":"linear"}},
            {"time":60000,"value":{"type":"float","value":0.5},"interpolation":{"type":"hold"}}]}}}"#,
    )
    .expect("keyframed parameter JSON");
    assert!(matches!(wire.params["radius"], Animated::Keyframes { .. }));
    // A bare value is not an animated parameter.
    assert!(
        serde_json::from_str::<crate::ClipEffect>(
            r#"{"id":"e","version":1,"params":{"radius":{"type":"float","value":0}}}"#
        )
        .is_err()
    );
}

#[test]
fn effect_resources_are_typed_and_references_must_resolve() {
    let mut project = with_effect(
        project(),
        crate::ClipEffect {
            id: "alpha2.effect.lut-3d".into(),
            version: 1,
            resources: std::collections::BTreeMap::from([(
                "lut".into(),
                crate::EffectResourceId::new("look").expect("id"),
            )]),
            params: BTreeMap::new(),
        },
    );
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::UnknownNodeResource { .. })
    ));
    project.effect_resources.push(crate::EffectResource {
        id: crate::EffectResourceId::new("look").expect("id"),
        kind: crate::EffectResourceKind::Lut3d,
        locator: "asset://looks/cinematic.cube".into(),
    });
    project.validate().expect("resource reference resolves");
    let json = serde_json::to_string(&project).expect("serializes");
    assert_eq!(Project::from_json(&json).expect("round trip"), project);
}

#[test]
fn the_background_defaults_to_black_and_must_be_in_range() {
    let mut project = project();
    let mut json: serde_json::Value = serde_json::to_value(&project).expect("serializes");
    json["settings"]
        .as_object_mut()
        .expect("settings object")
        .remove("background");
    let parsed = Project::from_json(&json.to_string()).expect("background is optional");
    assert_eq!(parsed.settings.background, crate::Rgb::BLACK);

    project.settings.background = crate::Rgb::new(0.25, 0.5, 1.0);
    project.validate().expect("in-range background");
    let json = serde_json::to_string(&project).expect("serializes");
    assert_eq!(Project::from_json(&json).expect("round trip"), project);

    for invalid in [
        crate::Rgb::new(1.5, 0.0, 0.0),
        crate::Rgb::new(0.0, -0.1, 0.0),
        crate::Rgb::new(0.0, 0.0, f32::NAN),
    ] {
        project.settings.background = invalid;
        assert_eq!(
            project.validate(),
            Err(crate::ModelError::InvalidBackground)
        );
    }
    assert_eq!(
        project
            .settings
            .clone()
            .with_background(crate::Rgb::new(2.0, 0.0, 0.0)),
        Err(crate::ModelError::InvalidBackground)
    );
}

#[test]
fn validation_enforces_resource_limits_before_rendering() {
    let mut invalid_rate = project();
    invalid_rate.settings.sample_rate = 4_000_000_000;
    assert!(matches!(
        invalid_rate.validate(),
        Err(crate::ModelError::UnsupportedSampleRate { .. })
    ));

    let mut invalid_canvas = project();
    invalid_canvas.settings.width = u32::MAX;
    assert!(matches!(
        invalid_canvas.validate(),
        Err(crate::ModelError::CanvasDimensionTooLarge { .. })
    ));

    let mut nodes = with_effect(
        project(),
        crate::ClipEffect {
            id: "bounded.effect".into(),
            version: 1,
            resources: BTreeMap::new(),
            params: BTreeMap::new(),
        },
    );
    let template = nodes.compositions[0].nodes[1].clone();
    nodes.compositions[0].nodes = (0..=crate::MAX_COMPOSITION_NODES)
        .map(|index| crate::CompositionNode {
            id: id(crate::NodeId::new, &format!("n{index}")),
            ..template.clone()
        })
        .collect();
    nodes.compositions[0].output = None;
    assert!(matches!(
        nodes.validate(),
        Err(crate::ModelError::TooManyCompositionNodes { .. })
    ));
}

#[test]
fn from_json_rejects_documents_over_the_parser_budget() {
    let oversized = " ".repeat(crate::MAX_PROJECT_JSON_BYTES + 1);
    assert!(matches!(
        Project::from_json(&oversized),
        Err(crate::ProjectLoadError::TooLarge { .. })
    ));
}

#[test]
fn disjoint_clips_at_the_track_limit_validate() {
    let mut project = project();
    let template = project.timeline.tracks[0].clips[0].clone();
    project.timeline.tracks[0].clips = (0..crate::MAX_CLIPS_PER_TRACK)
        .map(|index| {
            let start = i64::try_from(index).expect("index") * 2;
            let mut clip = template.clone();
            clip.id = id(ClipId::new, &format!("clip-{index}"));
            clip.timeline_range =
                TimeRange::new(MediaTime(start), MediaTime(start + 1)).expect("ordered range");
            clip
        })
        .collect();
    project.validate().expect("bounded disjoint clips validate");
}

#[test]
fn the_overlap_check_scales_without_comparing_all_pairs() {
    // Far more clips than a track holds, so comparing every pair could not finish: 2 * 10^10
    // comparisons for 200,000 clips. Each run gets its own track: reusing one keeps the small
    // track in cache across runs while the large one is not, which on an Apple M4 reads as 30x
    // growth for linear work.
    let template = project().timeline.tracks[0].clips[0].clone();
    let with_clips = |count: usize| {
        let mut track = project().timeline.tracks[0].clone();
        track.clips = (0..i64::try_from(count).expect("count"))
            .map(|index| {
                let mut clip = template.clone();
                clip.id = id(ClipId::new, &format!("clip-{index}"));
                clip.timeline_range =
                    TimeRange::new(MediaTime(index * 2), MediaTime(index * 2 + 1)).expect("range");
                clip
            })
            .collect();
        track
    };
    alpha2_scaling_support::assert_linear(
        "the overlap check",
        200_000 / alpha2_scaling_support::GROWTH,
        with_clips,
        |track| crate::project::ensure_overlaps_have_transitions(track).expect("disjoint clips"),
    );
}

#[test]
fn audio_clips_cannot_show_a_composition() {
    let mut project = with_effect(
        project(),
        crate::ClipEffect {
            id: "e".into(),
            version: 1,
            resources: BTreeMap::new(),
            params: BTreeMap::new(),
        },
    );
    project.sources[0].kind = MediaKind::Audio;
    project.timeline.tracks[0].kind = TrackKind::Audio;
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidClipComposition { .. })
    ));
}

#[test]
fn validation_rejects_negative_ranges_and_uncovered_overlaps() {
    let mut negative = project();
    negative.timeline.tracks[0].clips[0].source_range =
        TimeRange::new(MediaTime(-1), MediaTime(1)).expect("ordered range");
    assert!(matches!(
        negative.validate(),
        Err(crate::ModelError::NegativeClipRange { .. })
    ));

    let mut overlapping = project();
    let mut second = overlapping.timeline.tracks[0].clips[0].clone();
    second.id = id(ClipId::new, "clip-b");
    second.timeline_range =
        TimeRange::new(MediaTime(60_000), MediaTime(180_000)).expect("ordered range");
    overlapping.timeline.tracks[0].clips.push(second);
    assert!(matches!(
        overlapping.validate(),
        Err(crate::ModelError::OverlappingClips { .. })
    ));
}

#[test]
fn direct_deserialization_enforces_project_and_id_invariants() {
    assert!(serde_json::from_str::<ProjectId>(r#""""#).is_err());

    let mut value = serde_json::to_value(project()).expect("serialize project");
    value["settings"]["width"] = serde_json::Value::from(0);
    assert!(serde_json::from_value::<Project>(value).is_err());
}

#[test]
fn documents_from_another_schema_version_are_rejected() {
    let mut value = serde_json::to_value(project()).expect("serialize project");
    value["schema_version"] = serde_json::Value::from(crate::CURRENT_SCHEMA_VERSION + 1);
    let json = serde_json::to_string(&value).expect("serialize value");
    assert!(matches!(
        Project::from_json(&json),
        Err(crate::ProjectLoadError::InvalidProject(
            crate::ModelError::UnsupportedSchemaVersion { .. }
        ))
    ));
}

#[test]
fn current_documents_round_trip_through_json() {
    let original = project();
    let json = serde_json::to_string(&original).expect("serialize project");
    let parsed = Project::from_json(&json).expect("load current document");
    assert_eq!(parsed, original);
}

#[test]
fn missing_color_metadata_defaults_to_bt709_limited_and_hdr_is_reported() {
    let mut project = project();
    let fallback = project.sources[0].effective_color_metadata();
    assert_eq!(fallback.primaries, alpha2_media::ColorPrimaries::Bt709);
    assert_eq!(fallback.matrix, alpha2_media::ColorMatrix::Bt709);
    assert_eq!(fallback.range, alpha2_media::ColorRange::Limited);

    project.sources[0].color_metadata = Some(alpha2_media::ColorMetadata {
        primaries: alpha2_media::ColorPrimaries::Bt2020,
        transfer: alpha2_media::TransferFunction::Pq,
        matrix: alpha2_media::ColorMatrix::Bt2020Ncl,
        range: alpha2_media::ColorRange::Limited,
        dynamic_range: alpha2_media::DynamicRange::Hdr,
    });
    assert_eq!(project.hdr_sources(), vec![&project.sources[0].id]);
    // Tone-mapped by default, it is nothing to warn about; a project that refuses HDR is told.
    let issue = crate::LintIssue::HdrSource {
        source: project.sources[0].id.clone(),
    };
    assert!(!project.lint().contains(&issue));
    project.settings.color.hdr = crate::HdrSources::Reject;
    assert!(project.lint().contains(&issue));
}

#[test]
fn a_color_is_hdr_exactly_when_its_transfer_is_pq_or_hlg() {
    use alpha2_media::{
        ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, TransferFunction,
    };
    // Lint, conform, and the renderer would each have to pick one of the two fields; a document
    // that makes them disagree is refused instead.
    let color = |transfer, dynamic_range| ColorMetadata {
        primaries: ColorPrimaries::Bt2020,
        transfer,
        matrix: ColorMatrix::Bt2020Ncl,
        range: ColorRange::Limited,
        dynamic_range,
    };
    let source = project().sources[0].id.clone();
    for transfer in [
        TransferFunction::Bt709,
        TransferFunction::Srgb,
        TransferFunction::Pq,
        TransferFunction::Hlg,
    ] {
        let hdr = matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg);
        for dynamic_range in [DynamicRange::Sdr, DynamicRange::Hdr] {
            let consistent = hdr == (dynamic_range == DynamicRange::Hdr);
            let mut declared = project();
            declared.sources[0].color_metadata = Some(color(transfer, dynamic_range));
            let mut recorded = project();
            recorded.sources[0].fingerprint = Some(crate::MediaFingerprint {
                color: Some(color(transfer, dynamic_range)),
                ..crate::MediaFingerprint::default()
            });
            if consistent {
                declared.validate().expect("a consistent declaration");
                recorded.validate().expect("a consistent fingerprint");
            } else {
                assert_eq!(
                    declared.validate().map_err(|error| error.to_string()),
                    Err(format!(
                        "source {source} declares a dynamic range its transfer contradicts"
                    )),
                    "{transfer:?} {dynamic_range:?}"
                );
                assert_eq!(
                    recorded.validate().map_err(|error| error.to_string()),
                    Err(format!("source {source} has an invalid fingerprint")),
                    "{transfer:?} {dynamic_range:?}"
                );
            }
        }
    }
}

#[test]
fn the_color_pipeline_defaults_to_tone_mapping_from_1000_nits_and_bounds_its_peak() {
    let mut project = project();
    assert_eq!(project.settings.color, crate::ColorPipeline::default());
    assert_eq!(project.settings.color.hdr, crate::HdrSources::ToneMap);
    assert_eq!(project.settings.color.hdr_peak_nits, 1_000);
    let json = serde_json::to_value(&project).expect("serializes");
    assert_eq!(json["settings"]["color"]["hdr"], "tone_map");
    for peak in [99, 10_001] {
        project.settings.color.hdr_peak_nits = peak;
        assert_eq!(
            project.validate(),
            Err(crate::ModelError::InvalidColorPipeline)
        );
    }
    project.settings.color.hdr_peak_nits = 4_000;
    project.validate().expect("a 4000-nit peak");
}
