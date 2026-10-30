//! Text animators: karaoke by run times, motion that settles, the CPU
//! renderer, and a strip of an animation's frames.

use super::captions_and_text::text_project;
use super::*;

fn builtin_renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    renderer
}

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

/// A caption track whose one cue, the whole project long, holds `text` (a
/// JSON string or runs) in `style`.
fn caption_track(style: &str, text: &str) -> String {
    format!(
        r#", {{ "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {{
            "style": {style},
            "cues": [ {{ "timeline_range": {{ "start": 0, "end": 240000 }}, "text": {text} }} ] }} }}"#
    )
}

/// The frame of `project` at `ticks` on the GPU, at 160x90.
fn gpu_frame(
    renderer: &Renderer,
    texts: &mut crate::TextTextures,
    project: &crate::EngineProject,
    ticks: i64,
) -> Vec<u8> {
    let size = RenderSize::new(160, 90).expect("size");
    let frame = project.evaluate(MediaTime(ticks)).expect("frame");
    texts.prepare(renderer, &frame, size).expect("text");
    let output = renderer
        .render_frame(
            size,
            &frame,
            &crate::FramesWithText::new(&CaptionFrames(texts), texts),
        )
        .expect("frame renders");
    read_rgba8(renderer, &output.texture.expect("complete"))
}

/// Two timed words, each half a second; the said word turns red.
const KARAOKE: &str = r##"{ "font_size": 0.2, "position": { "x": 0.5, "y": 0.5 }, "anchor": { "x": 0.5, "y": 0.5 },
    "animators": [ { "selector": { "type": "timing", "state": "active" },
                     "properties": { "fill": { "mode": "constant", "value": "#ff0000" } } } ] }"##;
const KARAOKE_RUNS: &str = r#"[ { "text": "one ", "time": { "start": 0, "end": 60000 } },
                                 { "text": "two", "time": { "start": 60000, "end": 120000 } } ]"#;

/// The mean column of `pixels`' red pixels, 160 wide, if any.
fn red_centre(pixels: &[u8]) -> Option<f64> {
    let columns = pixels
        .chunks(4)
        .enumerate()
        .filter(|(_, pixel)| pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60)
        .map(|(index, _)| index % 160)
        .collect::<Vec<_>>();
    #[allow(clippy::cast_precision_loss)]
    (!columns.is_empty()).then(|| columns.iter().sum::<usize>() as f64 / columns.len() as f64)
}

#[test]
fn a_karaoke_cue_highlights_its_active_word() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let mut texts = fixture_texts();
    let project = text_project("", "", &caption_track(KARAOKE, KARAOKE_RUNS));
    // A quarter and three quarters of a second into the cue, then past both words.
    let first = red_centre(&gpu_frame(&renderer, &mut texts, &project, 30_000)).expect("red");
    let second = red_centre(&gpu_frame(&renderer, &mut texts, &project, 90_000)).expect("red");
    assert!(first < 80.0 && second > 80.0, "{first} then {second}");
    assert_eq!(
        red_centre(&gpu_frame(&renderer, &mut texts, &project, 150_000)),
        None
    );
    // The switch happens at the run boundary, between the frames either side of it.
    let before = red_centre(&gpu_frame(&renderer, &mut texts, &project, 59_999)).expect("red");
    let after = red_centre(&gpu_frame(&renderer, &mut texts, &project, 60_000)).expect("red");
    assert!(before < 80.0 && after > 80.0, "{before} then {after}");
}

/// A title whose characters rise into place over the first second, then
/// hold.
const RISE: &str = r##"{ "font_size": 0.2, "position": { "x": 0.5, "y": 0.5 }, "anchor": { "x": 0.5, "y": 0.5 },
    "strokes": [ { "color": "#000000", "width": 0.08 } ],
    "animators": [ { "selector": { "type": "range", "shape": "ramp_up",
                                   "offset": { "mode": "keyframes", "keyframes": [
                                       { "time": 0, "value": -100, "interpolation": { "type": "linear" } },
                                       { "time": 120000, "value": 100, "interpolation": { "type": "hold" } } ] } },
                     "properties": { "offset": { "mode": "constant", "value": { "x": 0, "y": 0.6 } },
                                     "opacity": { "mode": "constant", "value": 0 },
                                     "scale": { "mode": "constant", "value": { "x": 1.6, "y": 1.6 } } } } ] }"##;

