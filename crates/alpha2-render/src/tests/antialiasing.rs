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
    // A half-pixel square on a pixel corner covers four pixels equally; a centre-only test would
    // lose it.
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

/// Parameters that make `name`'s edge a hard one between plain black and white pictures.
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

/// Alternating opaque black and white columns, `width` x 8.
fn columns(width: u32) -> Vec<u8> {
    (0..8)
        .flat_map(|_| (0..width).flat_map(|x| if x % 2 == 0 { [0, 0, 0, 255] } else { [255; 4] }))
        .collect()
}

fn at_x(position_x: f32) -> LayerTransform {
    LayerTransform {
        position: [position_x, 0.5],
        ..visual([1.0, 1.0], 1.0)
    }
}

/// The first column with a channel more than `tolerance` off, if any.
fn first_wrong_column(width: u32, drawn: &[u8], expected: &[u8], tolerance: u8) -> Option<u32> {
    let row = (width * 4) as usize;
    (0..8)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .find(|&(x, y)| {
            let at = y * row + x as usize * 4;
            (0..4).any(|channel| drawn[at + channel].abs_diff(expected[at + channel]) > tolerance)
        })
        .map(|(x, _)| x)
}

#[test]
fn a_layer_drawn_one_to_one_keeps_every_column_at_any_width() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let largest = renderer.gpu.capabilities().max_texture_dimension_2d;
    let composite = |source: RenderSize, canvas: RenderSize, bytes: &[u8], visual| {
        let texture = rgba_image_texture(&renderer, source, bytes);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let output = renderer
            .render_layers(
                canvas,
                &[CompositeLayer {
                    texture: &view,
                    visual,
                    blend_mode: BlendMode::Normal,
                }],
            )
            .expect("draws");
        read_rgba8(&renderer, &output)
    };
    for width in [16_u32, 1280, 1920, 2560, 3840, 4096, 7680] {
        if width > largest {
            eprintln!("skipped: width {width} is past this adapter's {largest}-texel textures");
            continue;
        }
        let size = RenderSize::new(width, 8).expect("size");
        let bytes = columns(width);
        let drawn = composite(size, size, &bytes, at_x(0.5));
        // Past 4096 texels some GPUs' fixed-point bilinear weights miss a texel centre by a code.
        let tolerance = u8::from(width > 4096);
        assert_eq!(
            first_wrong_column(width, &drawn, &bytes, tolerance),
            None,
            "{width} wide"
        );
    }
    // At x 0.75 a layer moves a quarter of the canvas right, a whole number of pixels.
    for width in [1920_u32, 3840] {
        let size = RenderSize::new(width, 8).expect("size");
        let shift = width / 4;
        let expected: Vec<u8> = (0..8)
            .flat_map(|_| {
                (0..width).flat_map(move |x| match x {
                    x if x < shift => [0; 4],
                    x if (x - shift) % 2 == 0 => [0, 0, 0, 255],
                    _ => [255; 4],
                })
            })
            .collect();
        let drawn = composite(size, size, &columns(width), at_x(0.75));
        assert_eq!(
            first_wrong_column(width, &drawn, &expected, 0),
            None,
            "{width} at x 0.75"
        );
    }
    // Reduced 2:1, every pixel averages one black and one white column.
    for (source_width, width) in [(1920_u32, 960_u32), (3840, 1920), (7680, 3840)] {
        if source_width > largest {
            eprintln!(
                "skipped: width {source_width} is past this adapter's {largest}-texel textures"
            );
            continue;
        }
        let drawn = composite(
            RenderSize::new(source_width, 8).expect("size"),
            RenderSize::new(width, 8).expect("size"),
            &columns(source_width),
            at_x(0.5),
        );
        for (index, pixel) in drawn.as_chunks::<4>().0.iter().enumerate() {
            assert!(
                pixel[..3]
                    .iter()
                    .all(|channel| matches!(channel, 127 | 128))
                    && pixel[3] == 255,
                "{source_width} to {width}, pixel {index}: {pixel:?}"
            );
        }
    }
}

#[test]
fn the_cpu_renderer_draws_one_to_one_layers_exactly_at_any_width() {
    use std::sync::Arc;
    let cpu = crate::CpuRenderer::new(1);
    for width in [16_u32, 1920, 3840, 4096, 7680] {
        let size = RenderSize::new(width, 8).expect("size");
        let bytes = columns(width);
        let source = crate::CpuSource::image(Arc::new(
            crate::CpuImage::from_premultiplied_rgba8(size, &bytes).expect("picture"),
        ));
        let mut graph = RenderGraph::new(size);
        let node = graph.add_source(SourceView::cpu(&source));
        let output = graph
            .add_composite(vec![GraphLayer {
                source: node,
                visual: at_x(0.5),
                blend_mode: BlendMode::Normal,
            }])
            .expect("composite");
        graph.set_output(output).expect("output");
        let drawn = cpu.execute_graph(&graph).expect("CPU draws").read_rgba8();
        assert_eq!(
            first_wrong_column(width, &drawn, &bytes, 0),
            None,
            "{width} wide"
        );
    }
}
