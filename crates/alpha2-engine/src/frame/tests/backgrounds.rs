//! Backgrounds and canvases: opacity over the background, preview and export agreeing, and odd canvases.

use super::*;

fn white_clip(opacity: f32, scale: Vec2) -> ActiveClip<'static> {
    ActiveClip {
        clip_id: leak(ClipId::new("clip").expect("valid ID")),
        source_id: leak(SourceId::new("source").expect("valid ID")),
        track_id: leak(TrackId::new("track").expect("valid ID")),
        source_time: MediaTime::ZERO,
        visual: Some(visual(scale, opacity)),
        composition: None,
        blend_mode: ClipBlendMode::Normal,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        plan: None,
        source_composition: None,
    }
}

fn srgb_encode(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * libm::powf(linear, 1.0 / 2.4) - 0.055
    }
}

#[allow(clippy::cast_precision_loss)]
fn mean(values: &[u8]) -> f32 {
    values.iter().map(|&value| f32::from(value)).sum::<f32>() / values.len() as f32
}

#[test]
fn export_keeps_layer_opacity_over_the_background() {
    // Dropping alpha must preserve opacity; unpremultiplying makes layers fully bright.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 2).expect("even size");
    let white = rgba_texture(&renderer, [255, 255, 255, 255]);
    let view = white.create_view(&wgpu::TextureViewDescriptor::default());
    let frames = SingleFrame {
        source: SourceId::new("source").expect("valid ID"),
        texture: &view,
    };
    let mut previous = 0.0;
    for opacity in [0.25_f32, 0.5, 0.75] {
        let frame = FrameDesc {
            background: Some(alpha2_model::Rgb::BLACK),
            blend_space: alpha2_model::BlendSpace::LinearLight,
            time: MediaTime::ZERO,
            visual: vec![white_clip(opacity, Vec2::new(1.0, 1.0))],
            audio: Vec::new(),
            transitions: Vec::new(),
            captions: Vec::new(),
        };
        let planes = renderer
            .render_frame_yuv420p(size, &frame, &frames, centred(ColorMetadata::default()))
            .expect("frame encodes")
            .readback
            .expect("complete frame has readback")
            .wait()
            .expect("readback maps");
        let luma = mean(&planes.y);
        // White over black at this coverage, blended in linear light.
        let expected = 16.0 + 219.0 * srgb_encode(opacity);
        assert!(
            (luma - expected).abs() <= 2.0,
            "opacity {opacity}: Y {luma} != {expected}"
        );
        assert!(luma > previous, "opacity {opacity} is not brighter");
        previous = luma;
    }
}

#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn a_non_black_background_is_the_same_frame_in_preview_and_export() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let white = rgba_texture(&renderer, [255, 255, 255, 255]);
    let view = white.create_view(&wgpu::TextureViewDescriptor::default());
    let frames = SingleFrame {
        source: SourceId::new("source").expect("valid ID"),
        texture: &view,
    };
    let background = alpha2_model::Rgb::new(0.2, 0.4, 0.8);
    // A half-transparent layer over the middle half, empty canvas either side.
    let frame = FrameDesc {
        background: Some(background),
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![white_clip(0.5, Vec2::new(0.5, 1.0))],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let preview = read_rgba8(
        &renderer,
        &renderer
            .render_frame(size, &frame, &frames)
            .expect("frame renders")
            .texture
            .expect("complete frame has output"),
    );
    let exported = renderer
        .render_frame_yuv420p(size, &frame, &frames, centred(ColorMetadata::default()))
        .expect("frame encodes")
        .readback
        .expect("complete frame has readback")
        .wait()
        .expect("readback maps");

    let quantize = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    let pixel = |x: usize, y: usize| &preview[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
    assert!(
        preview
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255)
    );
    for x in [0, 1, 6, 7] {
        let expected = [background.red, background.green, background.blue].map(quantize);
        assert!(
            pixel(x, 0)[..3]
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "empty canvas at x={x} is {:?}, not the background {expected:?}",
            pixel(x, 0)
        );
    }
    assert!(pixel(4, 0)[..3].iter().all(|value| *value > 150));

    // The BT.709 limited-range encoding of the preview frame.
    let (kr, kb) = (0.2126_f32, 0.0722_f32);
    let components = |pixel: &[u8]| {
        pixel[..3]
            .iter()
            .map(|v| f32::from(*v) / 255.0)
            .collect::<Vec<_>>()
    };
    for y in 0..4 {
        for x in 0..8 {
            let rgb = components(pixel(x, y));
            let luma = kr * rgb[0] + (1.0 - kr - kb) * rgb[1] + kb * rgb[2];
            let expected = quantize((16.0 + 219.0 * luma) / 255.0);
            let actual = exported.y[y * 8 + x];
            assert!(
                actual.abs_diff(expected) <= 1,
                "Y at ({x}, {y}): export {actual}, preview {expected}"
            );
        }
    }
    for y in 0..2 {
        for x in 0..4 {
            let mut rgb = [0.0_f32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                for (sum, value) in rgb
                    .iter_mut()
                    .zip(components(pixel(2 * x + dx, 2 * y + dy)))
                {
                    *sum += value * 0.25;
                }
            }
            let luma = kr * rgb[0] + (1.0 - kr - kb) * rgb[1] + kb * rgb[2];
            let u = quantize((128.0 + 224.0 * (rgb[2] - luma) / (2.0 * (1.0 - kb))) / 255.0);
            let v = quantize((128.0 + 224.0 * (rgb[0] - luma) / (2.0 * (1.0 - kr))) / 255.0);
            assert!(exported.u[y * 4 + x].abs_diff(u) <= 1, "U at ({x}, {y})");
            assert!(exported.v[y * 4 + x].abs_diff(v) <= 1, "V at ({x}, {y})");
        }
    }
}

#[test]
fn an_odd_canvas_previews_even_though_it_cannot_be_exported() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = SourceId::new("source").expect("valid ID");
    let frame = FrameDesc {
        background: Some(alpha2_model::Rgb::BLACK),
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let input = rgba_texture(&renderer, [0, 255, 0, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(65, 49).expect("non-zero size");
    let output = renderer
        .render_frame(
            size,
            &frame,
            &SingleFrame {
                source,
                texture: &view,
            },
        )
        .expect("an odd preview size renders");
    let texture = output.texture.expect("complete frame has output");
    assert_eq!(read_rgba8(&renderer, &texture).len(), 65 * 49 * 4);
}
