use super::tests::{FRAME, corpus, load, state};
use super::*;
use crate::{ActiveIndex, evaluate};
use alpha2_model::Project;
use serde_json::{Value, json};

fn bake_request(targets: &Value) -> EditRequest {
    serde_json::from_value(json!({ "op": "bake", "targets": targets })).expect("request")
}

/// The edit corpus with an out-of-range scalar set bound to an opacity and to audio gains and pans.
fn out_of_range(interpolation: &str) -> Project {
    let mut document = corpus();
    document["data"].as_array_mut().expect("data").push(json!({
        "id": "big", "anchor": { "type": "timeline" }, "kind": "scalars", "interpolation": interpolation,
        "channels": ["x"], "times": [0, 8 * FRAME, 16 * FRAME, 24 * FRAME, 40 * FRAME],
        "values": [0.5, 2.0, -1.5, 30.0, 0.25] }));
    let bound = json!({ "mode": "data", "data": "big", "channel": "x" });
    document["timeline"]["tracks"][6]["clips"][0]["visual"] = json!({ "opacity": bound });
    document["timeline"]["tracks"][7]["clips"][0]["audio"] = json!({ "gain": bound, "pan": bound });
    document["timeline"]["tracks"][7]["audio"] = json!({ "gain": bound, "pan": bound });
    load(&document)
}

#[test]
fn baking_out_of_range_data_clamps_to_each_property_and_evaluates_as_live() {
    for interpolation in ["hold", "linear"] {
        let project = out_of_range(interpolation);
        let targets = json!([
            { "clip": "ta", "property": "visual.opacity" },
            { "clip": "au", "property": "audio.gain" },
            { "clip": "au", "property": "audio.pan" },
            { "track": "t-audio", "property": "audio.gain" },
            { "track": "t-audio", "property": "audio.pan" }
        ]);
        let (edited, _) = apply(&project, &bake_request(&targets)).expect("bake");
        let gains = |project: &Project, tick: i64| {
            let context = crate::SampleContext::NONE;
            let index = crate::DataIndex::new(project);
            let live = crate::SampleContext::timeline(project, &index);
            let clip = &project.timeline.tracks[7].clips[0].audio;
            let track = &project.timeline.tracks[7].audio;
            let sample = |animation: &alpha2_model::Animated<f32>| {
                let context = if matches!(animation, alpha2_model::Animated::Data(_)) {
                    &live
                } else {
                    &context
                };
                crate::sample_scalar(animation, MediaTime(tick), context)
            };
            [
                alpha2_model::Domain::GAIN.clamp(sample(&clip.gain)),
                alpha2_model::Domain::PAN.clamp(sample(&clip.pan)),
                alpha2_model::Domain::GAIN.clamp(sample(&track.gain)),
                alpha2_model::Domain::PAN.clamp(sample(&track.pan)),
            ]
        };
        // Every tick inside the clips, including those between samples and around crossings.
        for tick in (0..40 * FRAME).step_by(97) {
            let (before, after) = (gains(&project, tick), gains(&edited, tick));
            for (live, baked) in before.iter().zip(&after) {
                assert!(
                    (live - baked).abs() < 1e-5,
                    "{interpolation} at {tick}: {before:?} {after:?}"
                );
            }
            if tick < 40 * FRAME {
                let opacity = |project: &Project| {
                    let index = ActiveIndex::build(project).expect("index");
                    let frame = evaluate(project, &index, MediaTime(tick)).expect("frame");
                    frame
                        .visual
                        .iter()
                        .find(|clip| clip.clip_id.as_str() == "ta")
                        .map(|clip| clip.visual.expect("visual").opacity)
                };
                let (live, baked) = (opacity(&project), opacity(&edited));
                assert!(
                    live.zip(baked)
                        .is_none_or(|(live, baked)| (live - baked).abs() < 1e-5),
                    "{interpolation} opacity at {tick}: {live:?} {baked:?}"
                );
            }
        }
    }
}

