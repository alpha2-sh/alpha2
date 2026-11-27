//! Adjustment clips: a composition applied to the scene composited beneath it.

use super::*;
use crate::EngineProject;
use alpha2_render::{CacheEligibility, CpuRenderer, RenderNodeKind};

const W: u32 = 64;
const H: u32 = 32;
const AT: i64 = 60_000;

fn size() -> RenderSize {
    RenderSize::new(W, H).expect("size")
}

fn clip(id: &str, source: &str, extra: &str) -> String {
    format!(
        r#"{{ "id": "{id}", "source_id": "{source}", "timeline_range": {{ "start": 0, "end": 240000 }},
            "source_range": {{ "start": 0, "end": 240000 }} {extra} }}"#
    )
}

fn track(id: &str, clips: &[String], transitions: &str) -> String {
    format!(
        r#"{{ "id": "{id}", "name": "{id}", "kind": "video", "clips": [{}], "transitions": [{transitions}] }}"#,
        clips.join(", ")
    )
}

fn white_left_over_red() -> Vec<String> {
    vec![
        track("low", &[clip("red", "red", "")], ""),
        track("mid", &[clip("left", "left", "")], ""),
    ]
}

fn adjustment(extra: &str) -> String {
    track("adj", &[clip("adjust", "adjust", extra)], "")
}

fn over(tracks: &[String], top: &[String]) -> Vec<String> {
    tracks.iter().chain(top).cloned().collect()
}

const INVERT: &str = r#", "composition": { "id": "invert" }"#;

fn effect_node(id: &str, input: &str, effect: &str, params: &str) -> String {
    format!(
        r#"{{ "id": "{id}", "op": {{ "type": "effect", "input": "{input}",
            "effect": {{ "id": "{effect}", "version": 1, "params": {{ {params} }} }} }} }}"#
    )
}

fn compositions() -> String {
    let composition = |id: &str, nodes: &[String], output: &str| {
        format!(
            r#"{{ "id": "{id}", "nodes": [ {{ "id": "clip", "op": {{ "type": "clip" }} }}, {} ], "output": "{output}" }}"#,
            nodes.join(", ")
        )
    };
    let crop = r#""bottom_right": { "mode": "constant", "value": { "type": "vec2", "value": [0.5, 0] } },
        "feather": { "mode": "constant", "value": { "type": "float", "value": 0.0001 } }"#;
    let invert = effect_node("invert", "clip", "alpha2.effect.invert", "");
    let mut bypassed = invert.clone();
    bypassed.insert_str(1, r#" "bypass": true,"#);
    [
        composition("invert", std::slice::from_ref(&invert), "invert"),
        composition("bypassed", &[bypassed], "invert"),
        composition(
            "cropped",
            &[effect_node("crop", "clip", "alpha2.effect.crop", crop)],
            "crop",
        ),
        composition(
            "sepia",
            &[effect_node("sepia", "clip", "alpha2.effect.sepia", "")],
            "sepia",
        ),
        composition(
            "multiplied",
            &[
                invert,
                r#"{ "id": "merge", "op": { "type": "merge", "layers": [ { "input": "clip" },
                    { "input": "invert", "blend_mode": "multiply" } ] } }"#
                    .to_owned(),
            ],
            "merge",
        ),
        composition(
            "scaled",
            &[effect_node("scale", "clip", "test.scale", "")],
            "scale",
        ),
    ]
    .join(", ")
}

fn solid([red, green, blue]: [f32; 3]) -> String {
    format!(
        r#"{{ "type": "solid", "color": {{ "red": {red}, "green": {green}, "blue": {blue}, "alpha": 1 }} }}"#
    )
}

fn shape(id: &str, [x, y]: [f32; 2], [width, height]: [f32; 2], color: [f32; 3]) -> String {
    format!(
        r#"{{ "id": "{id}", "kind": "generated", "generator": {{ "type": "shapes", "items": [
            {{ "type": "rectangle", "center": {{ "mode": "constant", "value": {{ "x": {x}, "y": {y} }} }},
              "size": {{ "mode": "constant", "value": {{ "x": {width}, "y": {height} }} }} }},
            {{ "type": "fill", "paint": {{ "mode": "constant", "value": {} }} }} ] }} }}"#,
        solid(color)
    )
}

