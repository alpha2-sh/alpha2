//! Rich text: runs with their own size, face, paint and decorations, drawn end to end.

use super::captions_and_text::text_project;
use super::*;

fn rich_texts() -> crate::TextTextures {
    let mut texts = crate::TextTextures::new();
    for (family, file) in [
        ("Alpha2 Fixture", "Alpha2Fixture.ttf"),
        ("Alpha2 Fixture Hangul", "Alpha2FixtureHangul.ttf"),
        ("Alpha2 Fixture Styles", "Alpha2FixtureStyles.ttf"),
        ("Alpha2 Fixture Styles", "Alpha2FixtureStylesItalic.ttf"),
    ] {
        texts
            .add_font(
                family,
                std::fs::read(format!("../../tests/fonts/{file}")).expect("fixture font"),
                0,
            )
            .expect("font");
    }
    texts
}

fn caption_track(style: &str, runs: &str) -> String {
    format!(
        r#", {{ "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {{
            "style": {style},
            "cues": [ {{ "timeline_range": {{ "start": 0, "end": 240000 }}, "text": {runs} }} ] }} }}"#
    )
}

const RICH_STYLE: &str = r##"{ "font_size": 0.12, "max_width": 0.95, "line_height": 1.1,
    "position": { "x": 0.5, "y": 0.5 }, "anchor": { "x": 0.5, "y": 0.5 },
    "shadows": [ { "color": "#000000c0", "offset": { "x": 0.08, "y": 0.08 }, "blur": 0 } ] }"##;

const RICH_RUNS: &str = r##"[
    { "text": "small " },
    { "text": "BIG", "style": { "font_scale": { "mode": "constant", "value": 1.6 }, "fill": "#ffd020" } },
    { "text": " TILE ", "style": { "font_family": "Alpha2 Fixture Styles", "font_style": "italic" } },
    { "text": "안녕", "style": { "font_style": "italic" } },
    { "text": " under", "style": { "underline": true, "background_color": "#2040c0c0" } },
    { "text": " out", "style": { "line_through": true, "strokes": [ { "color": "#000000", "width": 0.1 } ] } }
]"##;

fn render_gpu(renderer: &Renderer, project: &crate::EngineProject, ticks: i64) -> Vec<u8> {
    let size = RenderSize::new(160, 90).expect("size");
    let mut texts = rich_texts();
    texts
        .prepare_at(renderer, project, MediaTime::from_ticks(ticks), size)
        .expect("text");
    let frame = project
        .evaluate(MediaTime::from_ticks(ticks))
        .expect("frame");
    let output = renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
        .expect("frame renders");
    read_rgba8(renderer, &output.texture.expect("complete"))
}

fn render_cpu(project: &crate::EngineProject, ticks: i64) -> Vec<u8> {
    let renderer = alpha2_render::CpuRenderer::new(2);
    let size = RenderSize::new(160, 90).expect("size");
    let mut texts = rich_texts();
    texts
        .prepare_at(&renderer, project, MediaTime::from_ticks(ticks), size)
        .expect("text");
    let frame = project
        .evaluate(MediaTime::from_ticks(ticks))
        .expect("frame");
    renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
        .expect("frame renders")
        .texture
        .expect("complete")
        .read_rgba8()
}

fn renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    renderer
}

#[test]
fn rich_text_golden() {
    let _lock = gpu_test_lock();
    let project = text_project("", "", &caption_track(RICH_STYLE, RICH_RUNS));
    let pixels = render_gpu(&renderer(), &project, 120_000);
    let yellow = pixels
        .chunks(4)
        .filter(|pixel| pixel[0] > 200 && pixel[1] > 160 && pixel[2] < 80)
        .count();
    let blue = pixels
        .chunks(4)
        .filter(|pixel| pixel[2] > 150 && pixel[0] < 80)
        .count();
    assert!(
        yellow > 20 && blue > 20,
        "{yellow} yellow, {blue} blue pixels"
    );
    // Fractional placement makes edges bilinear samples that software rasterizers weight apart.
    assert_golden("rich_text", (160, 90), &pixels, 8);
}

#[test]
fn rich_text_draws_the_same_on_the_cpu() {
    let _lock = gpu_test_lock();
    let project = text_project("", "", &caption_track(RICH_STYLE, RICH_RUNS));
    let gpu = render_gpu(&renderer(), &project, 120_000);
    let cpu = render_cpu(&project, 120_000);
    let largest = gpu
        .iter()
        .zip(&cpu)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    // Tolerance for the CPU renderer's resampling across devices.
    assert!(
        largest <= 8,
        "CPU and GPU rich text differ by up to {largest}"
    );
}

#[test]
fn a_growing_run_rewraps_mid_animation() {
    let _lock = gpu_test_lock();
    let runs = r#"[ { "text": "one " },
        { "text": "two", "style": { "font_scale": { "mode": "keyframes", "keyframes": [
            { "time": 0, "value": 1, "interpolation": { "type": "linear" } },
            { "time": 120000, "value": 4, "interpolation": { "type": "hold" } } ] } } },
        { "text": " three" } ]"#;
    let style = r#"{ "font_size": 0.1, "max_width": 0.5, "position": { "x": 0.5, "y": 0.5 },
        "anchor": { "x": 0.5, "y": 0.5 } }"#;
    let project = text_project("", "", &caption_track(style, runs));
    let size = RenderSize::new(160, 90).expect("size");
    let renderer = renderer();
    let mut texts = rich_texts();
    let lines = |texts: &mut crate::TextTextures, ticks: i64| {
        let frame = project
            .evaluate(MediaTime::from_ticks(ticks))
            .expect("frame");
        let caption = &frame.captions[0];
        texts
            .measure(
                &alpha2_layout::TextAt::new(caption.text, caption.local_time),
                caption.style,
                size,
            )
            .expect("measures")
            .line_count
    };
    assert_eq!(lines(&mut texts, 0), 1);
    assert!(lines(&mut texts, 60_000) >= 1);
    assert!(
        lines(&mut texts, 150_000) > 1,
        "the grown word wraps the line"
    );
    assert_ne!(
        render_gpu(&renderer, &project, 0),
        render_gpu(&renderer, &project, 150_000)
    );
}
