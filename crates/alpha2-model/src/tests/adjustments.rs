use super::*;
use crate::{ModelError, ProjectLoadError};

fn adjusted(source_extra: &str, clip_extra: &str, tracks: &str, compositions: &str) -> String {
    format!(
        r#"{{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": {{ "width": 64, "height": 36, "frame_rate": {{ "num": 30, "den": 1 }}, "sample_rate": 48000 }},
          "sources": [
            {{ "id": "video", "kind": "video", "locator": "asset://video" }},
            {{ "id": "adjust", "kind": "adjustment" {source_extra} }}
          ],
          "compositions": [ {compositions} ],
          "timeline": {{ "tracks": [
            {{ "id": "v", "name": "V", "kind": "video", "clips": [
              {{ "id": "picture", "source_id": "video", "timeline_range": {{ "start": 0, "end": 120000 }},
                "source_range": {{ "start": 0, "end": 120000 }} }} ] }},
            {{ "id": "a", "name": "A", "kind": "video", "clips": [
              {{ "id": "adjust", "source_id": "adjust", "timeline_range": {{ "start": 0, "end": 120000 }},
                "source_range": {{ "start": 0, "end": 120000 }} {clip_extra} }} ] }}
            {tracks}
          ] }}
        }}"#
    )
}

fn model_error(json: &str) -> ModelError {
    match Project::from_json(json) {
        Err(ProjectLoadError::InvalidProject(error)) => error,
        other => panic!("refused by validation, not {other:?}"),
    }
}

#[test]
fn an_adjustment_source_names_nothing_a_host_supplies() {
    let project = Project::from_json(&adjusted("", "", "", "")).expect("an adjustment layer");
    let source = &project.sources[1];
    assert_eq!(
        source,
        &MediaSource::adjustment(id(SourceId::new, "adjust"))
    );
    assert!(!source.is_host_supplied());
    assert!(!source.is_audible());
    assert_eq!(
        project
            .used_sources()
            .into_iter()
            .filter(|source| source.is_host_supplied())
            .map(|source| source.id.as_str())
            .collect::<Vec<_>>(),
        ["video"]
    );
    for extra in [
        r#", "locator": "asset://adjust""#,
        r#", "has_audio": true"#,
        r#", "generator": { "type": "fill", "paint": { "type": "solid", "color": { "red": 1, "green": 1, "blue": 1, "alpha": 1 } } }"#,
        r#", "composition": "look""#,
        r#", "color_metadata": { "primaries": "bt709", "transfer": "bt709", "matrix": "bt709", "range": "limited", "dynamic_range": "sdr" }"#,
        r#", "fingerprint": {}"#,
        r#", "provider": { "format": "latex", "latex": "x", "font_size": 64 }"#,
    ] {
        let compositions = r#"{ "id": "look", "nodes": [ { "id": "clip", "op": { "type": "clip" } } ], "output": "clip" }"#;
        match Project::from_json(&adjusted(extra, "", "", compositions)) {
            Err(ProjectLoadError::InvalidProject(error)) => assert_eq!(
                error,
                ModelError::InvalidAdjustmentSource {
                    source: id(SourceId::new, "adjust")
                },
                "{extra}"
            ),
            Err(ProjectLoadError::Json(error)) => panic!("{extra}: not a document: {error}"),
            other => panic!("{extra}: {other:?}"),
        }
    }
}

#[test]
fn adjustment_clips_sit_on_video_tracks_only() {
    let track = |kind: &str, captions: &str| {
        format!(
            r#", {{ "id": "x", "name": "X", "kind": "{kind}", {captions} "clips": [
              {{ "id": "elsewhere", "source_id": "adjust", "timeline_range": {{ "start": 0, "end": 120000 }},
                "source_range": {{ "start": 0, "end": 120000 }} }} ] }}"#
        )
    };
    assert!(matches!(
        model_error(&adjusted("", "", &track("audio", ""), "")),
        ModelError::IncompatibleClipSource { clip, .. } if clip.as_str() == "elsewhere"
    ));
    assert_eq!(
        model_error(&adjusted(
            "",
            "",
            &track("caption", r#""captions": {},"#),
            ""
        )),
        ModelError::InvalidCaptions {
            track: id(TrackId::new, "x")
        }
    );
}

#[test]
fn an_adjustment_clip_takes_no_matte_and_is_no_matte() {
    assert_eq!(
        model_error(&adjusted(
            "",
            r#", "matte": { "source": "picture", "mode": "alpha" }"#,
            "",
            ""
        )),
        ModelError::InvalidMatte {
            clip: id(ClipId::new, "adjust")
        }
    );
    let matted = r#", { "id": "m", "name": "M", "kind": "video", "clips": [
      { "id": "matted", "source_id": "video", "timeline_range": { "start": 0, "end": 120000 },
        "source_range": { "start": 0, "end": 120000 }, "matte": { "source": "adjust", "mode": "alpha" } } ] }"#;
    assert_eq!(
        model_error(&adjusted("", "", matted, "")),
        ModelError::InvalidMatte {
            clip: id(ClipId::new, "matted")
        }
    );
}

#[test]
fn an_adjustment_clip_transitions_only_by_fade() {
    let track = |from: &str, to: &str, transition: &str| {
        format!(
            r#", {{ "id": "t", "name": "T", "kind": "video", "clips": [
              {{ "id": "first", "source_id": "{from}", "timeline_range": {{ "start": 0, "end": 120000 }},
                "source_range": {{ "start": 0, "end": 120000 }} }},
              {{ "id": "second", "source_id": "{to}", "timeline_range": {{ "start": 60000, "end": 180000 }},
                "source_range": {{ "start": 0, "end": 120000 }} }} ],
              "transitions": [ {{ "from_clip": "first", "to_clip": "second",
                "timeline_range": {{ "start": 60000, "end": 120000 }}, "easing": {{ "type": "linear" }},
                "effect": {{ "id": "{transition}", "version": 1 }} }} ] }}"#
        )
    };
    for (from, to) in [
        ("adjust", "video"),
        ("video", "adjust"),
        ("adjust", "adjust"),
    ] {
        Project::from_json(&adjusted(
            "",
            "",
            &track(from, to, crate::ADJUSTMENT_TRANSITION_ID),
            "",
        ))
        .expect("a fade");
        let expected = if from == "adjust" { "first" } else { "second" };
        assert_eq!(
            model_error(&adjusted(
                "",
                "",
                &track(from, to, "alpha2.transition.directional"),
                ""
            )),
            ModelError::InvalidAdjustmentTransition {
                track: id(TrackId::new, "t"),
                clip: id(ClipId::new, expected),
            },
            "{from} to {to}"
        );
    }
    Project::from_json(&adjusted(
        "",
        "",
        &track("video", "video", "alpha2.transition.directional"),
        "",
    ))
    .expect("pictures take any transition");
}

#[test]
fn a_media_node_cannot_show_an_adjustment_source() {
    let compositions = r#"{ "id": "look", "nodes": [ { "id": "shown", "op": { "type": "media", "source_id": "adjust" } } ], "output": "shown" }"#;
    assert!(matches!(
        model_error(&adjusted("", "", "", compositions)),
        ModelError::InvalidNodeSource { node, source, .. }
            if node.as_str() == "shown" && source.as_str() == "adjust"
    ));
}
