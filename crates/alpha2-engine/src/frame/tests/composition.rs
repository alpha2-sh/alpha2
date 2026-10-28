//! Composition graphs rendered end to end: project JSON, evaluation, and the
//! renderer.

use super::*;
use crate::EngineProject;
use crate::frame::FrameRender;

const W: u32 = 4;
const H: u32 = 2;

/// A renderer with the tint effect, a fade transition, and the internal
/// passes compositions use.
fn renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    for definition in [
        runtime_tint_effect("fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }"),
        EffectDef {
            id: "test.fade".into(),
            version: 1,
            kind: EffectKind::Transition,
            inputs: Vec::new(),
            params: Vec::new(),
            resources: Vec::new(),
            wgsl_body: "fn transition(uv: vec2f) -> vec4f { return mix(from_color(uv), to_color(uv), progress()); }".into(),
            tier_min: CapabilityTier::TierB,
            license: Some("MIT".into()),
        },
        alpha2_render::generator_definition(),
        alpha2_render::track_matte_definition(),
    ]
    .into_iter()
    .chain(alpha2_render::mask_combine_definitions())
    {
        pollster::block_on(renderer.register_effect(definition)).expect("effect compiles");
    }
    renderer
}

fn fill(red: f32, green: f32, blue: f32) -> String {
    format!(
        r#"{{ "type": "fill", "paint": {{ "type": "solid", "color": {{ "red": {red}, "green": {green}, "blue": {blue}, "alpha": 1 }} }} }}"#
    )
}

/// One clip of the white source over 60000..180000 showing composition `g`
/// with `nodes` and `output`; `reference` holds the clip's other composition
/// fields. Sources: white, red, and blue fills, a white rectangle over the
/// left half, and a host-supplied video.
fn project(nodes: &str, output: &str, reference: &str, clip_extra: &str) -> EngineProject {
    project_with(nodes, output, reference, clip_extra, "", "", "white")
}

/// Like [`project`], with more compositions and sources (each a JSON list
/// tail, starting with a comma) and the clip's source.
fn project_with(
    nodes: &str,
    output: &str,
    reference: &str,
    clip_extra: &str,
    compositions: &str,
    sources: &str,
    clip_source: &str,
) -> EngineProject {
    let json = format!(
        r#"{{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": {{ "width": {W}, "height": {H}, "frame_rate": {{ "num": 30, "den": 1 }}, "sample_rate": 48000 }},
          "sources": [
            {{ "id": "white", "kind": "generated", "generator": {white} }},
            {{ "id": "red", "kind": "generated", "generator": {red} }},
            {{ "id": "blue", "kind": "generated", "generator": {blue} }},
            {{ "id": "left", "kind": "generated", "generator": {{ "type": "shapes", "items": [
                {{ "type": "rectangle", "center": {{ "mode": "constant", "value": {{ "x": 0.25, "y": 0.5 }} }},
                  "size": {{ "mode": "constant", "value": {{ "x": 0.5, "y": 1 }} }} }},
                {{ "type": "fill", "paint": {{ "mode": "constant", "value": {{ "type": "solid", "color": {{ "red": 1, "green": 1, "blue": 1, "alpha": 1 }} }} }} }} ] }} }},
            {{ "id": "video", "kind": "video", "locator": "asset://video" }} {sources}
          ],
          "compositions": [ {{ "id": "g", "nodes": [{nodes}] {output} }} {compositions} ],
          "timeline": {{ "tracks": [
            {{ "id": "t1", "name": "V", "kind": "video", "clips": [
              {{ "id": "c", "source_id": "{clip_source}", "timeline_range": {{ "start": 60000, "end": 180000 }},
                "source_range": {{ "start": 0, "end": 120000 }},
                "composition": {{ "id": "g" {reference} }} {clip_extra} }} ] }}
          ] }}
        }}"#,
        white = fill(1.0, 1.0, 1.0),
        red = fill(1.0, 0.0, 0.0),
        blue = fill(0.0, 0.0, 1.0),
        output = if output.is_empty() {
            String::new()
        } else {
            format!(r#", "output": "{output}""#)
        },
    );
    let project = alpha2_model::Project::from_json(&json).expect("a valid project");
    // What is rendered is what a saved document reloads as.
    let saved = serde_json::to_string(&project).expect("serializes");
    assert_eq!(
        alpha2_model::Project::from_json(&saved).expect("reloads"),
        project
    );
    EngineProject::new(project).expect("indexed")
}