fn effect_driven(effect: &str, param: &str) -> Project {
    let mut document = corpus();
    document["data"].as_array_mut().expect("data").push(json!({
        "id": "drive", "anchor": { "type": "source", "source_id": "v" }, "kind": "scalars",
        "interpolation": "hold", "channels": ["x"],
        "times": [0, 8 * FRAME, 16 * FRAME], "values": [0.5, 5.0, 0.5] }));
    document["compositions"] = json!([{ "id": "look", "output": "fx", "nodes": [
        { "id": "pic", "op": { "type": "media", "source_id": "v" } },
        { "id": "fx", "op": { "type": "effect", "input": "pic", "effect": {
            "id": effect, "version": 1,
            "params": { param: { "mode": "data", "data": "drive", "channel": "x", "via": { "node": "pic" } } } } } }
    ] }]);
    document["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({ "id": "look" });
    load(&document)
}

#[test]
fn baking_a_built_in_parameter_clamps_to_its_range() {
    let project = effect_driven("alpha2.effect.color-controls", "exposure");
    let targets =
        json!([{ "composition": "look", "node": "fx", "property": "effect.params.exposure" }]);
    let (edited, _) = apply(&project, &bake_request(&targets)).expect("bake");
    let alpha2_model::NodeOp::Effect { effect, .. } = &edited.compositions[0].nodes[1].op else {
        panic!("effect node");
    };
    let alpha2_model::Animated::Keyframes { keyframes } = &effect.params["exposure"] else {
        panic!("keyframes");
    };
    let values = keyframes.iter().map(|key| key.value).collect::<Vec<_>>();
    assert_eq!(
        values,
        [0.5, 3.0, 0.5].map(alpha2_model::EffectValue::Float),
        "the live value is clamped to -3..=3, so the baked one is"
    );
    for frame in 0..30 {
        assert_eq!(
            state(&project, frame * FRAME),
            state(&edited, frame * FRAME),
            "frame {frame}"
        );
    }
}

#[test]
fn baking_a_parameter_of_a_host_effect_is_refused() {
    let project = effect_driven("host.grade", "amount");
    let targets =
        json!([{ "composition": "look", "node": "fx", "property": "effect.params.amount" }]);
    assert!(matches!(
        apply(&project, &bake_request(&targets)),
        Err(EditError::UnboundedParameter(_))
    ));
}

fn source_linear() -> Project {
    let mut document = corpus();
    let second = 30 * FRAME;
    document["data"].as_array_mut().expect("data").push(json!({
        "id": "fade", "anchor": { "type": "source", "source_id": "v" }, "kind": "scalars",
        "interpolation": "linear", "channels": ["x"],
        "times": [0, second, 2 * second, 3 * second], "values": [0.0, 0.2, 0.6, 1.0] }));
    for track in document["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
    {
        let clips = track.get_mut("clips").and_then(Value::as_array_mut);
        for clip in clips.into_iter().flatten() {
            if clip["source_id"] == "v" {
                let id = clip["id"].clone();
                clip["visual"]["opacity"] = json!({ "mode": "data", "data": "fade", "channel": "x", "via": { "clip": id } });
            }
        }
    }
    load(&document)
}

#[test]
fn baking_linear_source_data_keeps_moving_until_the_clip_ends() {
    let project = source_linear();
    let clips = ["fwd", "fast", "slow", "rev", "rep", "pp", "frz", "ta"];
    let targets = clips
        .iter()
        .map(|clip| json!({ "clip": clip, "property": "visual.opacity" }))
        .collect::<Vec<_>>();
    let (edited, _) = apply(&project, &bake_request(&json!(targets))).expect("bake");
    let opacities = |project: &Project, tick: i64| {
        let index = ActiveIndex::build(project).expect("index");
        let frame = evaluate(project, &index, MediaTime(tick)).expect("frame");
        clips.map(|id| {
            frame
                .visual
                .iter()
                .find(|clip| clip.clip_id.as_str() == id)
                .map(|clip| clip.visual.expect("visual").opacity)
        })
    };
    let last = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .map(|clip| clip.timeline_range.end().ticks())
        .max()
        .expect("clips");
    for tick in (0..last).step_by(97).chain([last - 1]) {
        let (live, baked) = (opacities(&project, tick), opacities(&edited, tick));
        for ((id, live), baked) in clips.iter().zip(live).zip(baked) {
            assert!(
                live.zip(baked)
                    .is_none_or(|(live, baked)| (live - baked).abs() < 1e-5),
                "{id} at {tick}: live {live:?}, baked {baked:?}"
            );
        }
    }
}
