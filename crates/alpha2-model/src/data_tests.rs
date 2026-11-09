use crate::{BindingProblem, DataProblem, ModelError, Project};
use serde_json::{Value, json};

/// The shared test project as JSON, with an audio track, a composition shown by the clip and a
/// shapes source, so every clock has a place.
fn base() -> Value {
    let mut project = serde_json::to_value(crate::tests::project()).expect("serializes");
    project["sources"].as_array_mut().expect("sources").extend([
        json!({ "id": "music", "locator": "asset://music", "kind": "audio" }),
        json!({ "id": "other", "locator": "asset://other", "kind": "video" }),
        json!({ "id": "shapes", "locator": "", "kind": "generated",
                "generator": { "type": "shapes", "items": [
                    { "type": "rectangle", "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
                      "size": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } } },
                    { "type": "fill", "paint": { "mode": "constant", "value": { "type": "solid", "color": "#ff0000" } } }
                ] } }),
    ]);
    project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .push(json!({
            "id": "track-music", "name": "Music", "kind": "audio",
            "clips": [{ "id": "clip-music", "source_id": "music",
                        "timeline_range": { "start": 0, "end": 120_000 },
                        "source_range": { "start": 0, "end": 120_000 } }]
        }));
    project["compositions"] = json!([{
        "id": "comp",
        "nodes": [
            { "id": "clip", "op": { "type": "clip" } },
            { "id": "media", "op": { "type": "media", "source_id": "source-a" } },
            { "id": "blur", "op": { "type": "effect", "input": "clip",
                "effect": { "id": "alpha2.effect.gaussian-blur", "version": 1, "params": {} } } },
            { "id": "move", "op": { "type": "transform", "input": "blur" } }
        ],
        "output": "move"
    }]);
    project["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({ "id": "comp" });
    project
}

fn points(id: &str, anchor: &Value) -> Value {
    json!({ "id": id, "anchor": anchor, "kind": "points", "interpolation": "hold",
            "channels": ["nose", "chin"], "times": [0, 4004],
            "values": [0.4, 0.3, 0.5, 0.6, null, null, 0.5, 0.7] })
}

fn scalars(id: &str, anchor: &Value) -> Value {
    json!({ "id": id, "anchor": anchor, "kind": "scalars", "interpolation": "linear",
            "channels": ["rms"], "times": [0, 4004, 8008], "values": [0.1, null, 0.3] })
}

fn source(id: &str) -> Value {
    json!({ "type": "source", "source_id": id })
}

fn timeline() -> Value {
    json!({ "type": "timeline" })
}

fn load(project: &Value) -> Result<Project, String> {
    Project::from_json(&project.to_string()).map_err(|error| error.to_string())
}

fn model_error(project: &Value) -> ModelError {
    match Project::from_json(&project.to_string()) {
        Err(crate::ProjectLoadError::InvalidProject(error)) => error,
        other => panic!("expected a model error, got {other:?}"),
    }
}

fn with_data(sets: Vec<Value>) -> Value {
    let mut project = base();
    project["data"] = Value::Array(sets);
    project
}

fn data_problem(project: &Value) -> DataProblem {
    match model_error(project) {
        ModelError::InvalidDataSet { problem, .. } => problem,
        other => panic!("expected a data set error, got {other}"),
    }
}

fn binding_problem(project: &Value) -> (String, BindingProblem) {
    match model_error(project) {
        ModelError::InvalidDataBinding { at, problem, .. } => (at, problem),
        other => panic!("expected a binding error, got {other}"),
    }
}

#[test]
fn every_kind_round_trips() {
    let project = with_data(vec![
        points("face", &source("source-a")),
        scalars("loud", &source("music")),
        json!({ "id": "shake", "anchor": source("source-a"), "kind": "similarity",
                "interpolation": "hold", "times": [0, 4004],
                "values": [0.01, -0.02, 1.5, 1.0, null, null, null, null] }),
        json!({ "id": "words", "anchor": source("music"), "kind": "words", "words": [
            { "start": 0, "end": 6000, "text": "hello", "speaker": "A" },
            { "start": 6000, "end": 9000, "text": "there" } ] }),
        json!({ "id": "silence", "anchor": timeline(), "kind": "intervals", "intervals": [
            { "start": 0, "end": 0, "label": "beat" },
            { "start": 1000, "end": 9000, "score": 0.25 } ],
            "made_by": { "extractor": "com.example.vad", "version": 2, "params": { "k": 1 },
                         "plugin": { "version": "1.0.0", "artifact_sha256": "a".repeat(64) } } }),
    ]);
    let loaded = load(&project).expect("valid");
    let again = load(&serde_json::to_value(&loaded).expect("serializes")).expect("valid");
    assert_eq!(loaded, again);
    assert_eq!(loaded.data.len(), 5);
}