#[test]
fn a_settled_animation_hits_the_result_cache() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let mut texts = fixture_texts();
    let project = text_project("", "", &caption_track(RISE, r#""Rise""#));
    let size = RenderSize::new(160, 90).expect("size");
    let key_at = |texts: &mut crate::TextTextures, ticks: i64| {
        let frame = project.evaluate(MediaTime(ticks)).expect("frame");
        texts.prepare(&renderer, &frame, size).expect("text");
        let caption = &frame.captions[0];
        texts
            .get(alpha2_layout::animated_text_key(
                caption.text,
                caption.style,
                crate::text::picture(size),
                caption.local_time,
            ))
            .expect("prepared")
            .cache_key
    };
    // Moving, each frame is a picture of its own; settled, every frame is the one picture, which
    // the renderer's result cache finds again.
    let moving = [key_at(&mut texts, 30_000), key_at(&mut texts, 60_000)];
    assert_ne!(moving[0], moving[1]);
    let settled = [key_at(&mut texts, 150_000), key_at(&mut texts, 200_000)];
    assert_eq!(settled[0], settled[1]);
    assert_ne!(settled[0], moving[1]);
    // Settled, the text draws as the same style without animators.
    let still = text_project(
        "",
        "",
        &caption_track(
            r##"{ "font_size": 0.2, "position": { "x": 0.5, "y": 0.5 }, "anchor": { "x": 0.5, "y": 0.5 },
                  "strokes": [ { "color": "#000000", "width": 0.08 } ] }"##,
            r#""Rise""#,
        ),
    );
    assert_eq!(
        gpu_frame(&renderer, &mut texts, &project, 200_000),
        gpu_frame(&renderer, &mut fixture_texts(), &still, 200_000)
    );
}

#[test]
fn animated_text_draws_the_same_on_the_cpu() {
    let _lock = gpu_test_lock();
    let project = text_project("", "", &caption_track(RISE, r#""Rise""#));
    let size = RenderSize::new(160, 90).expect("size");
    let gpu = gpu_frame(&builtin_renderer(), &mut fixture_texts(), &project, 45_000);
    let cpu_renderer = alpha2_render::CpuRenderer::new(2);
    let mut texts = fixture_texts();
    let frame = project.evaluate(MediaTime(45_000)).expect("frame");
    texts.prepare(&cpu_renderer, &frame, size).expect("text");
    let cpu = cpu_renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
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

#[test]
fn text_animator_frames_golden() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let mut texts = fixture_texts();
    let project = text_project("", "", &caption_track(RISE, r#""Rise""#));
    // Four moments of the rise, side by side.
    let frames =
        [0, 40_000, 80_000, 120_000].map(|ticks| gpu_frame(&renderer, &mut texts, &project, ticks));
    let strip = (0..90)
        .flat_map(|row| {
            frames
                .iter()
                .flat_map(move |pixels| pixels[row * 640..(row + 1) * 640].iter().copied())
        })
        .collect::<Vec<_>>();
    assert_golden("text_animators", (640, 90), &strip, 2);
}

/// A project with a layout generator source whose one text node has
/// `animators`.
fn animated_layout(animators: &str) -> crate::EngineProject {
    let source = format!(
        r##"{{ "id": "title", "kind": "generated", "generator": {{ "type": "layout", "root": {{
            "justify_content": "center", "align_items": "center", "fill": "#ffffff",
            "children": [ {{ "text": {{ "mode": "constant", "value": "Layout" }},
                             "font_size": {{ "mode": "constant", "value": 0.2 }},
                             "text_animators": {animators} }} ] }} }} }},"##
    );
    text_project(
        &source,
        "",
        r#", { "id": "titles", "name": "Titles", "kind": "video", "clips": [
            { "id": "t", "source_id": "title", "timeline_range": { "start": 0, "end": 240000 },
              "source_range": { "start": 0, "end": 240000 } } ] }"#,
    )
}

#[test]
fn a_layout_text_node_animates_its_characters() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let still = gpu_frame(
        &renderer,
        &mut fixture_texts(),
        &animated_layout("[]"),
        60_000,
    );
    // Every character dropped and turned: the text moves, the layout does not.
    let moved = gpu_frame(
        &renderer,
        &mut fixture_texts(),
        &animated_layout(
            r#"[ { "selector": { "type": "range" },
                   "properties": { "offset": { "mode": "constant", "value": { "x": 0, "y": 0.4 } },
                                   "rotation_degrees": { "mode": "constant", "value": 10 } } } ]"#,
        ),
        60_000,
    );
    assert_ne!(moved, still);
    // A range that selects nothing draws the text as it is.
    let unselected = gpu_frame(
        &renderer,
        &mut fixture_texts(),
        &animated_layout(
            r#"[ { "selector": { "type": "range", "start": { "mode": "constant", "value": 50 },
                                 "end": { "mode": "constant", "value": 50 } },
                   "properties": { "opacity": { "mode": "constant", "value": 0 } } } ]"#,
        ),
        60_000,
    );
    assert_eq!(unselected, still);
}