fn fill(id: &str, color: [f32; 3]) -> String {
    format!(
        r#"{{ "id": "{id}", "kind": "generated", "generator": {{ "type": "fill", "paint": {} }} }}"#,
        solid(color)
    )
}

fn project(tracks: &[String]) -> EngineProject {
    let sources = [
        fill("red", [1.0, 0.0, 0.0]),
        fill("blue", [0.0, 0.0, 1.0]),
        fill("grey", [0.4, 0.4, 0.4]),
        shape("left", [0.25, 0.5], [0.5, 1.0], [1.0, 1.0, 1.0]),
        shape("corner", [0.9375, 0.5], [0.125, 1.0], [0.0, 0.0, 1.0]),
        r#"{ "id": "adjust", "kind": "adjustment" }"#.to_owned(),
    ];
    let json = format!(
        r#"{{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": {{ "width": {W}, "height": {H}, "frame_rate": {{ "num": 30, "den": 1 }}, "sample_rate": 48000 }},
          "sources": [{}],
          "compositions": [{}],
          "timeline": {{ "tracks": [{}] }}
        }}"#,
        sources.join(", "),
        compositions(),
        tracks.join(", ")
    );
    let project = alpha2_model::Project::from_json(&json).expect("a valid project");
    EngineProject::new(project).expect("indexed")
}

fn texts() -> crate::TextTextures {
    let mut texts = crate::TextTextures::new();
    texts
        .add_font(
            "Alpha2 Fixture",
            std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font"),
            0,
        )
        .expect("font");
    texts
}

fn renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    pollster::block_on(renderer.register_effect(scale_effect())).expect("compiles");
    renderer
}

fn scale_effect() -> EffectDef {
    EffectDef {
        id: "test.scale".into(),
        params: Vec::new(),
        ..runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return vec4f(vec3f(render_scale() * 0.5), 1.0); }",
        )
    }
}

fn draw_at(renderer: &Renderer, project: &EngineProject, time: i64, size: RenderSize) -> Vec<u8> {
    let frame = project.evaluate(MediaTime(time)).expect("evaluates");
    let mut texts = texts();
    texts.prepare(renderer, &frame, size).expect("texts");
    let scope = gpu()
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
        .expect("frame renders");
    let error = pollster::block_on(scope.pop());
    assert!(error.is_none(), "{error:?}");
    read_rgba8(renderer, &output.texture.expect("a complete frame"))
}

fn draw(renderer: &Renderer, project: &EngineProject) -> Vec<u8> {
    draw_at(renderer, project, AT, size())
}

fn draw_on_cpu(project: &EngineProject, time: i64) -> Result<Vec<u8>, RenderError> {
    let renderer = CpuRenderer::new(2);
    let frame = project.evaluate(MediaTime(time)).expect("evaluates");
    let mut texts = texts();
    texts.prepare(&renderer, &frame, size()).expect("texts");
    Ok(renderer
        .render_frame(size(), &frame, &CaptionFrames(&texts))?
        .texture
        .expect("a complete frame")
        .read_rgba8())
}

fn at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let start = ((y * W + x) * 4) as usize;
    pixels[start..start + 4].try_into().expect("four bytes")
}

fn inverted(pixels: &[u8]) -> Vec<u8> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|[r, g, b, a]| [255 - r, 255 - g, 255 - b, *a])
        .collect()
}

fn caption_track() -> String {
    r#"{ "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {
        "style": { "font_size": 0.2, "position": { "x": 0.5, "y": 0.8 },
          "background": { "color": { "red": 0, "green": 0.6, "blue": 0, "alpha": 1 }, "padding": 0.1 } },
        "cues": [ { "timeline_range": { "start": 0, "end": 240000 }, "text": "Hi" } ] } }"#
        .to_owned()
}