#[test]
fn a_kind_refuses_fields_of_another_kind() {
    let mut set = points("face", &source("source-a"));
    set["words"] = json!([]);
    assert!(
        load(&with_data(vec![set]))
            .unwrap_err()
            .contains("no field `words`")
    );
    let mut set = scalars("loud", &source("music"));
    set.as_object_mut().expect("object").remove("channels");
    assert!(
        load(&with_data(vec![set]))
            .unwrap_err()
            .contains("needs `channels`")
    );
    let mut set = json!({ "id": "s", "anchor": source("source-a"), "kind": "similarity",
                          "interpolation": "hold", "times": [], "values": [] });
    set["channels"] = json!(["x"]);
    assert!(
        load(&with_data(vec![set]))
            .unwrap_err()
            .contains("no field `channels`")
    );
}

#[test]
fn ids_must_be_unique_and_short() {
    let project = with_data(vec![
        points("face", &source("source-a")),
        scalars("face", &source("music")),
    ]);
    assert!(matches!(
        model_error(&project),
        ModelError::DuplicateDataSetId(_)
    ));
    let long = "x".repeat(crate::MAX_DATA_ID_BYTES + 1);
    assert_eq!(
        data_problem(&with_data(vec![points(&long, &source("source-a"))])),
        DataProblem::IdTooLong
    );
}

#[test]
fn too_many_sets_are_refused() {
    let sets = (0..=crate::MAX_DATA_SETS)
        .map(|index| scalars(&format!("s{index}"), &timeline()))
        .collect();
    assert!(matches!(
        model_error(&with_data(sets)),
        ModelError::TooManyDataSets { .. }
    ));
}

#[test]
fn anchors_need_a_media_source() {
    assert!(matches!(
        data_problem(&with_data(vec![points("p", &source("missing"))])),
        DataProblem::UnknownSource(_)
    ));
    assert!(matches!(
        data_problem(&with_data(vec![points("p", &source("shapes"))])),
        DataProblem::SourceWithoutMedia(_)
    ));
}

#[test]
fn channels_are_counted_and_named() {
    let mut set = scalars("s", &timeline());
    set["channels"] = json!([]);
    set["values"] = json!([]);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::ChannelCount
    );
    let mut set = points("p", &timeline());
    set["channels"] = json!(["a", "a"]);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::ChannelName
    );
    let mut set = points("p", &timeline());
    set["channels"] = json!(["", "b"]);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::ChannelName
    );
    let too_many = (0..=crate::MAX_DATA_CHANNELS)
        .map(|index| format!("c{index}"))
        .collect::<Vec<_>>();
    let mut set = scalars("s", &timeline());
    set["values"] = json!(vec![0.0; too_many.len() * 3]);
    set["channels"] = json!(too_many);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::ChannelCount
    );
}

#[test]
fn values_match_times_channels_and_arity() {
    let mut set = points("p", &timeline());
    set["values"].as_array_mut().expect("values").pop();
    assert_eq!(data_problem(&with_data(vec![set])), DataProblem::ValueCount);
}

#[test]
fn times_ascend_from_zero_within_the_source() {
    let mut set = scalars("s", &timeline());
    set["times"] = json!([0, 0, 1]);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::TimesNotAscending
    );
    let mut set = scalars("s", &timeline());
    set["times"] = json!([-1, 0, 1]);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::NegativeTime
    );
    let mut project = with_data(vec![scalars("s", &source("music"))]);
    project["sources"][1]["fingerprint"] = json!({ "duration_ticks": 5000 });
    assert_eq!(data_problem(&project), DataProblem::PastSourceEnd);
}

