//! Text looks: gradient fills, stacked strokes and shadows, on captions,
//! text generators and the CPU renderer.

use super::captions_and_text::text_project;
use super::*;

/// A caption track whose one cue shows `text` in `style` for the whole
/// project, as a track slot of [`text_project`].
fn caption_track(style: &str, text: &str) -> String {
    format!(
        r#", {{ "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {{
            "style": {style},
            "cues": [ {{ "timeline_range": {{ "start": 0, "end": 240000 }}, "text": "{text}" }} ] }} }}"#
    )
}

/// Every layer of a look: a box, two shadows (a hard magenta one and a soft
/// dark one under it), two strokes and a gradient fill. The font is 22.5
/// pixels at 160x90, so the hard shadow lies a whole 9 pixels off: a
/// fractional offset resamples its edge a second time, which software
/// rasterizers filter a code or more apart.
const LOOK: &str = r##"{ "font_size": 0.25, "position": { "x": 0.5, "y": 0.55 }, "anchor": { "x": 0.5, "y": 0.5 },
    "fill": { "type": "linear_gradient", "start": { "x": 0, "y": 0 }, "end": { "x": 1, "y": 0 },
              "stops": [ { "offset": 0, "color": "#ff3020" }, { "offset": 1, "color": "#2040ff" } ] },
    "strokes": [ { "color": "#000000", "width": 0.06 }, { "color": "#ffffff", "width": 0.08 } ],
    "shadows": [ { "color": "#ff00ffff", "offset": { "x": 0.4, "y": 0.4 }, "blur": 0 },
                 { "color": "#000000c0", "offset": { "x": 0, "y": 0.2 }, "blur": 0.3 } ],
    "background": { "color": "#2040a060", "padding": 0.1 } }"##;

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

/// `project`'s frame at one second at `size` on the GPU.
fn gpu_pixels(renderer: &Renderer, project: &crate::EngineProject, size: RenderSize) -> Vec<u8> {
    let mut texts = fixture_texts();
    texts
        .prepare_at(renderer, project, MediaTime::from_ticks(120_000), size)
        .expect("text");
    let frame = project
        .evaluate(MediaTime::from_ticks(120_000))
        .expect("frame");
    let output = renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
        .expect("frame renders");
    read_rgba8(renderer, &output.texture.expect("complete"))
}

/// The same on the CPU.
fn cpu_pixels(
    renderer: &alpha2_render::CpuRenderer,
    project: &crate::EngineProject,
    size: RenderSize,
) -> Vec<u8> {
    let mut texts = fixture_texts();
    texts
        .prepare_at(renderer, project, MediaTime::from_ticks(120_000), size)
        .expect("text");
    let frame = project
        .evaluate(MediaTime::from_ticks(120_000))
        .expect("frame");
    renderer
        .render_frame(size, &frame, &CaptionFrames(&texts))
        .expect("frame renders")
        .texture
        .expect("complete")
        .read_rgba8()
}

fn look_renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    renderer
}

fn size() -> RenderSize {
    RenderSize::new(160, 90).expect("size")
}

fn at(pixels: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    pixels[(y * width + x) * 4..][..4]
        .try_into()
        .expect("a pixel")
}

#[test]
fn caption_look_draws_background_shadows_strokes_fill_in_order() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let look = text_project("", "", &caption_track(LOOK, "Look"));
    // The same look with invisible shadows: drawn the same way, the boxes on a plane of their
    // own, so only the shadows' pixels differ.
    let unshadowed = LOOK
        .replace("#ff00ffff", "#ff00ff00")
        .replace("#000000c0", "#00000000");
    assert_ne!(unshadowed, LOOK);
    let plain = text_project("", "", &caption_track(&unshadowed, "Look"));
    let (look, plain) = (
        gpu_pixels(&renderer, &look, size()),
        gpu_pixels(&renderer, &plain, size()),
    );
    // The shadows lie under the strokes and fill: where the plain look shows its fill (red
    // to blue, saturated where no box or backdrop grey shows), which lies over the opaque black
    // stroke, the shadowed one is the same; elsewhere near the text the shadows show, over the
    // box. The white outer stroke's antialiased rim lets a shadow under it show through.
    let saturated = |pixel: &[u8]| {
        let (high, low) = (pixel[..3].iter().max(), pixel[..3].iter().min());
        high.zip(low).is_some_and(|(high, low)| high - low > 90)
    };
    let (mut bright, mut changed, mut shadowed, mut magenta) = (0, 0, 0, 0);
    for (with, without) in look.chunks(4).zip(plain.chunks(4)) {
        if saturated(without) {
            bright += 1;
            if with.iter().zip(without).any(|(a, b)| a.abs_diff(*b) > 2) {
                changed += 1;
            }
        } else if with != without {
            shadowed += 1;
            if with[0] > 150 && with[2] > 150 && with[1] < 100 {
                magenta += 1;
            }
        }
    }
    assert!(bright > 60, "{bright} fill pixels");
    assert!(
        changed * 20 < bright,
        "{changed} of {bright} fill pixels show a shadow"
    );
    assert!(
        shadowed > 100 && magenta > 20,
        "{shadowed} shadowed, {magenta} magenta"
    );
    // The caption lands at a fractional position, so its edges are bilinear samples; lavapipe
    // weights them a little apart from hardware, and the opaque magenta shadow's edge, cast by
    // those samples over a dark one, shows that as up to 7 codes in green.
    alpha2_golden_support::assert_rgba8_golden("caption_look", (160, 90), &look, 8);
}