#[test]
fn an_adjustment_applies_its_composition_to_everything_beneath_it() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let beneath = over(&white_left_over_red(), &[caption_track()]);
    let plain = draw(&renderer, &project(&beneath));
    assert_ne!(
        plain,
        draw(&renderer, &project(&white_left_over_red())),
        "the caption draws"
    );
    let adjusted = draw(&renderer, &project(&over(&beneath, &[adjustment(INVERT)])));
    assert_rgba8_near(&adjusted, &inverted(&plain), 1);
    let top = track("top", &[clip("corner", "corner", "")], "");
    let topped = draw(
        &renderer,
        &project(&over(&beneath, &[adjustment(INVERT), top])),
    );
    for y in 0..H {
        for x in 0..W {
            let expected = if x >= 56 {
                [0, 0, 255, 255]
            } else {
                at(&adjusted, x, y)
            };
            assert_eq!(at(&topped, x, y), expected, "at {x}, {y}");
        }
    }
    assert_rgba8_near(
        &draw_on_cpu(&project(&over(&beneath, &[adjustment(INVERT)])), AT).expect("cpu"),
        &adjusted,
        1,
    );
}

#[test]
fn an_adjustment_with_no_effect_draws_the_frame_without_it_byte_for_byte() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let top = track("top", &[clip("corner", "corner", "")], "");
    let without = over(&white_left_over_red(), std::slice::from_ref(&top));
    let expected = draw(&renderer, &project(&without));
    let expected_on_cpu = draw_on_cpu(&project(&without), AT).expect("cpu");
    let mask = r#", "masks": [ { "shape": { "type": "ellipse",
        "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
        "size": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } } } } ], "blend_mode": "multiply""#;
    for extra in [
        String::new(),
        mask.to_owned(),
        r#", "composition": { "id": "bypassed" }"#.to_owned(),
        format!(r#"{INVERT}, "visual": {{ "opacity": {{ "mode": "constant", "value": 0 }} }}"#),
    ] {
        let tracks = over(&white_left_over_red(), &[adjustment(&extra), top.clone()]);
        assert_eq!(draw(&renderer, &project(&tracks)), expected, "{extra}");
        assert_eq!(
            draw_on_cpu(&project(&tracks), AT).expect("cpu"),
            expected_on_cpu,
            "{extra}"
        );
    }
}

#[test]
fn masks_and_opacity_mix_the_adjusted_scene_with_the_backdrop() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let masked = r#", "masks": [ { "shape": { "type": "rectangle",
        "center": { "mode": "constant", "value": { "x": 0.25, "y": 0.5 } },
        "size": { "mode": "constant", "value": { "x": 0.5, "y": 1 } } } } ]"#;
    let red = [track("low", &[clip("red", "red", "")], "")];
    let full = draw(
        &renderer,
        &project(&over(&red, &[adjustment(&format!("{INVERT}{masked}"))])),
    );
    let half = draw(
        &renderer,
        &project(&over(
            &red,
            &[adjustment(&format!(
                r#"{INVERT}{masked}, "visual": {{ "opacity": {{ "mode": "constant", "value": 0.5 }} }}"#
            ))],
        )),
    );
    for y in 0..H {
        assert_eq!(at(&full, 8, y), [0, 255, 255, 255], "inverted inside");
        assert_rgba8_near(&at(&half, 8, y), &[128, 128, 128, 255], 1);
        for pixels in [&full, &half] {
            assert_eq!(at(pixels, 48, y), [255, 0, 0, 255], "untouched outside");
        }
    }
}

#[test]
fn the_adjustment_transform_moves_its_coverage_not_the_backdrop() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let placed = format!(
        r#"{INVERT}, "visual": {{ "position": {{ "mode": "constant", "value": {{ "x": 0.25, "y": 0.5 }} }},
            "scale": {{ "mode": "constant", "value": {{ "x": 0.5, "y": 1 }} }} }}"#
    );
    let pixels = draw(
        &renderer,
        &project(&over(&white_left_over_red(), &[adjustment(&placed)])),
    );
    for y in 0..H {
        for x in [2, 12, 24, 29] {
            assert_eq!(at(&pixels, x, y), [0, 0, 0, 255], "inverted white at {x}");
        }
        for x in [34, 48, 62] {
            assert_eq!(at(&pixels, x, y), [255, 0, 0, 255], "red at {x}");
        }
    }
}

