use crate::{Project, ProjectLoadError};
use serde_json::{Value, json};

fn constant(value: &Value) -> Value {
    json!({ "mode": "constant", "value": value })
}

fn keyframe(time: usize) -> Value {
    json!({ "time": time, "value": 0.5, "interpolation": { "type": "linear" } })
}

/// A document with one instance of every bounded sequence.
fn document() -> Value {
    let point = json!({ "x": 0.5, "y": 0.5 });
    let path = json!({ "subpaths": [{ "closed": true, "vertices": [{ "point": point }] }] });
    let style = json!({
        "strokes": [{ "color": "#000", "width": 0.01 }],
        "shadows": [{ "color": "#000", "offset": { "x": 0, "y": 0.01 }, "blur": 0.01 }],
        "animators": [{ "selector": { "type": "class", "class": "k" },
                        "properties": { "opacity": constant(&json!(0.5)) } }] });
    let eq = json!({ "type": "eq", "bands": [{ "kind": "peaking", "frequency_hz": 1000, "q": 1, "gain_db": 3 }] });
    json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48000 },
        "sources": [
            { "id": "v", "kind": "video", "locator": "v.mp4" },
            { "id": "a", "kind": "audio", "locator": "a.m4a" },
            { "id": "seq", "kind": "video", "locator": "asset://seq",
              "provider": { "format": "image_sequence", "frames": ["f1"], "frame_rate": { "num": 24, "den": 1 } } },
            { "id": "shapes", "kind": "generated", "generator": { "type": "shapes", "items": [
                { "type": "group", "items": [{ "type": "path", "path": constant(&path) }] },
                { "type": "stroke", "width": constant(&json!(0.01)), "paint": constant(&json!({ "type": "solid", "color": "#fff" })),
                  "dash": { "pattern": [constant(&json!(0.02))] } },
                { "type": "fill", "paint": constant(&json!({ "type": "linear_gradient",
                    "start": { "x": 0, "y": 0 }, "end": { "x": 1, "y": 0 },
                    "stops": [{ "offset": 0, "color": "#000" }, { "offset": 1, "color": "#fff" }] })) }
            ] } },
            { "id": "text", "kind": "generated", "generator": { "type": "text",
                "text": [{ "text": "a", "class": ["k"] }], "style": style } },
            { "id": "layout", "kind": "generated", "generator": { "type": "layout", "root": {
                "strokes": [{ "color": "#000", "width": 0.01 }],
                "children": [
                    { "text": constant(&json!("a")), "text_animators": [{ "selector": { "type": "range", "shape": "round" },
                        "properties": { "tracking": constant(&json!(0.2)) } }] },
                    { "shape": { "items": [{ "type": "fill", "paint": constant(&json!({ "type": "solid", "color": "#fff" })) }] } }
                ] } } }
        ],
        "effect_resources": [{ "id": "look", "kind": "lut3d", "locator": "look.cube" }],
        "compositions": [
            { "id": "group", "inputs": ["image"], "controls": [{ "name": "softness", "node": "blur", "param": "radius" }],
              "nodes": [{ "id": "in", "op": { "type": "group_input", "index": 0 } },
                        { "id": "blur", "op": { "type": "effect", "input": "in", "effect": { "id": "alpha2.effect.gaussian-blur", "version": 1 } } }],
              "output": "blur" },
            { "id": "look", "nodes": [
                { "id": "clip", "op": { "type": "clip" } },
                { "id": "soft", "op": { "type": "group", "composition": "group", "inputs": ["clip"] } },
                { "id": "fx", "op": { "type": "effect", "input": "clip", "inputs": { "k0": "clip" },
                    "effect": { "id": "alpha2.effect.gaussian-blur", "version": 1 } } },
                { "id": "masked", "op": { "type": "shape_mask", "input": "clip", "masks": [{ "shape": { "type": "ellipse",
                    "center": constant(&point), "size": constant(&json!({ "x": 0.2, "y": 0.2 })) } }] } },
                { "id": "out", "op": { "type": "merge", "layers": [{ "input": "soft" }] } }
              ], "output": "out" }
        ],
        "timeline": { "tracks": [
            { "id": "t", "name": "T", "kind": "video", "clips": [
                { "id": "c", "source_id": "v", "timeline_range": { "start": 0, "end": 10 }, "source_range": { "start": 0, "end": 10 },
                  "visual": { "opacity": { "mode": "keyframes", "keyframes": [keyframe(0)] },
                              "position": { "mode": "keyframes", "keyframes": [{ "time": 0, "value": point, "interpolation": { "type": "linear" } }] } },
                  "masks": [{ "shape": { "type": "ellipse", "center": constant(&json!({ "x": 0.5, "y": 0.5 })),
                              "size": constant(&json!({ "x": 0.2, "y": 0.2 })) } }] }
              ], "transitions": [{ "from_clip": "c", "to_clip": "c", "timeline_range": { "start": 0, "end": 5 },
                                   "easing": { "type": "linear" }, "effect": { "id": "cross_dissolve", "version": 1, "params": {} } }] },
            { "id": "music", "name": "M", "kind": "audio", "audio": { "effects": [eq.clone()] }, "clips": [
                { "id": "m", "source_id": "a", "timeline_range": { "start": 0, "end": 10 }, "source_range": { "start": 0, "end": 10 },
                  "audio": { "effects": [eq.clone()], "channels": [0] } }
            ] },
            { "id": "subs", "name": "S", "kind": "caption", "captions": { "style": style_patch(),
                "cues": [{ "timeline_range": { "start": 0, "end": 10 }, "text": "hi" }] } }
        ] },
        "audio": { "effects": [eq] },
        "data": [
            { "id": "s", "anchor": { "type": "timeline" }, "kind": "scalars", "interpolation": "hold",
              "channels": ["c"], "times": [0], "values": [0.5] },
            { "id": "w", "anchor": { "type": "timeline" }, "kind": "words", "words": [{ "start": 0, "end": 10, "text": "hi" }] },
            { "id": "i", "anchor": { "type": "timeline" }, "kind": "intervals", "intervals": [{ "start": 0, "end": 10 }] }
        ]
    })
}