#[test]
fn values_stay_inside_their_bounds() {
    let mut set = points("p", &timeline());
    set["values"][1] = json!(16.5);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidValue { index: 1 }
    );
    let mut set = scalars("s", &timeline());
    set["values"][2] = json!(2e9);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidValue { index: 2 }
    );
    let similarity = |values: Value| {
        json!({ "id": "m", "anchor": timeline(), "kind": "similarity", "interpolation": "hold",
                "times": [0], "values": values })
    };
    for (values, index) in [
        (json!([0, 0, 40000, 1]), 2),
        (json!([0, 0, 0, 0]), 3),
        (json!([0, 0, 0, 1001]), 3),
        (json!([17, 0, 0, 1]), 0),
    ] {
        assert_eq!(
            data_problem(&with_data(vec![similarity(values)])),
            DataProblem::InvalidValue { index }
        );
    }
}

#[test]
fn a_lost_marker_covers_a_whole_channel() {
    let mut set = points("p", &timeline());
    set["values"][4] = json!(0.1);
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidValue { index: 5 }
    );
    let set = json!({ "id": "m", "anchor": timeline(), "kind": "similarity",
                      "interpolation": "hold", "times": [0], "values": [null, 0, 0, 1] });
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidValue { index: 1 }
    );
}

#[test]
fn words_and_intervals_are_ordered_and_bounded() {
    let words =
        |items: Value| json!({ "id": "w", "anchor": timeline(), "kind": "words", "words": items });
    for items in [
        json!([{ "start": 10, "end": 10, "text": "a" }]),
        json!([{ "start": 0, "end": 10, "text": "" }]),
        json!([{ "start": 0, "end": 10, "text": "x".repeat(crate::MAX_DATA_LABEL_BYTES + 1) }]),
        json!([{ "start": 5, "end": 10, "text": "a" }, { "start": 0, "end": 10, "text": "b" }]),
    ] {
        assert!(matches!(
            data_problem(&with_data(vec![words(items)])),
            DataProblem::InvalidItem { .. }
        ));
    }
    let intervals = |items: Value| json!({ "id": "i", "anchor": timeline(), "kind": "intervals", "intervals": items });
    for items in [
        json!([{ "start": 10, "end": 9 }]),
        json!([{ "start": 0, "end": 9, "score": 1e10 }]),
        json!([{ "start": 0, "end": 9, "label": "x".repeat(crate::MAX_DATA_LABEL_BYTES + 1) }]),
        json!([{ "start": 0, "end": 9 }, { "start": 0, "end": 8 }]),
    ] {
        assert!(matches!(
            data_problem(&with_data(vec![intervals(items)])),
            DataProblem::InvalidItem { .. }
        ));
    }
    let mut overlapping = intervals(
        json!([{ "start": 0, "end": 9 }, { "start": 0, "end": 12 }, { "start": 3, "end": 4 }]),
    );
    overlapping["intervals"][2]["label"] = json!("inside");
    load(&with_data(vec![overlapping])).expect("overlaps are allowed");
}

#[test]
fn provenance_is_well_formed() {
    let mut set = points("p", &source("source-a"));
    set["made_by"] = json!({ "extractor": "", "version": 1 });
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidMadeBy
    );
    let mut set = points("p", &source("source-a"));
    set["made_by"] = json!({ "extractor": "a.b", "version": 1, "plugin": { "version": "1", "artifact_sha256": "xyz" } });
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidMadeBy
    );
    let mut set = points("p", &source("source-a"));
    set["made_from"] = json!({ "content_sha256": "nothex" });
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidMadeFrom("content_sha256")
    );
    let mut set = points("p", &timeline());
    set["made_from"] = json!({ "duration_ticks": 5 });
    assert_eq!(
        data_problem(&with_data(vec![set])),
        DataProblem::InvalidMadeFrom("anchor")
    );
}

fn bind(project: &mut Value, pointer: &str, binding: Value) {
    *project
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("no {pointer}")) = binding;
}

const CLIP: &str = "/timeline/tracks/0/clips/0";

