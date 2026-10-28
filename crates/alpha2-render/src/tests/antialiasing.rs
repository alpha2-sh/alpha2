use super::*;

fn composite_pixels(
    renderer: &Renderer,
    source: &wgpu::TextureView,
    size: RenderSize,
    visual: LayerTransform,
) -> Vec<u8> {
    let output = renderer
        .render_layers(
            size,
            &[CompositeLayer {
                texture: source,
                visual,
                blend_mode: BlendMode::Normal,
            }],
        )
        .unwrap();
    read_rgba8(renderer, &output)
}

#[test]
fn rotated_media_has_fractional_coverage_without_dark_fringes() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = rgba_texture(&renderer, [255; 4]);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(64, 64).unwrap();
    for scale in [[0.6, 0.6], [-0.6, 0.6]] {
        let mut transform = visual(scale, 0.5);
        transform.rotation_degrees = 23.0;
        let pixels = composite_pixels(&renderer, &view, size, transform);
        let partial = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 0 && p[3] < 126)
            .count();
        assert!(partial > 80, "missing edge coverage: {partial}");
        for pixel in pixels.as_chunks::<4>().0 {
            assert_rgba8_near(pixel, &[pixel[3]; 4], 1);
        }
        let area: f64 = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| f64::from(p[3]) / 127.5)
            .sum();
        assert!(
            (area - (64.0 * 0.6_f64).powi(2)).abs() < 10.0,
            "area={area}"
        );
    }
}

#[test]
fn pixel_aligned_identity_is_sharp_and_subpixel_layers_keep_coverage() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(16, 16).unwrap();
    let bytes: Vec<u8> = (0..256)
        .flat_map(|i| {
            if i % 2 == 0 {
                [0; 4]
            } else {
                [128, 64, 0, 128]
            }
        })
        .collect();
    let texture = rgba_image_texture(&renderer, size, &bytes);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    assert_rgba8_near(
        &composite_pixels(&renderer, &view, size, visual([1.0, 1.0], 1.0)),
        &bytes,
        1,
    );

    let white = rgba_texture(&renderer, [255; 4]);
    let view = white.create_view(&wgpu::TextureViewDescriptor::default());
    // A 0.5 x 0.5 pixel square centered on a pixel corner covers four pixels equally; a
    // center-only UV test would lose it entirely.
    let pixels = composite_pixels(
        &renderer,
        &view,
        size,
        visual([0.5 / 16.0, 0.5 / 16.0], 1.0),
    );
    let nonzero: Vec<_> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] > 0)
        .collect();
    assert_eq!(nonzero.len(), 4);
    for pixel in nonzero {
        assert_rgba8_near(pixel, &[16; 4], 1);
    }
}

#[test]
fn minification_averages_texture_detail_and_associated_alpha() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source_size = RenderSize::new(96, 96).unwrap();
    let output_size = RenderSize::new(32, 32).unwrap();
    for color in [[255; 4], [128, 64, 0, 128]] {
        let bytes: Vec<u8> = (0..96 * 96)
            .flat_map(|i| if i % 3 == 1 { color } else { [0; 4] })
            .collect();
        let texture = rgba_image_texture(&renderer, source_size, &bytes);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let transform = visual([1.0, 1.0], 1.0);
        renderer.set_quality_controls(1, 1);
        let unfiltered = composite_pixels(&renderer, &view, output_size, transform);
        assert_rgba8_near(&unfiltered[..4], &color, 1);
        renderer.set_quality_controls(64, 1);
        let filtered = composite_pixels(&renderer, &view, output_size, transform);
        for pixel in filtered.as_chunks::<4>().0 {
            assert_rgba8_near(pixel, &color.map(|v| v / 3), 1);
        }
    }
}

/// Parameters that make `name`'s edge one between the plain black and white
/// pictures: backgrounds and shadows drawn white or transparent, hard edges.
fn edge_test_params(name: &str) -> EffectParams<'static> {
    let mut params = EffectParams::new();
    if [
        "circle",
        "circle-crop",
        "left-right",
        "top-bottom",
        "simple-flip",
        "rotate-scale-fade",
        "rotate-scale-vanish",
    ]
    .contains(&name)
    {
        params = params.with("back_color", ParamValue::Color([1.0; 4]));
    }
    if name == "bounce" {
        params = params.with("shadow_color", ParamValue::Color([1.0; 4]));
    }
    if name == "directional-scaled" {
        params = params.with("scale", ParamValue::Float(1.5));
    }
    if ["cube", "doorway", "swap"].contains(&name) {
        params = params.with("back_color", ParamValue::Color([0.0; 4]));
    }
    if name == "grid-flip" {
        params = params.with("bg_color", ParamValue::Color([0.0; 4]));
    }
    if ["circle-open", "directional-wipe", "window-slice"].contains(&name) {
        params = params.with("smoothness", ParamValue::Float(0.0));
    }
    if ["rectangle", "rectangle-crop"].contains(&name) {
        params = params.with("bg_color", ParamValue::Color([1.0; 4]));
    }
    if name == "static-wipe" {
        params = params.with("max_static_span", ParamValue::Float(0.0));
    }
    if name == "gradient-wipe" {
        params = params.with("softness", ParamValue::Float(0.0));
    }
    params
}