fn tint(id: &str, input: &str, color: [f32; 4]) -> String {
    let [r, g, b, a] = color;
    format!(
        r#"{{ "id": "{id}", "op": {{ "type": "effect", "input": "{input}", "effect": {{ "id": "test.tint", "version": 1,
            "params": {{ "tint": {{ "mode": "constant", "value": {{ "type": "color", "value": [{r}, {g}, {b}, {a}] }} }} }} }} }} }}"#
    )
}

fn render(
    renderer: &Renderer,
    project: &EngineProject,
    time: i64,
    frames: &dyn FrameProvider,
) -> FrameRender {
    let frame = project.evaluate(MediaTime(time)).expect("evaluates");
    // wgpu reports validation errors out of band; a scope makes an invalid pass fail here.
    let gpu = gpu();
    let device = gpu.device();
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer
        .render_frame(RenderSize::new(W, H).expect("size"), &frame, frames)
        .expect("frame renders");
    let error = pollster::block_on(scope.pop());
    assert!(error.is_none(), "{error:?}");
    output
}

fn pixels(renderer: &Renderer, project: &EngineProject, time: i64) -> Vec<u8> {
    let output = render(renderer, project, time, &MissingFrames);
    assert!(output.report.is_complete(), "{:?}", output.report);
    read_rgba8(renderer, &output.texture.expect("a complete frame"))
}

fn pixel(pixels: &[u8], x: usize, y: usize) -> [u8; 4] {
    let at = (y * W as usize + x) * 4;
    pixels[at..at + 4].try_into().expect("four bytes")
}

#[test]
fn two_effect_branches_of_one_picture_merge_back_together() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let branches = format!(
        r#"{}, {}, {{ "id": "clip", "op": {{ "type": "clip" }} }},
           {{ "id": "merge", "op": {{ "type": "merge", "layers": [
             {{ "input": "reds" }}, {{ "input": "blues", "blend_mode": "screen" }} ] }} }}"#,
        tint("reds", "clip", [1.0, 0.0, 0.0, 1.0]),
        tint("blues", "clip", [0.0, 0.0, 1.0, 1.0]),
    );
    let merged = project(&branches, "merge", "", "");
    let image = pixels(&renderer, &merged, 90_000);
    // Red screen blue is magenta, in every pixel.
    for chunk in image.as_chunks::<4>().0 {
        assert_eq!(*chunk, [255, 0, 255, 255]);
    }
    let reds = project(&branches, "reds", "", "");
    assert_eq!(
        pixel(&pixels(&renderer, &reds, 90_000), 0, 0),
        [255, 0, 0, 255]
    );
}

#[test]
fn a_bypassed_node_passes_its_input_through() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let nodes = |bypass: bool| {
        format!(
            r#"{{ "id": "clip", "op": {{ "type": "clip" }} }}, {}"#,
            tint("reds", "clip", [1.0, 0.0, 0.0, 1.0]).replacen(
                r#""id": "reds","#,
                &format!(r#""id": "reds", "bypass": {bypass},"#),
                1
            )
        )
    };
    let tinted = project(&nodes(false), "reds", "", "");
    assert_eq!(
        pixel(&pixels(&renderer, &tinted, 90_000), 0, 0),
        [255, 0, 0, 255]
    );
    let bypassed = project(&nodes(true), "reds", "", "");
    assert_eq!(
        pixel(&pixels(&renderer, &bypassed, 90_000), 0, 0),
        [255, 255, 255, 255]
    );
}

