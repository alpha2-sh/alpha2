//! Text boxes drawn end to end: their shape, a box that follows a reveal, and a highlight.

use super::captions_and_text::text_project;
use super::*;

fn fixture_texts() -> crate::TextTextures {
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

/// A caption track at height `y` with one project-long cue of `text` in `style`.
fn track(id: &str, y: f32, style: &str, text: &str) -> String {
    format!(
        r#", {{ "id": "{id}", "name": "{id}", "kind": "caption", "captions": {{
            "style": {{ "font_size": 0.08, "position": {{ "x": 0.5, "y": {y} }},
                        "anchor": {{ "x": 0.5, "y": 0.5 }}, "max_width": 0.9 {style} }},
            "cues": [ {{ "timeline_range": {{ "start": 0, "end": 240000 }}, "text": {text} }} ] }} }}"#
    )
}

fn boxes_project() -> crate::EngineProject {
    let rounded = track(
        "rounded",
        0.22,
        r##", "background": { "color": "#1e2a60e0", "padding": "0.15 0.4", "radius": 0.5,
              "border": { "width": 0.1, "color": "#ffd040", "dash": { "pattern": [0.4, 0.25] } } }"##,
        r#""Rounded boxes\nover two lines""#,
    );
    let typed = track(
        "typed",
        0.58,
        r##", "background": { "color": "#000000c0", "padding": 0.2, "radius": 0.2, "extent": "revealed" },
            "animators": [ { "selector": { "type": "range",
                "start": { "mode": "keyframes", "keyframes": [
                    { "time": 0, "value": 0, "interpolation": { "type": "linear" } },
                    { "time": 240000, "value": 100, "interpolation": { "type": "hold" } } ] } },
              "properties": { "opacity": { "mode": "constant", "value": 0 } } } ]"##,
        r#""Typewriter box""#,
    );
    let said = track(
        "said",
        0.85,
        r##", "highlight": { "color": "#e04020", "padding": "0.05 0.15", "radius": 0.3, "transition": 24000 }"##,
        r#"[ { "text": "one ", "time": { "start": 0, "end": 60000 } },
             { "text": "two ", "time": { "start": 60000, "end": 150000 } },
             { "text": "three", "time": { "start": 150000, "end": 240000 } } ]"#,
    );
    text_project("", "", &format!("{rounded}{typed}{said}"))
}

const SIZE: (u32, u32) = (320, 180);
/// Half way through the slide from `one` to `two`, and about a third of the way through typing.
const AT: i64 = 72_000;

#[test]
fn text_boxes_golden() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    let mut texts = fixture_texts();
    let project = boxes_project();
    let size = RenderSize::new(SIZE.0, SIZE.1).expect("size");
    let frame = project.evaluate(MediaTime(AT)).expect("frame");
    texts.prepare(&renderer, &frame, size).expect("text");
    let output = renderer
        .render_frame(
            size,
            &frame,
            &crate::FramesWithText::new(&CaptionFrames(&texts), &texts),
        )
        .expect("frame renders");
    let pixels = read_rgba8(&renderer, &output.texture.expect("complete"));
    assert_golden("text_boxes", SIZE, &pixels, 2);
}

#[test]
fn text_boxes_draw_the_same_on_the_cpu() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    let project = boxes_project();
    let size = RenderSize::new(SIZE.0, SIZE.1).expect("size");
    let frame = project.evaluate(MediaTime(AT)).expect("frame");
    let mut texts = fixture_texts();
    texts.prepare(&renderer, &frame, size).expect("text");
    let output = renderer
        .render_frame(
            size,
            &frame,
            &crate::FramesWithText::new(&CaptionFrames(&texts), &texts),
        )
        .expect("frame renders");
    let gpu = read_rgba8(&renderer, &output.texture.expect("complete"));
    let cpu_renderer = alpha2_render::CpuRenderer::new(2);
    let mut cpu_texts = fixture_texts();
    cpu_texts
        .prepare(&cpu_renderer, &frame, size)
        .expect("text");
    let cpu = cpu_renderer
        .render_frame(size, &frame, &CaptionFrames(&cpu_texts))
        .expect("frame renders")
        .texture
        .expect("complete")
        .read_rgba8();
    let largest = gpu
        .iter()
        .zip(&cpu)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(largest <= 2, "CPU and GPU differ by up to {largest}");
}