#[test]
fn clip_values_follow_source_data_through_a_clip() {
    let mut project = with_data(vec![
        points("face", &source("source-a")),
        scalars("loud", &source("music")),
    ]);
    let face = json!({ "mode": "data", "data": "face", "channel": "nose",
                       "via": { "clip": "clip-a" }, "space": "picture", "offset": [0, -0.1] });
    let loud = json!({ "mode": "data", "data": "loud", "channel": "rms", "via": { "clip": "clip-music" },
                       "remap": { "input": [0, 0.3], "output": [0.4, 1] } });
    project["timeline"]["tracks"][0]["clips"][0]["masks"] = json!([{
        "shape": { "type": "ellipse", "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
                   "size": { "mode": "constant", "value": { "x": 0.2, "y": 0.2 } } } }]);
    bind(
        &mut project,
        &format!("{CLIP}/masks/0/shape/center"),
        face.clone(),
    );
    bind(
        &mut project,
        &format!("{CLIP}/visual/opacity"),
        loud.clone(),
    );
    project["timeline"]["tracks"][0]["clips"][0]["visual"]["scale"] = loud.clone();
    project["timeline"]["tracks"][1]["clips"][0]["audio"] =
        json!({ "gain": loud.clone(), "pan": loud.clone() });
    project["timeline"]["tracks"][1]["audio"] = json!({ "gain": loud.clone() });
    project["audio"] = json!({ "gain": loud });
    load(&project).expect("valid bindings");
}

#[test]
fn timeline_data_binds_without_via() {
    let mut project = with_data(vec![points("p", &timeline())]);
    bind(
        &mut project,
        &format!("{CLIP}/visual/position"),
        json!({ "mode": "data", "data": "p", "channel": "nose", "offset": [0.1, 0] }),
    );
    load(&project).expect("valid");
    bind(
        &mut project,
        &format!("{CLIP}/visual/position"),
        json!({ "mode": "data", "data": "p", "channel": "nose", "via": { "clip": "clip-a" }, "space": "canvas" }),
    );
    assert_eq!(binding_problem(&project).1, BindingProblem::ViaForbidden);
    bind(
        &mut project,
        &format!("{CLIP}/visual/position"),
        json!({ "mode": "data", "data": "p", "channel": "nose", "space": "canvas" }),
    );
    assert_eq!(
        binding_problem(&project).1,
        BindingProblem::FieldNotApplicable("space")
    );
}

#[test]
fn bindings_refuse_what_does_not_fit() {
    let cases = [
        (
            json!({ "mode": "data", "data": "nope", "channel": "nose" }),
            "/visual/position",
            BindingProblem::UnknownData,
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "clip-a" }, "space": "picture" }),
            "/visual/opacity",
            BindingProblem::WrongKind(crate::DataKind::Points),
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "ear", "via": { "clip": "clip-a" }, "space": "picture" }),
            "/visual/position",
            BindingProblem::UnknownChannel,
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose" }),
            "/visual/position",
            BindingProblem::ViaRequired,
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "clip-a" } }),
            "/visual/position",
            BindingProblem::SpaceRequired,
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "ghost" }, "space": "picture" }),
            "/visual/position",
            BindingProblem::UnknownViaClip,
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose", "via": { "node": "media" }, "space": "picture" }),
            "/visual/position",
            BindingProblem::ViaWrongReader,
        ),
        (
            json!({ "mode": "data", "data": "loud", "channel": "rms", "via": { "clip": "clip-a" } }),
            "/visual/opacity",
            BindingProblem::ViaShowsOtherSource,
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "clip-a" }, "space": "canvas" }),
            "/visual/position",
            BindingProblem::Cycle,
        ),
        (
            json!({ "mode": "data", "data": "loud", "channel": "rms", "via": { "clip": "clip-music" }, "space": "picture" }),
            "/visual/opacity",
            BindingProblem::FieldNotApplicable("space"),
        ),
        (
            json!({ "mode": "data", "data": "loud", "channel": "rms", "via": { "clip": "clip-music" }, "offset": [0, 0] }),
            "/visual/opacity",
            BindingProblem::FieldNotApplicable("offset"),
        ),
        (
            json!({ "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "clip-a" }, "space": "picture",
                 "remap": { "input": [0, 1], "output": [0, 1] } }),
            "/visual/position",
            BindingProblem::FieldNotApplicable("remap"),
        ),
        (
            json!({ "mode": "data", "data": "loud", "channel": "rms", "via": { "clip": "clip-music" },
                 "remap": { "input": [1, 1], "output": [0, 1] } }),
            "/visual/opacity",
            BindingProblem::InvalidRemap,
        ),
    ];
    for (binding, at, problem) in cases {
        let mut project = with_data(vec![
            points("face", &source("source-a")),
            scalars("loud", &source("music")),
        ]);
        bind(&mut project, &format!("{CLIP}{at}"), binding);
        assert_eq!(binding_problem(&project).1, problem, "at {at}");
    }
}