#[test]
fn a_matte_node_keeps_one_source_where_another_says() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let nodes = r#"{ "id": "clip", "op": { "type": "clip" } },
        { "id": "shape", "op": { "type": "media", "source_id": "left" } },
        { "id": "red", "op": { "type": "media", "source_id": "red" } },
        { "id": "keep", "op": { "type": "matte", "input": "red", "matte": "shape", "mode": "alpha" } }"#;
    let image = pixels(&renderer, &project(nodes, "keep", "", ""), 90_000);
    // Red on the left, where the rectangle is; the black background on the right.
    assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&image, 3, 1), [0, 0, 0, 255]);
    let inverted = nodes.replace(r#""mode": "alpha""#, r#""mode": "alpha_inverted""#);
    let image = pixels(&renderer, &project(&inverted, "keep", "", ""), 90_000);
    assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&image, 3, 1), [255, 0, 0, 255]);
}

#[test]
fn a_transition_node_mixes_its_inputs_at_its_progress() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let nodes = r#"{ "id": "red", "op": { "type": "media", "source_id": "red" } },
        { "id": "blue", "op": { "type": "media", "source_id": "blue" } },
        { "id": "mix", "op": { "type": "transition", "from": "red", "to": "blue",
          "effect": { "id": "test.fade", "version": 1 }, "progress": { "mode": "constant", "value": 0.25 } } }"#;
    let image = pixels(&renderer, &project(nodes, "mix", "", ""), 90_000);
    assert_rgba8_near(&pixel(&image, 1, 1), &[191, 0, 64, 255], 1);
}

#[test]
fn settings_animate_in_composition_time_and_move_with_the_clip() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    // Opacity rises from 0 to 1 over the first second of composition time.
    let nodes = r#"{ "id": "clip", "op": { "type": "clip" } },
        { "id": "fade", "op": { "type": "transform", "input": "clip", "transform": {
          "position": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } }, "anchor": { "x": 0.5, "y": 0.5 },
          "scale": { "mode": "constant", "value": { "x": 1, "y": 1 } }, "rotation_degrees": { "mode": "constant", "value": 0 },
          "opacity": { "mode": "keyframes", "keyframes": [
            { "time": 0, "value": 0, "interpolation": { "type": "linear" } },
            { "time": 120000, "value": 1, "interpolation": { "type": "hold" } } ] } } } }"#;
    let here = project(nodes, "fade", "", "");
    let start = pixel(&pixels(&renderer, &here, 60_000), 0, 0);
    let middle = pixel(&pixels(&renderer, &here, 120_000), 0, 0);
    assert_eq!(
        start,
        [0, 0, 0, 255],
        "transparent at composition time zero"
    );
    assert!(middle[0] > 0 && middle[0] < 255, "half-way: {middle:?}");
    let moved = {
        let mut document = here.project().clone();
        document.timeline.tracks[0].clips[0].timeline_range =
            alpha2_time::TimeRange::new(MediaTime(300_000), MediaTime(420_000)).expect("range");
        EngineProject::new(document).expect("indexed")
    };
    assert_eq!(pixel(&pixels(&renderer, &moved, 360_000), 0, 0), middle);
    let offset = project(nodes, "fade", r#", "offset": 60000"#, "");
    assert_eq!(pixel(&pixels(&renderer, &offset, 60_000), 0, 0), middle);
}