#[test]
fn geometric_transition_edges_have_partial_coverage() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let from = rgba_texture(&renderer, [0; 4]);
    let to = rgba_texture(&renderer, [255; 4]);
    let a = from.create_view(&wgpu::TextureViewDescriptor::default());
    let b = to.create_view(&wgpu::TextureViewDescriptor::default());
    for name in [
        "heart",
        "angular",
        "pinwheel",
        "polka-dots-curtain",
        "wipe-up",
        "wipe-down",
        "wipe-left",
        "wipe-right",
        "directional",
        "squeeze",
        "circle",
        "circle-crop",
        "window-slice",
        "circle-open",
        "directional-wipe",
        "radial",
        "rotate-scale-fade",
        "rotate-scale-vanish",
        "whip-pan",
        "bounce",
        "directional-scaled",
        "slides",
        "fold",
        "left-right",
        "top-bottom",
        "rolls",
        "split-slide-in-horizontal",
        "split-slide-in-vertical",
        "split-slide-out-horizontal",
        "split-slide-out-vertical",
        "split-slide-in-out-horizontal",
        "split-slide-in-out-vertical",
        "x-axis-translation",
        "cover",
        "cube",
        "doorway",
        "swap",
        "grid-flip",
        "book-flip",
        "simple-flip",
        "page-peel",
        "box",
        "chessboard",
        "bow-tie",
        "bow-tie-horizontal",
        "bow-tie-vertical",
        "polar-function",
        "rectangle",
        "rectangle-crop",
        "star-wipe",
        "shape-wipe",
        "screen-melt",
        "squares-wire",
        "static-wipe",
        "gradient-wipe",
    ] {
        let id = ids
            .iter()
            .find(|id| id.as_str() == format!("alpha2.transition.{name}"))
            .unwrap();
        let params = edge_test_params(name);
        let size = RenderSize::new(64, 64).unwrap();
        let out = renderer
            .render_effect(
                id,
                &a,
                Some(&b),
                size,
                &params,
                EffectRuntime {
                    progress: 0.303,
                    ..Default::default()
                },
            )
            .unwrap();
        let pixels = read_rgba8(&renderer, &out);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] > 1 && p[3] < 254),
            "{name} has no edge AA"
        );
        for pixel in pixels.as_chunks::<4>().0 {
            assert_rgba8_near(pixel, &[pixel[3]; 4], 1);
        }
    }
}

#[test]
fn wipe_coverage_is_one_pixel_wide_at_multiple_resolutions() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let id = ids
        .iter()
        .find(|id| id.as_str().ends_with(".wipe-right"))
        .unwrap();
    let from = rgba_texture(&renderer, [0; 4]);
    let to = rgba_texture(&renderer, [255; 4]);
    let a = from.create_view(&wgpu::TextureViewDescriptor::default());
    let b = to.create_view(&wgpu::TextureViewDescriptor::default());
    for width in [32, 128] {
        let size = RenderSize::new(width, 8).unwrap();
        let out = renderer
            .render_effect(
                id,
                &a,
                Some(&b),
                size,
                &EffectParams::new(),
                EffectRuntime {
                    progress: 0.3,
                    ..Default::default()
                },
            )
            .unwrap();
        let pixels = read_rgba8(&renderer, &out);
        let row = &pixels[..usize::try_from(width).unwrap() * 4];
        let partial: Vec<_> = row
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 1 && p[3] < 254)
            .collect();
        assert_eq!(partial.len(), 1);
        let expected = if width == 32 { 153 } else { 102 };
        assert_rgba8_near(partial[0], &[expected; 4], 1);
    }
}

#[test]
fn antialiasing_preserves_patterned_transition_endpoints() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let size = RenderSize::new(16, 16).unwrap();
    let from: Vec<u8> = (0..256)
        .flat_map(|i| {
            if i % 3 == 0 {
                [128, 64, 0, 128]
            } else {
                [0; 4]
            }
        })
        .collect();
    let to: Vec<u8> = (0..256)
        .flat_map(|i| {
            if i % 2 == 0 {
                [0, 32, 64, 64]
            } else {
                [255; 4]
            }
        })
        .collect();
    let a = rgba_image_texture(&renderer, size, &from);
    let b = rgba_image_texture(&renderer, size, &to);
    let a = a.create_view(&wgpu::TextureViewDescriptor::default());
    let b = b.create_view(&wgpu::TextureViewDescriptor::default());
    for id in &ids {
        for (progress, expected) in [(0.0, &from), (1.0, &to)] {
            let out = renderer
                .render_effect(
                    id,
                    &a,
                    Some(&b),
                    size,
                    &EffectParams::new(),
                    EffectRuntime {
                        progress,
                        ..EffectRuntime::default()
                    },
                )
                .unwrap();
            assert_rgba8_near(&read_rgba8(&renderer, &out), expected, 1);
        }
    }
}