#[test]
fn canvas_through_an_audio_clip_is_refused() {
    let mut project = with_data(vec![points("p", &source("music"))]);
    bind(
        &mut project,
        &format!("{CLIP}/visual/position"),
        json!({ "mode": "data", "data": "p", "channel": "nose", "via": { "clip": "clip-music" }, "space": "canvas" }),
    );
    assert_eq!(binding_problem(&project).1, BindingProblem::ViaWrongReader);
}

#[test]
fn canvas_follows_must_be_acyclic() {
    let mut project = with_data(vec![
        points("a", &source("source-a")),
        points("b", &source("other")),
    ]);
    project["timeline"]["tracks"][0]["clips"][0]["composition"] = Value::Null;
    let mut second = project["timeline"]["tracks"][0]["clips"][0].clone();
    second["id"] = json!("clip-b");
    second["source_id"] = json!("other");
    project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .push(json!({
        "id": "track-b", "name": "B", "kind": "video", "clips": [second] }));
    let follow = |data: &str, clip: &str| json!({ "mode": "data", "data": data, "channel": "nose", "via": { "clip": clip }, "space": "canvas" });
    bind(
        &mut project,
        "/timeline/tracks/2/clips/0/visual/position",
        follow("a", "clip-a"),
    );
    load(&project).expect("one follow is fine");
    // Picture space on its own clip is not a dependency on its visual.
    project["timeline"]["tracks"][0]["clips"][0]["masks"] = json!([{
        "shape": { "type": "ellipse", "size": { "mode": "constant", "value": { "x": 0.2, "y": 0.2 } },
                   "center": { "mode": "data", "data": "a", "channel": "nose", "via": { "clip": "clip-a" }, "space": "picture" } } }]);
    load(&project).expect("own picture space is fine");
    bind(
        &mut project,
        &format!("{CLIP}/visual/position"),
        follow("b", "clip-b"),
    );
    assert_eq!(binding_problem(&project).1, BindingProblem::Cycle);
}

#[test]
fn composition_values_follow_through_a_media_node() {
    let mut project = with_data(vec![
        points("face", &source("source-a")),
        scalars("loud", &source("source-a")),
    ]);
    let nodes = "/compositions/0/nodes";
    bind(
        &mut project,
        &format!("{nodes}/2/op/effect/params"),
        json!({ "radius": { "mode": "data", "data": "loud", "channel": "rms", "via": { "node": "media" } } }),
    );
    project
        .pointer_mut(&format!("{nodes}/3/op"))
        .expect("transform")["transform"] = json!({
        "position": { "mode": "data", "data": "face", "channel": "nose", "via": { "node": "media" }, "space": "picture" } });
    load(&project).expect("valid");

    let mut canvas = project.clone();
    canvas
        .pointer_mut(&format!("{nodes}/3/op/transform/position"))
        .expect("position")["space"] = json!("canvas");
    assert_eq!(
        binding_problem(&canvas).1,
        BindingProblem::FieldNotApplicable("space")
    );

    let mut not_media = project.clone();
    not_media
        .pointer_mut(&format!("{nodes}/3/op/transform/position"))
        .expect("position")["via"] = json!({ "node": "blur" });
    assert_eq!(
        binding_problem(&not_media).1,
        BindingProblem::InvalidViaNode
    );

    let mut by_clip = project.clone();
    by_clip
        .pointer_mut(&format!("{nodes}/3/op/transform/position"))
        .expect("position")["via"] = json!({ "clip": "clip-a" });
    assert_eq!(binding_problem(&by_clip).1, BindingProblem::ViaWrongReader);

    let mut timeline_data = project;
    timeline_data["data"]
        .as_array_mut()
        .expect("data")
        .push(scalars("t", &timeline()));
    bind(
        &mut timeline_data,
        &format!("{nodes}/2/op/effect/params"),
        json!({ "radius": { "mode": "data", "data": "t", "channel": "rms" } }),
    );
    assert_eq!(
        binding_problem(&timeline_data).1,
        BindingProblem::ClockRefusesAnchor
    );
}