#[test]
fn media_nodes_read_host_frames_at_their_own_time_and_wait_for_them() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let red = rgba_texture(&renderer, [255, 0, 0, 255]);
    let blue = rgba_texture(&renderer, [0, 0, 255, 255]);
    let (red, blue) = (
        red.create_view(&wgpu::TextureViewDescriptor::default()),
        blue.create_view(&wgpu::TextureViewDescriptor::default()),
    );
    // Red at source time zero, blue at any other.
    let frames = TimedFrames {
        source: SourceId::new("video").expect("id"),
        first: &red,
        second: &blue,
    };
    let nodes = r#"{ "id": "clip", "op": { "type": "clip" } },
        { "id": "video", "op": { "type": "media", "source_id": "video" } }"#;
    let project = project(nodes, "video", "", "");
    let needs = project.frame_needs(MediaTime(60_000)).expect("needs");
    assert_eq!(needs.len(), 1);
    assert_eq!(needs[0].source_id.as_str(), "video");
    assert_eq!(needs[0].media_time, MediaTime::ZERO);
    let first = render(&renderer, &project, 60_000, &frames);
    assert_eq!(
        pixel(
            &read_rgba8(&renderer, &first.texture.expect("complete")),
            0,
            0
        ),
        [255, 0, 0, 255]
    );
    let later = render(&renderer, &project, 90_000, &frames);
    assert_eq!(
        pixel(
            &read_rgba8(&renderer, &later.texture.expect("complete")),
            0,
            0
        ),
        [0, 0, 255, 255]
    );
    // Without the frame there is no picture, not an empty one.
    let missing = render(&renderer, &project, 60_000, &MissingFrames);
    assert!(missing.texture.is_none());
    assert_eq!(
        missing.report.missing_sources,
        [SourceId::new("video").expect("id")]
    );
}

#[test]
fn the_clip_node_reads_the_source_where_the_clip_time_map_says() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let red = rgba_texture(&renderer, [255, 0, 0, 255]);
    let blue = rgba_texture(&renderer, [0, 0, 255, 255]);
    let (red, blue) = (
        red.create_view(&wgpu::TextureViewDescriptor::default()),
        blue.create_view(&wgpu::TextureViewDescriptor::default()),
    );
    let frames = TimedFrames {
        source: SourceId::new("video").expect("id"),
        first: &red,
        second: &blue,
    };
    let nodes = format!(
        r#"{{ "id": "clip", "op": {{ "type": "clip" }} }}, {}"#,
        tint("keep", "clip", [1.0, 1.0, 1.0, 1.0])
    );
    let with_map = |time_map: &str| {
        let project = project(&nodes, "keep", "", time_map);
        let mut document = project.project().clone();
        document.timeline.tracks[0].clips[0].source_id = SourceId::new("video").expect("id");
        EngineProject::new(document).expect("indexed")
    };
    let color = |project: &EngineProject, time: i64| {
        let output = render(&renderer, project, time, &frames);
        pixel(
            &read_rgba8(&renderer, &output.texture.expect("complete")),
            0,
            0,
        )
    };
    // Forward, source time zero is at the clip's start only.
    let forward = with_map("");
    assert_eq!(color(&forward, 60_000), [255, 0, 0, 255]);
    assert_eq!(color(&forward, 120_000), [0, 0, 255, 255]);
    // Frozen on it, it is shown throughout.
    let frozen = with_map(r#", "time_map": { "type": "freeze", "source_time": 0 }"#);
    assert_eq!(color(&frozen, 120_000), [255, 0, 0, 255]);
    // Reversed, it is shown at the clip's end instead.
    let reversed = with_map(r#", "time_map": { "type": "reverse" }"#);
    assert_eq!(color(&reversed, 60_000), [0, 0, 255, 255]);
    assert_eq!(color(&reversed, 176_000), [255, 0, 0, 255]);
}

#[test]
fn an_output_that_needs_an_unconnected_input_is_an_error() {
    let nodes = r#"{ "id": "clip", "op": { "type": "clip" } },
        { "id": "keep", "op": { "type": "matte", "input": "clip", "mode": "luma" } }"#;
    let project = project(nodes, "keep", "", "");
    match project.evaluate(MediaTime(90_000)) {
        Err(alpha2_eval::EvalError::IncompleteComposition { reason, .. }) => assert_eq!(
            reason,
            alpha2_eval::PlanError::MissingInput {
                node: alpha2_model::NodeId::new("keep").expect("id"),
                input: "matte".into(),
            }
        ),
        other => panic!("expected an incomplete composition, got {other:?}"),
    }
    project
        .evaluate(MediaTime(30_000))
        .expect("before the clip, nothing is incomplete");
}

#[test]
fn a_group_draws_exactly_what_its_nodes_draw_inline() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    // Flat: the clip tinted twice. No channel lands on a half step: 0.5 would
    // store 127.5, which GPUs round to 127 or 128.
    let flat = project(
        &[
            r#"{ "id": "clip", "op": { "type": "clip" } }"#.to_owned(),
            tint("a", "clip", [1.0, 0.6, 0.25, 1.0]),
            tint("b", "a", [0.6, 1.0, 1.0, 1.0]),
        ]
        .join(","),
        "b",
        "",
        "",
    );
    // Grouped: the same two tints in a definition whose first tint is a control, set by the
    // group node.
    let definition = format!(
        r#", {{ "id": "pair", "inputs": ["image"],
              "controls": [{{ "name": "warmth", "node": "a", "param": "tint" }}],
              "nodes": [{{ "id": "in", "op": {{ "type": "group_input", "index": 0 }} }}, {a}, {b}],
              "output": "b" }}"#,
        a = tint("a", "in", [1.0, 1.0, 1.0, 1.0]),
        b = tint("b", "a", [0.6, 1.0, 1.0, 1.0]),
    );
    let grouped = project_with(
        r#"{ "id": "clip", "op": { "type": "clip" } },
           { "id": "group", "op": { "type": "group", "composition": "pair", "inputs": ["clip"],
             "controls": { "warmth": { "mode": "constant", "value": { "type": "color", "value": [1.0, 0.6, 0.25, 1.0] } } } } }"#,
        "group",
        "",
        "",
        &definition,
        "",
        "white",
    );
    let expected = pixels(&renderer, &flat, 120_000);
    assert_eq!(pixel(&expected, 0, 0), [153, 153, 64, 255]);
    assert_eq!(pixels(&renderer, &grouped, 120_000), expected);
}

