//! Frames whose correction and effect parameters follow data sets.

use std::sync::Arc;

use serde_json::Value;

use super::text_look::look_renderer;
use super::*;

const W: u32 = 160;
const H: u32 = 90;
const PICTURE: (u32, u32) = (128, 72);

/// An opaque 16:9 picture with a coloured quadrant per corner and a white diagonal, so turns,
/// moves and pins all show.
fn picture() -> Vec<u8> {
    let (width, height) = PICTURE;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let diagonal = (i64::from(x) * i64::from(height) - i64::from(y) * i64::from(width))
                .abs()
                < i64::from(width);
            let pixel = match (x < width / 2, y < height / 2) {
                _ if diagonal => [255, 255, 255, 255],
                (true, true) => [220, 40, 40, 255],
                (false, true) => [40, 180, 60, 255],
                (true, false) => [40, 70, 220, 255],
                (false, false) => [230, 200, 40, 255],
            };
            pixels.extend_from_slice(&pixel);
        }
    }
    pixels
}

/// Every value held over the clip: a similarity correction, and four picture-space corner points.
fn data() -> Value {
    serde_json::json!([
        { "id": "stab", "anchor": { "type": "source", "source_id": "v" }, "kind": "similarity",
          "interpolation": "linear", "times": [0, 120_000],
          "values": [0.1, -0.05, 15.0, 0.75, 0.1, -0.05, 15.0, 0.75] },
        { "id": "pins", "anchor": { "type": "source", "source_id": "v" }, "kind": "points",
          "channels": ["tl", "tr", "br", "bl"], "interpolation": "linear", "times": [0, 120_000],
          "values": [0.1, 0.15, 0.85, 0.05, 0.95, 0.9, 0.05, 0.8,
                     0.1, 0.15, 0.85, 0.05, 0.95, 0.9, 0.05, 0.8] }
    ])
}

fn project(clip: &Value, compositions: &Value) -> crate::EngineProject {
    let document = serde_json::json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": W, "height": H, "frame_rate": { "num": 30, "den": 1 },
                      "sample_rate": 48000, "background": { "red": 0.1, "green": 0.1, "blue": 0.1 } },
        "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
        "data": data(),
        "compositions": compositions,
        "timeline": { "tracks": [{ "id": "t", "name": "T", "kind": "video", "clips": [clip] }] }
    });
    let project = alpha2_model::Project::from_json(&document.to_string()).expect("project");
    crate::EngineProject::new(project).expect("indexed")
}

fn clip(extra: &Value) -> Value {
    let mut clip = serde_json::json!({
        "id": "c", "source_id": "v",
        "timeline_range": { "start": 0, "end": 120_000 }, "source_range": { "start": 0, "end": 120_000 }
    });
    for (key, value) in extra.as_object().expect("object") {
        clip[key] = value.clone();
    }
    clip
}

/// A clip corrected by `stab`, and a clip whose corner pin follows `pins`.
fn projects() -> [crate::EngineProject; 2] {
    let pin = |channel: &str| {
        serde_json::json!({ "mode": "data", "data": "pins", "channel": channel,
                            "via": { "node": "media" }, "space": "picture" })
    };
    let pinned = serde_json::json!([{
        "id": "k",
        "nodes": [
            { "id": "clip", "op": { "type": "clip" } },
            { "id": "media", "op": { "type": "media", "source_id": "v" } },
            { "id": "pin", "op": { "type": "effect", "input": "clip", "effect": {
                "id": "alpha2.effect.corner-pin", "version": 1,
                "params": { "top_left": pin("tl"), "top_right": pin("tr"),
                            "bottom_right": pin("br"), "bottom_left": pin("bl") } } } }
        ],
        "output": "pin"
    }]);
    [
        project(
            &clip(&serde_json::json!({ "visual": { "correction": { "data": "stab" } } })),
            &serde_json::json!([]),
        ),
        project(
            &clip(&serde_json::json!({ "composition": { "id": "k" } })),
            &pinned,
        ),
    ]
}

/// The two frames side by side.
fn strip(frames: &[Vec<u8>; 2]) -> Vec<u8> {
    let row = (W * 4) as usize;
    (0..H as usize)
        .flat_map(|y| {
            frames
                .iter()
                .flat_map(move |frame| frame[y * row..(y + 1) * row].iter().copied())
        })
        .collect()
}

const TIME: MediaTime = MediaTime::from_ticks(60_000);

#[test]
fn a_correction_and_a_data_driven_corner_pin_draw_their_golden() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let size = RenderSize::new(PICTURE.0, PICTURE.1).expect("size");
    let texture = rgba_image_texture(&renderer, size, &picture());
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let frames = SingleFrame {
        source: SourceId::new("v").expect("id"),
        texture: &view,
    };
    let canvas = RenderSize::new(W, H).expect("size");
    let drawn = projects().map(|project| {
        let frame = project.evaluate(TIME).expect("frame");
        assert!(frame.visual[0].correction.is_some() || frame.visual[0].composition.is_some());
        let output = renderer
            .render_frame(canvas, &frame, &frames)
            .expect("frame renders");
        read_rgba8(&renderer, &output.texture.expect("complete"))
    });
    // The pinned quad's edges sample between pixels, where 8-bit intermediates round apart.
    assert_golden_or_rgba8(
        "data_driven",
        "data_driven_rgba8",
        (W * 2, H),
        &strip(&drawn),
        4,
    );
}

/// Answers every source with one CPU picture.
struct Cpu<'a>(&'a alpha2_render::CpuSource);

impl FrameProvider for Cpu<'_> {
    fn texture_for(&self, _: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        Some(SourceView::cpu(self.0))
    }
}

#[test]
fn a_correction_and_a_data_driven_corner_pin_draw_their_golden_on_the_cpu() {
    let renderer = alpha2_render::CpuRenderer::new(2);
    let size = RenderSize::new(PICTURE.0, PICTURE.1).expect("size");
    let source = alpha2_render::CpuSource::image(Arc::new(
        alpha2_render::CpuImage::from_premultiplied_rgba8(size, &picture()).expect("picture"),
    ));
    let canvas = RenderSize::new(W, H).expect("size");
    let drawn = projects().map(|project| {
        let frame = project.evaluate(TIME).expect("frame");
        renderer
            .render_frame(canvas, &frame, &Cpu(&source))
            .expect("frame renders")
            .texture
            .expect("complete")
            .read_rgba8()
    });
    assert_cpu_golden("data_driven", (W * 2, H), &strip(&drawn), 2);
}

#[test]
fn a_held_correction_moves_turns_and_scales_the_picture() {
    let corrected = &projects()[0];
    let frame = corrected.evaluate(TIME).expect("frame");
    let correction = frame.visual[0].correction.expect("a correction");
    assert_eq!(
        (
            correction.translate,
            correction.rotation_degrees,
            correction.scale
        ),
        (Vec2::new(0.1, -0.05), 15.0, 0.75)
    );
    // Without the data set the clip draws uncorrected.
    let plain = project(&clip(&serde_json::json!({})), &serde_json::json!([]));
    assert_eq!(
        plain.evaluate(TIME).expect("frame").visual[0].correction,
        None
    );
}