#[test]
fn effect_params_take_data_only_when_float_or_vec2() {
    let mut project = with_data(vec![
        points("face", &source("source-a")),
        scalars("loud", &source("source-a")),
    ]);
    let effect = "/compositions/0/nodes/2/op/effect";
    *project.pointer_mut(effect).expect("effect") = json!({ "id": "alpha2.effect.corner-pin", "version": 1, "params": {
        "top_left": { "mode": "data", "data": "face", "channel": "nose", "via": { "node": "media" }, "space": "picture" } } });
    load(&project).expect("corner-pin follows a point");
    let mut wrong = project.clone();
    *wrong.pointer_mut(effect).expect("effect") = json!({ "id": "alpha2.effect.gaussian-blur", "version": 1, "params": {
        "radius": { "mode": "data", "data": "face", "channel": "nose", "via": { "node": "media" }, "space": "picture" } } });
    assert_eq!(
        binding_problem(&wrong).1,
        BindingProblem::WrongKind(crate::DataKind::Points)
    );
}

#[test]
fn picture_time_places_refuse_data() {
    let mut project = with_data(vec![scalars("t", &timeline())]);
    bind(
        &mut project,
        "/sources/3/generator/items/0/size",
        json!({ "mode": "data", "data": "t", "channel": "rms" }),
    );
    let (at, problem) = binding_problem(&project);
    assert_eq!(
        (at.as_str(), problem),
        ("items[0].size", BindingProblem::ClockRefusesAnchor)
    );
}

#[test]
fn a_correction_needs_similarity_on_the_clips_source() {
    let similarity = |anchor: Value| {
        json!({ "id": "shake", "anchor": anchor, "kind": "similarity",
        "interpolation": "hold", "times": [0], "values": [0, 0, 0, 1] })
    };
    let mut project = with_data(vec![
        similarity(source("source-a")),
        points("face", &source("source-a")),
    ]);
    project["timeline"]["tracks"][0]["clips"][0]["visual"]["correction"] =
        json!({ "data": "shake" });
    load(&project).expect("valid");
    project["data"][0] = similarity(source("other"));
    assert_eq!(binding_problem(&project).1, BindingProblem::NotOwnSource);
    project["timeline"]["tracks"][0]["clips"][0]["visual"]["correction"] =
        json!({ "data": "face" });
    assert_eq!(
        binding_problem(&project).1,
        BindingProblem::WrongKind(crate::DataKind::Points)
    );
    let mut node = with_data(vec![similarity(source("source-a"))]);
    node["compositions"][0]["nodes"][3]["op"]["transform"] =
        json!({ "correction": { "data": "shake" } });
    assert_eq!(
        binding_problem(&node).1,
        BindingProblem::FieldNotApplicable("correction")
    );
}

#[test]
fn caption_tracks_take_cues_or_words() {
    let words = json!({ "id": "words", "anchor": source("music"), "kind": "words",
                        "words": [{ "start": 0, "end": 6000, "text": "hi" }] });
    let mut project = with_data(vec![words]);
    project["timeline"]["tracks"].as_array_mut().expect("tracks").push(json!({
        "id": "subs", "name": "Subs", "kind": "caption",
        "captions": { "from_words": { "data": "words", "track": "track-music", "max_chars": 42 } } }));
    load(&project).expect("valid");
    let track = "/timeline/tracks/2/captions";
    let mut both = project.clone();
    both.pointer_mut(track).expect("captions")["cues"] =
        json!([{ "timeline_range": { "start": 0, "end": 10 }, "text": "x" }]);
    assert!(matches!(
        model_error(&both),
        ModelError::InvalidCaptions { .. }
    ));
    for words in [
        json!({ "data": "words", "max_chars": 42 }),
        json!({ "data": "words", "track": "subs", "max_chars": 42 }),
        json!({ "data": "words", "track": "track-music", "max_chars": 0 }),
    ] {
        let mut project = project.clone();
        project.pointer_mut(track).expect("captions")["from_words"] = words;
        assert_eq!(
            binding_problem(&project).1,
            BindingProblem::InvalidCaptionTrack
        );
    }
}