#[test]
fn caption_look_draws_the_same_on_the_cpu() {
    let _lock = gpu_test_lock();
    let gpu = gpu_pixels(
        &look_renderer(),
        &text_project("", "", &caption_track(LOOK, "Look")),
        size(),
    );
    let cpu = cpu_pixels(
        &alpha2_render::CpuRenderer::new(2),
        &text_project("", "", &caption_track(LOOK, "Look")),
        size(),
    );
    let largest = gpu
        .iter()
        .zip(&cpu)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    // Within what the CPU renderer's blur and resampling keep to on every device, lavapipe the
    // farthest.
    assert!(largest <= 8, "CPU and GPU looks differ by up to {largest}");
}

#[test]
fn a_shadow_scales_with_the_font_size() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    // A hard green shadow half an em right and down of white text, on the grey backdrop.
    let style = r##"{ "font_size": 0.2, "position": { "x": 0.4, "y": 0.45 }, "anchor": { "x": 0.5, "y": 0.5 },
        "shadows": [ { "color": "#00ff00", "offset": { "x": 0.5, "y": 0.5 }, "blur": 0 } ] }"##;
    let project = text_project("", "", &caption_track(style, "II"));
    // Centroids of the white text and the green shadow at two sizes.
    let centroids = |pixels: &[u8], width: usize| {
        let (mut text, mut shadow) = (([0.0_f64; 2], 0.0), ([0.0_f64; 2], 0.0));
        for (index, pixel) in pixels.chunks(4).enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let point = [(index % width) as f64, (index / width) as f64];
            let target = if pixel[0] > 250 && pixel[1] > 250 && pixel[2] > 250 {
                &mut text
            } else if pixel[1] > 250 && pixel[0] < 10 {
                &mut shadow
            } else {
                continue;
            };
            target.0[0] += point[0];
            target.0[1] += point[1];
            target.1 += 1.0;
        }
        [0, 1].map(|axis| shadow.0[axis] / shadow.1 - text.0[axis] / text.1)
    };
    let small = centroids(&gpu_pixels(&renderer, &project, size()), 160);
    let large = centroids(
        &gpu_pixels(
            &renderer,
            &project,
            RenderSize::new(320, 180).expect("size"),
        ),
        320,
    );
    for axis in 0..2 {
        assert!(small[axis] > 5.0, "{small:?}");
        let ratio = large[axis] / small[axis];
        assert!(
            (ratio - 2.0).abs() < 0.15,
            "axis {axis}: {small:?} then {large:?}"
        );
    }
}

#[test]
fn text_gradient_matches_a_shape_gradient() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let gradient = r##"{ "type": "linear_gradient", "start": { "x": 0, "y": 0 }, "end": { "x": 1, "y": 0 },
        "stops": [ { "offset": 0, "color": "#ff2010" }, { "offset": 0.6, "color": "#20c040" },
                   { "offset": 1, "color": "#1030ff" } ] }"##;
    // Text whose layout box is the canvas's whole width, from its left edge: the gradient over
    // the box is the gradient over the canvas.
    let style = format!(
        r#"{{ "font_size": 0.8, "max_width": 1, "align": "start", "position": {{ "x": 0, "y": 0.5 }},
            "anchor": {{ "x": 0, "y": 0.5 }}, "fill": {gradient} }}"#
    );
    let text = gpu_pixels(
        &renderer,
        &text_project(
            &format!(
                r#"{{ "id": "title", "kind": "generated", "generator": {{ "type": "text",
                    "text": "IIIIII", "style": {style} }} }},"#
            ),
            "",
            r#", { "id": "titles", "name": "Titles", "kind": "video", "clips": [
                { "id": "t", "source_id": "title", "timeline_range": { "start": 0, "end": 240000 },
                  "source_range": { "start": 0, "end": 240000 } } ] }"#,
        ),
        size(),
    );
    let fill = gpu_pixels(
        &renderer,
        &text_project(
            &format!(
                r#"{{ "id": "paint", "kind": "generated", "generator": {{ "type": "fill", "paint": {gradient} }} }},"#
            ),
            "",
            r#", { "id": "painted", "name": "Painted", "kind": "video", "clips": [
                { "id": "p", "source_id": "paint", "timeline_range": { "start": 0, "end": 240000 },
                  "source_range": { "start": 0, "end": 240000 } } ] }"#,
        ),
        size(),
    );
    let grey = at(
        &gpu_pixels(&renderer, &text_project("", "", ""), size()),
        160,
        0,
        0,
    );
    // Pixels inside the stems, where the glyphs cover the pixel and its neighbours wholly.
    let mut compared = 0;
    for y in 1..89 {
        for x in 1..159 {
            let inside = (0..9).all(|n| {
                let pixel = at(&text, 160, x + n % 3 - 1, y + n / 3 - 1);
                pixel != grey && at(&fill, 160, x + n % 3 - 1, y + n / 3 - 1) != grey
            }) && (0..9).all(|n| {
                let (px, py) = (x + n % 3 - 1, y + n / 3 - 1);
                at(&text, 160, px, py)
                    .iter()
                    .zip(at(&fill, 160, px, py))
                    .all(|(a, b)| a.abs_diff(b) <= 40)
            });
            if !inside {
                continue;
            }
            compared += 1;
            let (a, b) = (at(&text, 160, x, y), at(&fill, 160, x, y));
            assert!(
                a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 1),
                "({x}, {y}): text {a:?}, shape {b:?}"
            );
        }
    }
    assert!(compared > 100, "{compared} pixels compared");
}