#[test]
fn a_composition_source_draws_what_its_nodes_draw_as_the_clip_picture() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let nodes = [
        r#"{ "id": "left", "op": { "type": "media", "source_id": "left" } }"#.to_owned(),
        tint("warm", "left", [1.0, 0.6, 0.25, 1.0]),
    ]
    .join(",");
    // The nodes as the clip's own composition, over a white clip they do not read.
    let inline = project(&nodes, "warm", "", "");
    // The same nodes as a composition the clip shows as its source.
    let source = project_with(
        r#"{ "id": "clip", "op": { "type": "clip" } }"#,
        "clip",
        "",
        "",
        &format!(r#", {{ "id": "title", "nodes": [{nodes}], "output": "warm" }}"#),
        r#", { "id": "title", "kind": "composition", "composition": "title" }"#,
        "title",
    );
    let expected = pixels(&renderer, &inline, 120_000);
    assert_eq!(pixel(&expected, 0, 0), [255, 153, 64, 255]);
    assert_eq!(pixel(&expected, 3, 0), [0, 0, 0, 255]);
    assert_eq!(pixels(&renderer, &source, 120_000), expected);
}

/// An effect with named `inputs` and float parameters `params` (name, default, max), each in
/// `0..=max`.
fn effect_with_inputs(
    id: &str,
    inputs: &[&str],
    params: &[(&str, f32, f32)],
    body: &str,
) -> EffectDef {
    EffectDef {
        id: id.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: inputs
            .iter()
            .map(|name| alpha2_render::InputSpec {
                name: (*name).into(),
            })
            .collect(),
        params: params
            .iter()
            .map(|(name, default, max)| alpha2_render::ParamSpec {
                name: (*name).into(),
                param_type: alpha2_render::ParamType::Float,
                default: alpha2_render::ParamValue::Float(*default),
                min: Some(0.0),
                max: Some(*max),
                step: None,
                unit: alpha2_render::ParamUnit::Normalized,
                enum_values: Vec::new(),
            })
            .collect(),
        resources: Vec::new(),
        wgsl_body: body.into(),
        tier_min: CapabilityTier::TierB,
        license: None,
    }
}