#[test]
fn data_mode_on_other_types_fails_to_deserialize() {
    let mut project = with_data(vec![scalars("t", &timeline())]);
    bind(
        &mut project,
        "/sources/3/generator/items/1/paint",
        json!({ "mode": "data", "data": "t", "channel": "rms" }),
    );
    assert!(load(&project).unwrap_err().contains("takes no `data` mode"));
    let text = json!({ "mode": "data", "data": "t", "channel": "rms" });
    assert!(serde_json::from_value::<crate::Animated<String>>(text.clone()).is_err());
    assert!(serde_json::from_value::<crate::Animated<crate::LayoutLength>>(text.clone()).is_err());
    assert!(serde_json::from_value::<crate::Animated<f32>>(text).is_ok());
}

#[test]
fn stale_data_is_reported_by_the_hosts_comparison() {
    let mut set = points("face", &source("source-a"));
    set["made_from"] = json!({ "byte_length": 10 });
    let project = load(&with_data(vec![set, points("t", &timeline())])).expect("valid");
    let issues = project.stale_data(|_, made_from| made_from.byte_length != Some(11));
    assert_eq!(issues.len(), 1);
    assert!(
        matches!(&issues[0], crate::LintIssue::StaleData { data, .. } if data.as_str() == "face")
    );
    assert_eq!(project.stale_data(|_, _| false), Vec::new());
}

#[cfg(feature = "schema")]
mod schema {
    use serde_json::Value;