#[test]
fn drop_shadow_effect_is_unchanged_by_the_shared_shadow_pass() {
    // The drop-shadow effect is drawn by the passes `add_shadow` draws: its golden and the
    // host-parity effects project hold its pixels. Here the shared passes are held to the
    // effect's own: a sharp shadow of a square is the square moved and tinted.
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let size = RenderSize::new(16, 16).expect("size");
    let square = (0..16_u32)
        .flat_map(|y| {
            (0..16_u32).map(move |x| {
                if (4..8).contains(&x) && (4..8).contains(&y) {
                    [255_u8; 4]
                } else {
                    [0; 4]
                }
            })
        })
        .flatten()
        .collect::<Vec<_>>();
    let texture = rgba_image_texture(&renderer, size, &square);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut graph = alpha2_render::RenderGraph::new(size);
    let source = graph.add_source(SourceView::from_wgpu(&view));
    let shadow = graph
        .add_shadow(source, [3.0, 2.0], 0.0, [1.0, 0.0, 0.0, 1.0])
        .expect("a shadow");
    graph.set_output(shadow).expect("output");
    let output = renderer.render_graph(&graph, &[]).expect("renders");
    let pixels = read_rgba8(&renderer, &output);
    for y in 0..16 {
        for x in 0..16 {
            let inside = (7..11).contains(&x) && (6..10).contains(&y);
            let expected = if inside { [255, 0, 0, 255] } else { [0; 4] };
            assert_eq!(at(&pixels, 16, x, y), expected, "({x}, {y})");
        }
    }
}

#[test]
fn caption_speakers_golden() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    // Two speakers, each with a style of their name; the second cue names its style itself.
    let track = r##", { "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {
        "style": { "font_size": 0.16, "position": { "x": 0.5, "y": 0.9 },
                   "strokes": [ { "color": "#000000", "width": 0.08 } ] },
        "styles": {
            "Ann": { "fill": "#ffd040", "position": { "x": 0.3, "y": 0.4 } },
            "boxed": { "fill": "#40e0ff", "align": "end", "position": { "x": 0.95, "y": 0.9 },
                       "anchor": { "x": 1, "y": 1 },
                       "background": { "color": "#102040c0", "padding": 0.2 } } },
        "cues": [
            { "timeline_range": { "start": 0, "end": 120000 }, "text": "Ann", "speaker": "Ann" },
            { "timeline_range": { "start": 120000, "end": 240000 }, "text": "Bob", "speaker": "Bob",
              "style": "boxed" } ] } }"##;
    let project = text_project("", "", track);
    let frame = |ticks: i64| {
        let mut texts = fixture_texts();
        let size = size();
        texts
            .prepare_at(&renderer, &project, MediaTime(ticks), size)
            .expect("text");
        let frame = project.evaluate(MediaTime(ticks)).expect("frame");
        let output = renderer
            .render_frame(size, &frame, &CaptionFrames(&texts))
            .expect("frame renders");
        read_rgba8(&renderer, &output.texture.expect("complete"))
    };
    let (ann, bob) = (frame(60_000), frame(180_000));
    let strip = (0..90)
        .flat_map(|row| {
            [&ann, &bob]
                .into_iter()
                .flat_map(move |pixels| pixels[row * 640..(row + 1) * 640].iter().copied())
        })
        .collect::<Vec<_>>();
    alpha2_golden_support::assert_rgba8_golden("caption_speakers", (320, 90), &strip, 2);
}