/// [`renderer`] with the built-in blur and the effects the named-input tests use:
/// `test.multiply-map`, `test.input-size`, `test.bright-pass`, `test.add-glow`, and
/// `test.displace`.
fn renderer_with_inputs() -> Renderer {
    let mut renderer = renderer();
    renderer
        .register_builtin_gaussian_blur()
        .expect("blur compiles");
    for definition in [
        effect_with_inputs(
            "test.multiply-map",
            &["map"],
            &[],
            "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * sample_map(uv); }",
        ),
        effect_with_inputs(
            "test.input-size",
            &["map"],
            &[],
            "fn effect(_uv: vec2f, _src: vec4f) -> vec4f { return vec4f(size_map() / 255.0, 0.0, 1.0); }",
        ),
        effect_with_inputs(
            "test.bright-pass",
            &[],
            &[("threshold", 0.7, 1.0)],
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { let luma = dot(src.rgb, vec3f(0.2126, 0.7152, 0.0722)); return select(vec4f(0.0), src, luma >= params.threshold); }",
        ),
        effect_with_inputs(
            "test.add-glow",
            &["glow"],
            &[("intensity", 1.0, 4.0)],
            "fn effect(uv: vec2f, src: vec4f) -> vec4f { return min(src + sample_glow(uv) * params.intensity, vec4f(1.0)); }",
        ),
        effect_with_inputs(
            "test.displace",
            &["map"],
            &[("amount", 0.05, 1.0)],
            "fn effect(uv: vec2f, _src: vec4f) -> vec4f { let offset = (sample_map(uv).rg - vec2f(0.5)) * params.amount; return sample_src(uv + offset); }",
        ),
    ] {
        pollster::block_on(renderer.register_effect(definition)).expect("effect compiles");
    }
    renderer
}

/// An effect node `id` of `effect` reading `input` and, by name, `inputs` (a JSON object).
fn effect_node(id: &str, effect: &str, input: &str, inputs: &str) -> String {
    format!(
        r#"{{ "id": "{id}", "op": {{ "type": "effect", "input": "{input}", "inputs": {inputs},
            "effect": {{ "id": "{effect}", "version": 1 }} }} }}"#
    )
}