    /// Every place whose schema holds a data-accepting value, as `Def.property` (enum variants as
    /// `Def[tag].property`), with the clock it is on. "holder" means the clock depends on what
    /// holds the struct (a clip, a composition node or a generator tree).
    const CLASSIFIED: &[(&str, &str)] = &[
        ("AudioProperties.gain", "timeline"),
        ("AudioProperties.pan", "timeline"),
        ("TrackAudio.gain", "timeline"),
        ("TrackAudio.pan", "timeline"),
        ("MasterAudio.gain", "timeline"),
        (
            "VisualProperties.position",
            "holder: clip timeline, transform node composition",
        ),
        (
            "VisualProperties.scale",
            "holder: clip timeline, transform node composition",
        ),
        (
            "VisualProperties.rotation_degrees",
            "holder: clip timeline, transform node composition",
        ),
        (
            "VisualProperties.opacity",
            "holder: clip timeline, transform node composition",
        ),
        (
            "ClipEffect.params",
            "holder: transition timeline, effect node composition",
        ),
        (
            "ShapeMask.feather",
            "holder: clip timeline, mask node composition",
        ),
        ("MergeLayer.opacity", "composition"),
        ("NodeOp[transition].progress", "composition"),
        ("NodeOp[group].controls", "composition"),
        (
            "ShapeGeometry[rectangle].center",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[rectangle].center", "picture"),
        (
            "ShapeGeometry[rectangle].size",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[rectangle].size", "picture"),
        (
            "ShapeGeometry[rectangle].corner_radius",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[rectangle].corner_radius", "picture"),
        (
            "ShapeGeometry[rectangle].rotation_degrees",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[rectangle].rotation_degrees", "picture"),
        (
            "ShapeGeometry[ellipse].center",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[ellipse].center", "picture"),
        (
            "ShapeGeometry[ellipse].size",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[ellipse].size", "picture"),
        (
            "ShapeGeometry[ellipse].rotation_degrees",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[ellipse].rotation_degrees", "picture"),
        (
            "ShapeGeometry[polystar].center",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].center", "picture"),
        (
            "ShapeGeometry[polystar].points",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].points", "picture"),
        (
            "ShapeGeometry[polystar].outer_radius",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].outer_radius", "picture"),
        (
            "ShapeGeometry[polystar].inner_radius",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].inner_radius", "picture"),
        (
            "ShapeGeometry[polystar].outer_roundness",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].outer_roundness", "picture"),
        (
            "ShapeGeometry[polystar].inner_roundness",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].inner_roundness", "picture"),
        (
            "ShapeGeometry[polystar].rotation_degrees",
            "holder: masks timeline or composition, or picture in generators",
        ),
        ("ShapeItem[polystar].rotation_degrees", "picture"),
        ("ShapeTransform.anchor", "picture"),
        ("ShapeTransform.position", "picture"),
        ("ShapeTransform.scale", "picture"),
        ("ShapeTransform.rotation_degrees", "picture"),
        ("ShapeTransform.opacity", "picture"),
        ("ShapeItem[fill].opacity", "picture"),
        ("ShapeItem[stroke].width", "picture"),
        ("ShapeItem[stroke].opacity", "picture"),
        ("Dash.pattern", "picture"),
        ("Dash.offset", "picture"),
        ("ShapeItem[trim].start", "picture"),
        ("ShapeItem[trim].end", "picture"),
        ("ShapeItem[trim].offset", "picture"),
        ("TextAnimatorProperties.offset", "picture: text time"),
        ("TextAnimatorProperties.scale", "picture: text time"),
        (
            "TextAnimatorProperties.rotation_degrees",
            "picture: text time",
        ),
        ("TextAnimatorProperties.opacity", "picture: text time"),
        ("TextAnimatorProperties.tracking", "picture: text time"),
        ("TextSelector[range].start", "picture: text time"),
        ("TextSelector[range].end", "picture: text time"),
        ("TextSelector[range].offset", "picture: text time"),
        ("TextSelector[range].amount", "picture: text time"),
        ("LayoutNode.flex_grow", "picture"),
        ("LayoutNode.flex_shrink", "picture"),
        ("LayoutNode.opacity", "picture"),
        ("LayoutNode.scale", "picture"),
        ("LayoutNode.rotation_degrees", "picture"),
    ];

    const ACCEPTING: &[&str] = &[
        "#/$defs/Animated_float",
        "#/$defs/Animated_Vec2",
        "#/$defs/Animated_EffectValue",
        "#/$defs/AnimatedPoint",
    ];

    fn refs_accepting(schema: &Value) -> bool {
        match schema {
            Value::Object(map) => {
                if map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|target| ACCEPTING.contains(&target))
                {
                    return true;
                }
                ["anyOf", "oneOf", "allOf"]
                    .iter()
                    .filter_map(|key| map.get(*key).and_then(Value::as_array))
                    .flatten()
                    .chain(
                        ["items", "additionalProperties"]
                            .iter()
                            .filter_map(|key| map.get(*key)),
                    )
                    .any(refs_accepting)
            }
            _ => false,
        }
    }

    fn places(name: &str, schema: &Value, found: &mut Vec<String>) {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (property, value) in properties {
                if refs_accepting(value) {
                    found.push(format!("{name}.{property}"));
                }
            }
        }
        for union in ["oneOf", "anyOf"] {
            for variant in schema
                .get(union)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let tag = variant
                    .get("properties")
                    .and_then(Value::as_object)
                    .and_then(|properties| properties.values().find_map(|value| value.get("const")))
                    .and_then(Value::as_str);
                match tag {
                    Some(tag) => places(&format!("{name}[{tag}]"), variant, found),
                    None => places(name, variant, found),
                }
            }
        }
    }

    #[test]
    fn every_animated_field_is_classified() {
        let schema = crate::project_json_schema();
        let mut found = Vec::new();
        for (name, definition) in schema["$defs"].as_object().expect("definitions") {
            if name.starts_with("Animated") || name.starts_with("Data") {
                continue;
            }
            places(name, definition, &mut found);
        }
        found.sort();
        let mut classified = CLASSIFIED
            .iter()
            .map(|(place, _)| (*place).to_owned())
            .collect::<Vec<_>>();
        classified.sort();
        assert_eq!(
            found, classified,
            "a data-accepting value is not classified by its clock"
        );
    }

    #[test]
    fn only_accepting_types_offer_the_data_mode() {
        let schema = crate::project_json_schema();
        let offers = |name: &str| {
            schema["$defs"][name]["oneOf"]
                .as_array()
                .expect("a union")
                .iter()
                .any(|variant| {
                    variant.pointer("/properties/mode/const") == Some(&Value::from("data"))
                })
        };
        for name in [
            "Animated_float",
            "Animated_Vec2",
            "Animated_EffectValue",
            "AnimatedPoint",
        ] {
            assert!(offers(name), "{name}");
        }
        for name in [
            "Keyframed_string",
            "Keyframed_Paint",
            "Keyframed_Rgba",
            "Keyframed_Path",
            "Keyframed_LayoutLength",
            "Keyframed_LayoutSize",
            "Keyframed_LayoutOffset",
            "Keyframed_Insets",
        ] {
            assert!(!offers(name), "{name}");
        }
    }
}