#[test]
fn a_normal_adjustment_replaces_the_backdrop_alpha_included() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let red = [track("low", &[clip("red", "red", "")], "")];
    let cropped = r#", "composition": { "id": "cropped" }"#;
    let full = draw(&renderer, &project(&over(&red, &[adjustment(cropped)])));
    let faded = draw(
        &renderer,
        &project(&over(
            &red,
            &[adjustment(&format!(
                r#"{cropped}, "visual": {{ "opacity": {{ "mode": "constant", "value": 0.75 }} }}"#
            ))],
        )),
    );
    for y in 0..H {
        assert_eq!(at(&full, 8, y), [255, 0, 0, 255]);
        assert_eq!(
            at(&full, 48, y),
            [0, 0, 0, 255],
            "the background shows where the crop took the scene"
        );
        assert_eq!(at(&faded, 8, y), [255, 0, 0, 255]);
        assert_rgba8_near(&at(&faded, 48, y), &[137, 0, 0, 255], 1);
    }
}

#[test]
fn an_artistic_blend_adjustment_meets_the_backdrop_by_the_w3c_formula() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let blended = |mode: &str| {
        draw(
            &renderer,
            &project(&over(
                &white_left_over_red(),
                &[adjustment(&format!(r#"{INVERT}, "blend_mode": "{mode}""#))],
            )),
        )
    };
    let multiplied = blended("multiply");
    let screened = blended("screen");
    for y in 0..H {
        for x in [4, 48] {
            assert_eq!(at(&multiplied, x, y), [0, 0, 0, 255], "multiply at {x}");
            assert_eq!(at(&screened, x, y), [255, 255, 255, 255], "screen at {x}");
        }
    }
    let grey = [track("low", &[clip("grey", "grey", "")], "")];
    let by_mode = draw(
        &renderer,
        &project(&over(
            &grey,
            &[adjustment(&format!(
                r#"{INVERT}, "blend_mode": "multiply""#
            ))],
        )),
    );
    let by_merge = draw(
        &renderer,
        &project(&over(
            &grey,
            &[adjustment(r#", "composition": { "id": "multiplied" }"#)],
        )),
    );
    assert_eq!(by_mode, by_merge);
    assert_ne!(
        at(&by_mode, 0, 0),
        at(&draw(&renderer, &project(&grey)), 0, 0)
    );
}

#[test]
fn a_transition_beneath_an_adjustment_is_adjusted_after_it_folds() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let wiped = track(
        "low",
        &[
            r#"{ "id": "red", "source_id": "red", "timeline_range": { "start": 0, "end": 120000 },
                "source_range": { "start": 0, "end": 120000 } }"#
                .to_owned(),
            r#"{ "id": "blue", "source_id": "blue", "timeline_range": { "start": 0, "end": 240000 },
                "source_range": { "start": 0, "end": 240000 } }"#
                .to_owned(),
        ],
        r#"{ "from_clip": "red", "to_clip": "blue", "timeline_range": { "start": 0, "end": 120000 },
            "easing": { "type": "linear" }, "effect": { "id": "alpha2.transition.wipe-left", "version": 1 } }"#,
    );
    let plain = draw(&renderer, &project(std::slice::from_ref(&wiped)));
    assert!(
        plain.as_chunks::<4>().0.contains(&[255, 0, 0, 255])
            && plain.as_chunks::<4>().0.contains(&[0, 0, 255, 255]),
        "both sides of the wipe show"
    );
    let adjusted = draw(&renderer, &project(&[wiped, adjustment(INVERT)]));
    assert_rgba8_near(&adjusted, &inverted(&plain), 1);
}