fn style_patch() -> Value {
    json!({
        "strokes": [{ "color": "#000", "width": 0.01 }],
        "shadows": [{ "color": "#000", "offset": { "x": 0, "y": 0.01 }, "blur": 0.01 }],
        "animators": [{ "selector": { "type": "class", "class": "k" },
                        "properties": { "opacity": constant(&json!(0.5)) } }] })
}

/// `document()` with the sequence at `pointer` holding `count` elements, each `element(index)`;
/// an object gets keys `k0`, `k1`, ...
fn with_count(pointer: &str, count: usize, element: &dyn Fn(usize) -> Value) -> String {
    let mut document = document();
    let target = document
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("{pointer} is in the document"));
    *target = if target.is_object() {
        Value::Object(
            (0..count)
                .map(|at| (format!("k{at}"), element(at)))
                .collect(),
        )
    } else {
        Value::Array((0..count).map(element).collect())
    };
    document.to_string()
}

fn refused_for_its_count(loaded: &Result<Project, ProjectLoadError>, maximum: usize) -> bool {
    matches!(loaded, Err(ProjectLoadError::Json(error))
        if error.to_string().contains(&format!("invalid length {}, expected a", maximum + 1)))
}

type Element = Box<dyn Fn(usize) -> Value>;

#[test]
#[allow(clippy::too_many_lines)]
fn every_bounded_sequence_is_refused_by_the_parser_one_past_its_limit() {
    // Well shaped; validation refuses its transition from a clip to itself.
    assert!(matches!(
        Project::from_json(&document().to_string()),
        Err(ProjectLoadError::InvalidProject(_))
    ));
    let copy_of = |pointer: &str| {
        let element = document()
            .pointer(&format!("{pointer}/0"))
            .unwrap_or_else(|| panic!("{pointer} has an element"))
            .clone();
        move |_: usize| element.clone()
    };
    let sources =
        |at: usize| json!({ "id": format!("s{at}"), "kind": "audio", "locator": "s.m4a" });
    let clips = |at: usize| {
        json!({ "id": format!("c{at}"), "source_id": "v",
        "timeline_range": { "start": at * 10, "end": at * 10 + 10 }, "source_range": { "start": 0, "end": 10 } })
    };
    let tracks = |at: usize| json!({ "id": format!("t{at}"), "name": "T", "kind": "video" });
    let compositions = |at: usize| json!({ "id": format!("k{at}"), "nodes": [] });
    let resources =
        |at: usize| json!({ "id": format!("r{at}"), "kind": "lut3d", "locator": "r.cube" });
    let sets = |at: usize| json!({ "id": format!("d{at}"), "anchor": { "type": "timeline" }, "kind": "words", "words": [] });
    let nodes = |at: usize| json!({ "id": format!("n{at}"), "op": { "type": "clip" } });
    let keyframes = |at: usize| keyframe(at);
    let points = |at: usize| json!({ "time": at, "value": { "x": 0.5, "y": 0.5 }, "interpolation": { "type": "linear" } });
    let channels = |at: usize| json!(format!("c{at}"));
    let number = |_: usize| json!(0.5);
    let rows: Vec<(&str, usize, Element)> = vec![
        ("/sources", crate::MAX_CLIPS, Box::new(sources)),
        (
            "/effect_resources",
            crate::MAX_EFFECT_RESOURCES,
            Box::new(resources),
        ),
        (
            "/compositions",
            crate::MAX_COMPOSITIONS,
            Box::new(compositions),
        ),
        ("/data", crate::MAX_DATA_SETS, Box::new(sets)),
        ("/timeline/tracks", crate::MAX_TRACKS, Box::new(tracks)),
        (
            "/timeline/tracks/0/clips",
            crate::MAX_CLIPS_PER_TRACK,
            Box::new(clips),
        ),
        (
            "/timeline/tracks/0/transitions",
            crate::MAX_TRANSITIONS_PER_TRACK,
            Box::new(copy_of("/timeline/tracks/0/transitions")),
        ),
        (
            "/timeline/tracks/0/clips/0/masks",
            crate::MAX_CLIP_MASKS,
            Box::new(copy_of("/timeline/tracks/0/clips/0/masks")),
        ),
        (
            "/timeline/tracks/0/clips/0/visual/opacity/keyframes",
            crate::MAX_KEYFRAMES,
            Box::new(keyframes),
        ),
        (
            "/timeline/tracks/0/clips/0/visual/position/keyframes",
            crate::MAX_KEYFRAMES,
            Box::new(points),
        ),
        (
            "/timeline/tracks/1/clips/0/audio/effects",
            crate::MAX_CHAIN_EFFECTS,
            Box::new(copy_of("/timeline/tracks/1/clips/0/audio/effects")),
        ),
        (
            "/timeline/tracks/1/clips/0/audio/effects/0/bands",
            crate::MAX_EQ_BANDS,
            Box::new(copy_of("/timeline/tracks/1/clips/0/audio/effects/0/bands")),
        ),
        (
            "/timeline/tracks/1/clips/0/audio/channels",
            crate::MAX_SELECTED_CHANNELS,
            Box::new(|at| json!(at)),
        ),
        (
            "/timeline/tracks/1/audio/effects",
            crate::MAX_CHAIN_EFFECTS,
            Box::new(copy_of("/timeline/tracks/1/audio/effects")),
        ),
        (
            "/audio/effects",
            crate::MAX_CHAIN_EFFECTS,
            Box::new(copy_of("/audio/effects")),
        ),
        (
            "/timeline/tracks/2/captions/cues",
            crate::MAX_CLIPS_PER_TRACK,
            Box::new(copy_of("/timeline/tracks/2/captions/cues")),
        ),
        (
            "/timeline/tracks/2/captions/style/strokes",
            crate::MAX_TEXT_STROKES,
            Box::new(copy_of("/timeline/tracks/2/captions/style/strokes")),
        ),
        (
            "/timeline/tracks/2/captions/style/shadows",
            crate::MAX_TEXT_SHADOWS,
            Box::new(copy_of("/timeline/tracks/2/captions/style/shadows")),
        ),
        (
            "/timeline/tracks/2/captions/style/animators",
            crate::MAX_TEXT_ANIMATORS,
            Box::new(copy_of("/timeline/tracks/2/captions/style/animators")),
        ),
        (
            "/compositions/0/inputs",
            crate::MAX_GROUP_INPUTS,
            Box::new(channels),
        ),
        (
            "/compositions/0/controls",
            crate::MAX_COMPOSITION_CONTROLS,
            Box::new(copy_of("/compositions/0/controls")),
        ),
        (
            "/compositions/1/nodes",
            crate::MAX_COMPOSITION_NODES,
            Box::new(nodes),
        ),
        (
            "/compositions/1/nodes/1/op/inputs",
            crate::MAX_GROUP_INPUTS,
            Box::new(copy_of("/compositions/1/nodes/1/op/inputs")),
        ),
        (
            "/compositions/1/nodes/2/op/inputs",
            crate::MAX_EFFECT_INPUTS,
            Box::new(|_| json!("clip")),
        ),
        (
            "/compositions/1/nodes/3/op/masks",
            crate::MAX_CLIP_MASKS,
            Box::new(copy_of("/compositions/1/nodes/3/op/masks")),
        ),
        (
            "/compositions/1/nodes/4/op/layers",
            crate::MAX_MERGE_LAYERS,
            Box::new(copy_of("/compositions/1/nodes/4/op/layers")),
        ),
        (
            "/sources/2/provider/frames",
            crate::MAX_SEQUENCE_FRAMES,
            Box::new(copy_of("/sources/2/provider/frames")),
        ),
        (
            "/sources/3/generator/items",
            crate::MAX_SHAPE_ITEMS,
            Box::new(copy_of("/sources/3/generator/items")),
        ),
        (
            "/sources/3/generator/items/0/items",
            crate::MAX_SHAPE_ITEMS,
            Box::new(copy_of("/sources/3/generator/items/0/items")),
        ),
        (
            "/sources/3/generator/items/0/items/0/path/value/subpaths",
            crate::MAX_PATH_SUBPATHS,
            Box::new(copy_of(
                "/sources/3/generator/items/0/items/0/path/value/subpaths",
            )),
        ),
        (
            "/sources/3/generator/items/0/items/0/path/value/subpaths/0/vertices",
            crate::MAX_PATH_VERTICES,
            Box::new(copy_of(
                "/sources/3/generator/items/0/items/0/path/value/subpaths/0/vertices",
            )),
        ),
        (
            "/sources/3/generator/items/1/dash/pattern",
            crate::MAX_DASH_ENTRIES,
            Box::new(copy_of("/sources/3/generator/items/1/dash/pattern")),
        ),
        (
            "/sources/3/generator/items/2/paint/value/stops",
            crate::MAX_GRADIENT_STOPS,
            Box::new(copy_of("/sources/3/generator/items/2/paint/value/stops")),
        ),
        (
            "/sources/4/generator/text",
            crate::MAX_TEXT_RUNS,
            Box::new(copy_of("/sources/4/generator/text")),
        ),
        (
            "/sources/4/generator/text/0/class",
            crate::MAX_RUN_CLASSES,
            Box::new(channels),
        ),
        (
            "/sources/4/generator/style/strokes",
            crate::MAX_TEXT_STROKES,
            Box::new(copy_of("/sources/4/generator/style/strokes")),
        ),
        (
            "/sources/4/generator/style/shadows",
            crate::MAX_TEXT_SHADOWS,
            Box::new(copy_of("/sources/4/generator/style/shadows")),
        ),
        (
            "/sources/4/generator/style/animators",
            crate::MAX_TEXT_ANIMATORS,
            Box::new(copy_of("/sources/4/generator/style/animators")),
        ),
        (
            "/sources/5/generator/root/children",
            crate::MAX_LAYOUT_NODES,
            Box::new(copy_of("/sources/5/generator/root/children")),
        ),
        (
            "/sources/5/generator/root/strokes",
            crate::MAX_TEXT_STROKES,
            Box::new(copy_of("/sources/5/generator/root/strokes")),
        ),
        (
            "/sources/5/generator/root/children/0/text_animators",
            crate::MAX_TEXT_ANIMATORS,
            Box::new(copy_of(
                "/sources/5/generator/root/children/0/text_animators",
            )),
        ),
        (
            "/sources/5/generator/root/children/1/shape/items",
            crate::MAX_SHAPE_ITEMS,
            Box::new(copy_of("/sources/5/generator/root/children/1/shape/items")),
        ),
        (
            "/data/0/channels",
            crate::MAX_DATA_CHANNELS,
            Box::new(channels),
        ),
        (
            "/data/0/times",
            crate::MAX_DATA_VALUES,
            Box::new(|at| json!(at)),
        ),
        ("/data/0/values", crate::MAX_DATA_VALUES, Box::new(number)),
        (
            "/data/1/words",
            crate::MAX_DATA_WORDS,
            Box::new(copy_of("/data/1/words")),
        ),
        (
            "/data/2/intervals",
            crate::MAX_DATA_INTERVALS,
            Box::new(copy_of("/data/2/intervals")),
        ),
    ];
    for (pointer, maximum, element) in rows {
        let at = Project::from_json(&with_count(pointer, maximum, &element));
        assert!(
            !refused_for_its_count(&at, maximum),
            "{pointer} at {maximum}: {:?}",
            at.err()
        );
        assert!(
            !matches!(at, Err(ProjectLoadError::Json(_))),
            "{pointer} at {maximum} is well shaped: {:?}",
            at.err()
        );
        let over = Project::from_json(&with_count(pointer, maximum + 1, &element));
        assert!(
            refused_for_its_count(&over, maximum),
            "{pointer} at {}: {:?}",
            maximum + 1,
            over.err()
        );
    }
}