#[test]
fn a_named_input_reads_another_branch() {
    let _lock = gpu_test_lock();
    let renderer = renderer_with_inputs();
    let nodes = format!(
        r#"{{ "id": "clip", "op": {{ "type": "clip" }} }},
           {{ "id": "red", "op": {{ "type": "media", "source_id": "red" }} }}, {}"#,
        effect_node("fx", "test.multiply-map", "clip", r#"{ "map": "red" }"#)
    );
    let image = pixels(&renderer, &project(&nodes, "fx", "", ""), 90_000);
    for chunk in image.as_chunks::<4>().0 {
        assert_eq!(*chunk, [255, 0, 0, 255]);
    }
}

#[test]
fn an_image_feeds_a_named_input_at_its_own_resolution() {
    // A media node feeds a named input as any other node does: its frame is asked for,
    // waited for, and bound at the size it was decoded at.
    let _lock = gpu_test_lock();
    let renderer = renderer_with_inputs();
    let texture = rgba_image_texture(&renderer, RenderSize::new(2, 1).expect("size"), &[255; 8]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let frames = SingleFrame {
        source: SourceId::new("video").expect("id"),
        texture: &view,
    };
    let nodes = format!(
        r#"{{ "id": "clip", "op": {{ "type": "clip" }} }},
           {{ "id": "image", "op": {{ "type": "media", "source_id": "video" }} }}, {}"#,
        effect_node("fx", "test.input-size", "clip", r#"{ "map": "image" }"#)
    );
    let project = project(&nodes, "fx", "", "");
    let needs = project.frame_needs(MediaTime(90_000)).expect("needs");
    assert_eq!(
        needs
            .iter()
            .map(|need| need.source_id.as_str())
            .collect::<Vec<_>>(),
        ["video"]
    );
    let drawn = render(&renderer, &project, 90_000, &frames);
    let image = read_rgba8(&renderer, &drawn.texture.expect("complete"));
    assert_eq!(pixel(&image, 0, 0), [2, 1, 0, 255]);
    let missing = render(&renderer, &project, 90_000, &MissingFrames);
    assert!(missing.texture.is_none());
    assert_eq!(
        missing.report.missing_sources,
        [SourceId::new("video").expect("id")]
    );
}

#[test]
fn a_bypassed_effect_does_not_need_its_named_inputs() {
    let _lock = gpu_test_lock();
    let renderer = renderer_with_inputs();
    let nodes = format!(
        r#"{{ "id": "clip", "op": {{ "type": "clip" }} }},
           {{ "id": "image", "op": {{ "type": "media", "source_id": "video" }} }}, {}"#,
        effect_node("fx", "test.multiply-map", "clip", r#"{ "map": "image" }"#).replacen(
            r#""id": "fx","#,
            r#""id": "fx", "bypass": true,"#,
            1
        )
    );
    let project = project(&nodes, "fx", "", "");
    assert_eq!(project.frame_needs(MediaTime(90_000)).expect("needs"), []);
    assert_eq!(
        pixel(&pixels(&renderer, &project, 90_000), 0, 0),
        [255, 255, 255, 255]
    );
}

#[test]
fn an_unconnected_named_input_makes_the_composition_incomplete() {
    let nodes = format!(
        r#"{{ "id": "clip", "op": {{ "type": "clip" }} }}, {}"#,
        effect_node("fx", "test.multiply-map", "clip", r#"{ "map": null }"#)
    );
    let project = project(&nodes, "fx", "", "");
    let error = project
        .evaluate(MediaTime(90_000))
        .expect_err("the map is needed");
    assert_eq!(
        crate::ErrorCode::of(&error),
        Some(crate::ErrorCode::IncompleteComposition)
    );
    match error {
        alpha2_eval::EvalError::IncompleteComposition { reason, .. } => assert_eq!(
            reason,
            alpha2_eval::PlanError::MissingInput {
                node: alpha2_model::NodeId::new("fx").expect("id"),
                input: "inputs.map".into(),
            }
        ),
        other => panic!("expected an incomplete composition, got {other:?}"),
    }
}

#[test]
fn a_named_input_the_definition_does_not_declare_fails_the_frame() {
    let _lock = gpu_test_lock();
    let renderer = renderer_with_inputs();
    let frame_error = |nodes: String| {
        let project = project(&nodes, "fx", "", "");
        let frame = project.evaluate(MediaTime(90_000)).expect("evaluates");
        let error = renderer
            .render_frame(RenderSize::new(W, H).expect("size"), &frame, &MissingFrames)
            .expect_err("the frame fails");
        crate::ErrorCode::of(&error)
    };
    let red = r#"{ "id": "red", "op": { "type": "media", "source_id": "red" } }"#;
    // The tint declares no input; the map declares one the node leaves out.
    assert_eq!(
        frame_error(format!(
            r#"{{ "id": "clip", "op": {{ "type": "clip" }} }}, {red}, {}"#,
            effect_node("fx", "test.tint", "clip", r#"{ "map": "red" }"#)
        )),
        Some(crate::ErrorCode::EffectError)
    );
    assert_eq!(
        frame_error(format!(
            r#"{{ "id": "clip", "op": {{ "type": "clip" }} }}, {red}, {}"#,
            effect_node("fx", "test.multiply-map", "clip", r#"{ "mask": "red" }"#)
        )),
        Some(crate::ErrorCode::EffectError)
    );
}

/// The bloom reference's three passes, as nodes reading `source`: the bright parts, blurred,
/// added back over the source by name.
fn bloom_nodes(source: &str, radius: f32, intensity: f32) -> String {
    format!(
        r#"{{ "id": "bright", "op": {{ "type": "effect", "input": "{source}", "effect": {{ "id": "test.bright-pass", "version": 1 }} }} }},
           {{ "id": "blur", "op": {{ "type": "effect", "input": "bright", "effect": {{ "id": "alpha2.effect.gaussian-blur", "version": 1,
             "params": {{ "radius": {{ "mode": "constant", "value": {{ "type": "float", "value": {radius} }} }} }} }} }} }},
           {{ "id": "combine", "op": {{ "type": "effect", "input": "{source}", "inputs": {{ "glow": "blur" }},
             "effect": {{ "id": "test.add-glow", "version": 1,
             "params": {{ "intensity": {{ "mode": "constant", "value": {{ "type": "float", "value": {intensity} }} }} }} }} }} }}"#
    )
}

#[test]
fn a_multi_pass_group_draws_what_its_passes_draw_inline() {
    // Bloom with no engine primitive of its own: a group of three effect nodes, the last
    // reading the blurred bright parts as a named input, its settings exposed as controls.
    let _lock = gpu_test_lock();
    let renderer = renderer_with_inputs();
    let inline = |intensity: f32| {
        project_with(
            &format!(
                r#"{{ "id": "clip", "op": {{ "type": "clip" }} }}, {}"#,
                bloom_nodes("clip", 0.5, intensity)
            ),
            "combine",
            "",
            "",
            "",
            "",
            "left",
        )
    };
    let definition = format!(
        r#", {{ "id": "bloom", "inputs": ["Source"],
              "controls": [
                {{ "name": "threshold", "node": "bright", "param": "threshold" }},
                {{ "name": "radius", "node": "blur", "param": "radius" }},
                {{ "name": "intensity", "node": "combine", "param": "intensity" }} ],
              "nodes": [{{ "id": "source", "op": {{ "type": "group_input", "index": 0 }} }}, {}],
              "output": "combine" }}"#,
        bloom_nodes("source", 0.012, 1.0)
    );
    let grouped = |intensity: f32| {
        project_with(
            &format!(
                r#"{{ "id": "clip", "op": {{ "type": "clip" }} }},
                   {{ "id": "bloom", "op": {{ "type": "group", "composition": "bloom", "inputs": ["clip"],
                     "controls": {{
                       "radius": {{ "mode": "constant", "value": {{ "type": "float", "value": 0.5 }} }},
                       "intensity": {{ "mode": "constant", "value": {{ "type": "float", "value": {intensity} }} }} }} }} }}"#
            ),
            "bloom",
            "",
            "",
            &definition,
            "",
            "left",
        )
    };
    let glowing = pixels(&renderer, &inline(2.0), 120_000);
    assert_eq!(pixels(&renderer, &grouped(2.0), 120_000), glowing);
    // The white left half stays white, and its glow reaches the transparent right half.
    assert_eq!(pixel(&glowing, 0, 0), [255, 255, 255, 255]);
    assert!(pixel(&glowing, 3, 0)[0] > 0, "{:?}", pixel(&glowing, 3, 0));
    // Without intensity there is no glow: the picture is the source's.
    let plain = pixels(&renderer, &grouped(0.0), 120_000);
    assert_eq!(pixels(&renderer, &inline(0.0), 120_000), plain);
    assert_eq!(pixel(&plain, 3, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&plain, 0, 0), [255, 255, 255, 255]);
}

#[test]
fn a_displacement_map_moves_the_picture() {
    // A red map offsets every sample by (+0.5, -0.5) * amount: a quarter of the width at 0.5,
    // which moves the left half's edge left by one pixel of four.
    let _lock = gpu_test_lock();
    let renderer = renderer_with_inputs();
    let displaced = |amount: f32| {
        let nodes = format!(
            r#"{{ "id": "clip", "op": {{ "type": "clip" }} }},
               {{ "id": "noise", "op": {{ "type": "media", "source_id": "red" }} }},
               {{ "id": "warp", "op": {{ "type": "effect", "input": "clip", "inputs": {{ "map": "noise" }},
                 "effect": {{ "id": "test.displace", "version": 1,
                   "params": {{ "amount": {{ "mode": "constant", "value": {{ "type": "float", "value": {amount} }} }} }} }} }} }}"#
        );
        pixels(
            &renderer,
            &project_with(&nodes, "warp", "", "", "", "", "left"),
            120_000,
        )
    };
    let still = displaced(0.0);
    assert_eq!(pixel(&still, 1, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&still, 2, 0), [0, 0, 0, 255]);
    let moved = displaced(0.5);
    assert_eq!(pixel(&moved, 0, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&moved, 1, 0), [0, 0, 0, 255]);
}