fn fades_into(first: &str, second: &str) -> (EngineProject, f32) {
    let faded = track(
        "adj",
        &[
            format!(
                r#"{{ "id": "first", "source_id": "{}", "timeline_range": {{ "start": 0, "end": 120000 }},
                    "source_range": {{ "start": 0, "end": 120000 }} {} }}"#,
                first.split('|').next().expect("source"),
                first.split('|').nth(1).unwrap_or_default()
            ),
            format!(
                r#"{{ "id": "second", "source_id": "{}", "timeline_range": {{ "start": 0, "end": 240000 }},
                    "source_range": {{ "start": 0, "end": 240000 }} {} }}"#,
                second.split('|').next().expect("source"),
                second.split('|').nth(1).unwrap_or_default()
            ),
        ],
        r#"{ "from_clip": "first", "to_clip": "second", "timeline_range": { "start": 0, "end": 120000 },
            "easing": { "type": "linear" }, "effect": { "id": "alpha2.transition.fade", "version": 1 } }"#,
    );
    let project = project(&[track("low", &[clip("red", "red", "")], ""), faded]);
    let progress = project
        .evaluate(MediaTime(AT))
        .expect("evaluates")
        .transitions[0]
        .progress;
    (project, progress)
}

fn mixed(from: &[u8], to: &[u8], progress: f32) -> Vec<u8> {
    from.iter()
        .zip(to)
        .map(|(&from, &to)| {
            let value = f32::from(from) * (1.0 - progress) + f32::from(to) * progress;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let value = value.round() as u8;
            value
        })
        .collect()
}

#[test]
fn a_fade_with_an_adjustment_clip_mixes_the_composed_scenes() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let red = [track("low", &[clip("red", "red", "")], "")];
    let alone = |extra: &[String]| draw(&renderer, &project(&over(&red, extra)));
    let picture = alone(&[track("adj", &[clip("left", "left", "")], "")]);
    let inverted = alone(&[adjustment(INVERT)]);
    let sepia = alone(&[adjustment(r#", "composition": { "id": "sepia" }"#)]);
    let (into, progress) = fades_into("left", &format!("adjust|{INVERT}"));
    assert!(progress > 0.2 && progress < 0.8, "{progress}");
    assert_rgba8_near(
        &draw(&renderer, &into),
        &mixed(&picture, &inverted, progress),
        1,
    );
    let (between, progress) = fades_into(
        &format!("adjust|{INVERT}"),
        r#"adjust|, "composition": { "id": "sepia" }"#,
    );
    assert_rgba8_near(
        &draw(&renderer, &between),
        &mixed(&inverted, &sepia, progress),
        1,
    );
    let (unchanged, progress) = fades_into("left", "adjust|");
    assert_rgba8_near(
        &draw(&renderer, &unchanged),
        &mixed(&picture, &alone(&[]), progress),
        1,
    );
}

#[test]
fn the_backdrop_is_drawn_once_for_the_effect_and_the_mix() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let project = project(&over(
        &white_left_over_red(),
        &[adjustment(&format!(
            r#"{INVERT}, "visual": {{ "opacity": {{ "mode": "constant", "value": 0.5 }} }}"#
        ))],
    ));
    let frame = project.evaluate(MediaTime(AT)).expect("evaluates");
    let built = renderer
        .build_frame_graph(size(), &frame, &MissingFrames)
        .expect("builds");
    let graph = built.graph.expect("complete");
    let plan = graph.compile().expect("compiles");
    let kinds = plan
        .execution_order()
        .iter()
        .map(|&node| graph.node_kind(node).expect("a node"))
        .collect::<Vec<_>>();
    let count = |kind| kinds.iter().filter(|&&other| other == kind).count();
    assert_eq!(
        (
            count(RenderNodeKind::Composite),
            count(RenderNodeKind::Effect)
        ),
        (2, 2),
        "the scene beneath, the invert, the mix and the frame: {kinds:?}"
    );
}

#[test]
fn an_adjustment_result_is_found_again_until_something_beneath_changes() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    renderer.set_result_cache_budget(1 << 24);
    let fading = |id: &str, source: &str, from: i64| {
        clip(
            id,
            source,
            &format!(
                r#", "visual": {{ "opacity": {{ "mode": "keyframes", "keyframes": [
                    {{ "time": {from}, "value": 0, "interpolation": {{ "type": "linear" }} }},
                    {{ "time": {}, "value": 1, "interpolation": {{ "type": "hold" }} }} ] }} }}"#,
                from + 60_000
            ),
        )
    };
    let tracks = [
        track("low", &[clip("red", "red", "")], ""),
        track("mid", &[fading("left", "left", 120_000)], ""),
        adjustment(&format!(
            r#"{INVERT}, "visual": {{ "opacity": {{ "mode": "constant", "value": 0.5 }} }}"#
        )),
        track("top", &[fading("corner", "corner", 0)], ""),
    ];
    let project = project(&tracks);
    let drawn = |time| {
        let before = renderer.diagnostics().graph_nodes;
        draw_at(&renderer, &project, time, size());
        renderer.diagnostics().graph_nodes - before
    };
    drawn(20_000);
    let hits = renderer.result_cache_stats().hits;
    let above_changed = drawn(40_000);
    assert!(renderer.result_cache_stats().hits > hits);
    drawn(140_000);
    let beneath_changed = drawn(160_000);
    assert!(
        beneath_changed >= above_changed + 3,
        "the scene beneath, its invert and the mix are drawn again: {above_changed} then {beneath_changed}"
    );
}

#[test]
fn a_bypassed_adjustment_keeps_the_scene_beneath_live() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let eligibility = |cache: &str| {
        let project = project(&over(
            &white_left_over_red(),
            &[adjustment(&format!(r#"{INVERT}, "cache": "{cache}""#))],
        ));
        let frame = project.evaluate(MediaTime(AT)).expect("evaluates");
        let built = renderer
            .build_frame_graph(size(), &frame, &MissingFrames)
            .expect("builds");
        let graph = built.graph.expect("complete");
        let plan = graph.compile().expect("compiles");
        plan.execution_order()
            .iter()
            .map(|&node| graph.cache_eligibility(node).expect("a node"))
            .collect::<Vec<_>>()
    };
    let bypassed = eligibility("bypass");
    assert!(
        bypassed
            .iter()
            .all(|&state| state == CacheEligibility::Bypassed),
        "{bypassed:?}"
    );
    let auto = eligibility("auto");
    assert!(
        auto.iter()
            .all(|&state| state == CacheEligibility::Eligible),
        "{auto:?}"
    );
}

#[test]
fn the_cpu_renderer_draws_an_adjustment_as_the_gpu_does() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let adjusted = format!(
        r#"{INVERT}, "blend_mode": "screen", "masks": [ {{ "shape": {{ "type": "ellipse",
            "center": {{ "mode": "constant", "value": {{ "x": 0.5, "y": 0.5 }} }},
            "size": {{ "mode": "constant", "value": {{ "x": 0.6, "y": 0.8 }} }} }},
            "feather": {{ "mode": "constant", "value": 0.1 }} }} ],
            "visual": {{ "opacity": {{ "mode": "constant", "value": 0.7 }},
              "position": {{ "mode": "constant", "value": {{ "x": 0.55, "y": 0.5 }} }},
              "rotation_degrees": {{ "mode": "constant", "value": 10 }} }}"#
    );
    let project = project(&over(
        &over(&white_left_over_red(), &[caption_track()]),
        &[adjustment(&adjusted)],
    ));
    let gpu = draw(&renderer, &project);
    let cpu = draw_on_cpu(&project, AT).expect("cpu");
    assert_rgba8_near(&cpu, &gpu, 2);
}

#[test]
fn pixel_sized_effects_in_an_adjustment_use_project_pixels() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let project = project(&over(
        &white_left_over_red(),
        &[adjustment(r#", "composition": { "id": "scaled" }"#)],
    ));
    let full = draw(&renderer, &project);
    let half = draw_at(
        &renderer,
        &project,
        AT,
        RenderSize::new(W / 2, H / 2).expect("size"),
    );
    assert_rgba8_near(&full[..4], &[128, 128, 128, 255], 1);
    assert_rgba8_near(&half[..4], &[64, 64, 64, 255], 1);
    assert_eq!(draw_on_cpu(&project, AT), Err(RenderError::UnknownEffect));
}
