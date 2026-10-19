//! Bounded layer passes: where a layer draws, what is culled, and backgrounds and empty canvases.

use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn bounded_layer_passes_match_full_frame_passes_bit_for_bit() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    // A soft-edged, partly transparent source, so edges and blending show.
    let pixels = (0..32 * 32)
        .flat_map(|index| {
            let (x, y) = (index % 32, index / 32);
            let alpha = u8::try_from((x * 8).min(255)).expect("small");
            let red = u8::try_from((y * 8).min(255)).expect("small");
            [red / 2, 90 / 2, 200 / 2, alpha].map(|value| {
                u8::try_from(u16::from(value) * u16::from(alpha) / 255).expect("small")
            })
        })
        .collect::<Vec<_>>();
    let source = rgba_image_texture(&renderer, RenderSize::new(32, 32).expect("size"), &pixels);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let layer = |x: f32, y: f32, scale: f32, rotation: f32, blend_mode| CompositeLayer {
        texture: &view,
        visual: LayerTransform {
            position: [x, y],
            anchor: [0.5, 0.5],
            scale: [scale, scale * 1.3],
            rotation_degrees: rotation,
            opacity: 0.85,
        },
        blend_mode,
    };
    let scenes: Vec<Vec<CompositeLayer<'_>>> = vec![
        // Small layers, overlapping, rotated, in every blend mode.
        vec![
            layer(0.3, 0.4, 0.2, 0.0, BlendMode::Normal),
            layer(0.35, 0.45, 0.25, 30.0, BlendMode::Multiply),
            layer(0.7, 0.6, 0.15, -45.0, BlendMode::Screen),
            layer(0.5, 0.5, 0.3, 10.0, BlendMode::Overlay),
            layer(0.52, 0.48, 0.1, 80.0, BlendMode::Normal),
        ],
        // Partly off the target, one wholly off, and a full-frame layer.
        vec![
            layer(0.02, 0.5, 0.2, 15.0, BlendMode::Screen),
            layer(1.6, -0.4, 0.2, 0.0, BlendMode::Multiply),
            layer(0.5, 0.5, 1.2, 5.0, BlendMode::Overlay),
            layer(0.95, 0.95, 0.1, 0.0, BlendMode::Normal),
        ],
        vec![layer(-1.0, -1.0, 0.2, 0.0, BlendMode::Normal)],
        vec![layer(0.8, 0.2, 0.1, 0.0, BlendMode::Multiply)],
        // Invisible layers between visible ones, full-frame and bounded.
        vec![
            layer(0.5, 0.5, 1.2, 0.0, BlendMode::Normal),
            CompositeLayer {
                visual: LayerTransform {
                    opacity: 0.0,
                    ..layer(0.5, 0.5, 1.4, 0.0, BlendMode::Normal).visual
                },
                ..layer(0.5, 0.5, 1.4, 0.0, BlendMode::Multiply)
            },
            layer(0.3, 0.3, 0.2, 20.0, BlendMode::Screen),
            CompositeLayer {
                visual: LayerTransform {
                    opacity: 0.0,
                    ..layer(0.7, 0.7, 0.2, 0.0, BlendMode::Normal).visual
                },
                ..layer(0.7, 0.7, 0.2, 0.0, BlendMode::Difference)
            },
            layer(0.75, 0.7, 0.15, 0.0, BlendMode::Normal),
        ],
    ];
    // An odd size puts the target's last quads half outside it.
    for size in [
        RenderSize::new(96, 64).expect("size"),
        RenderSize::new(77, 51).expect("size"),
    ] {
        for blend_space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
            renderer.blend_space = blend_space;
            for (index, scene) in scenes.iter().enumerate() {
                for background in [None, Some(Color::rgba(0.2, 0.4, 0.6, 1.0))] {
                    let render = |full_frame: bool| {
                        renderer
                            .full_frame_layers
                            .store(full_frame, std::sync::atomic::Ordering::Relaxed);
                        let mut graph = RenderGraph::new(size);
                        let layers = scene
                            .iter()
                            .map(|layer| GraphLayer {
                                source: graph.add_source(SourceView::from_wgpu(layer.texture)),
                                visual: layer.visual,
                                blend_mode: layer.blend_mode,
                            })
                            .collect::<Vec<_>>();
                        let output = match background {
                            Some(background) => graph.add_composite_over(layers, background),
                            None => graph.add_composite(layers),
                        }
                        .expect("composite");
                        graph.set_output(output).expect("output");
                        let output = renderer.execute_graph(&graph).expect("executes");
                        let pixels = read_rgba8(&renderer, &output);
                        (output, pixels)
                    };
                    let (reference_texture, reference) = render(true);
                    // Bounded renders draw into their own textures; given the reference's by the
                    // pool, a bounded render that ran nothing would still match.
                    renderer.clear_texture_pool();
                    let (bounded_texture, bounded) = render(false);
                    assert_ne!(reference_texture.texture, bounded_texture.texture);
                    drop((reference_texture, bounded_texture));
                    let first_difference = reference
                        .iter()
                        .zip(&bounded)
                        .position(|(left, right)| left != right);
                    assert_eq!(
                        first_difference, None,
                        "{size:?} {blend_space:?}, scene {index}, background {background:?}"
                    );
                }
            }
        }
    }
    renderer
        .full_frame_layers
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_small_layer_over_a_full_frame_one_draws_where_it_is_on_a_fresh_renderer() {
    // The bounded path draws over a copy of the region it covers; a fresh renderer keeps a pass
    // that wrote nothing from matching a pooled picture.
    let _lock = gpu_test_lock();
    let size = RenderSize::new(64, 32).expect("size");
    let colors = [
        [40_u8, 90, 200, 255],
        [220, 60, 30, 255],
        [30, 200, 90, 255],
    ];
    // (centre, scale), normalized; the edges fall on pixel boundaries.
    let placements = [
        ([0.5, 0.5], [1.0, 1.0]),
        // x 8..24, y 8..16.
        ([0.25, 0.375], [0.25, 0.25]),
        // x 56..72, y 24..40: clamped to 56..64, 24..32.
        ([1.0, 1.0], [0.25, 0.5]),
    ];
    let expected = (0..size.height)
        .flat_map(|y| {
            (0..size.width).flat_map(move |x| {
                if (56..64).contains(&x) && (24..32).contains(&y) {
                    colors[2]
                } else if (8..24).contains(&x) && (8..16).contains(&y) {
                    colors[1]
                } else {
                    colors[0]
                }
            })
        })
        .collect::<Vec<_>>();
    for (gpu, blend_space) in gpus_by_intermediate().into_iter().flat_map(|gpu| {
        [BlendSpace::SrgbEncoded, BlendSpace::LinearLight].map(|space| (gpu.clone(), space))
    }) {
        let renderer = Renderer::new(gpu).with_blend_space(blend_space);
        let format = renderer.intermediate_format();
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let textures =
            colors.map(|color| rgba_image_texture(&renderer, size, &color.repeat(64 * 32)));
        let views = textures
            .iter()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect::<Vec<_>>();
        let mut graph = RenderGraph::new(size);
        let layers = views
            .iter()
            .zip(placements)
            .map(|(view, (position, scale))| GraphLayer {
                source: graph.add_source(SourceView::from_wgpu(view)),
                visual: LayerTransform {
                    position,
                    ..visual(scale, 1.0)
                },
                blend_mode: BlendMode::Normal,
            })
            .collect::<Vec<_>>();
        let output = graph.add_composite(layers).expect("composite");
        graph.set_output(output).expect("output");
        let pixels = read_rgba8(
            &renderer,
            &renderer.execute_graph(&graph).expect("executes"),
        );
        let worst = pixels
            .iter()
            .zip(&expected)
            .map(|(got, want)| got.abs_diff(*want))
            .max()
            .expect("pixels");
        assert!(
            worst <= 1,
            "{format:?} {blend_space:?}: first difference at pixel {:?}",
            pixels
                .iter()
                .zip(&expected)
                .position(|(got, want)| got.abs_diff(*want) > 1)
                .map(|index| (index / 4 % 64, index / 4 / 64))
        );
        assert!(pollster::block_on(scope.pop()).is_none());
    }
}

#[test]
fn a_layer_with_no_opacity_costs_no_pass_and_leaves_the_picture_as_without_it() {
    // Inactive mattes and finished fades reach opacity 0; drawing them costs a full-frame pass
    // and makes the next bounded layer copy the whole frame, so they are skipped.
    let _lock = gpu_test_lock();
    for gpu in gpus_by_intermediate() {
        let renderer = Renderer::new(gpu);
        let format = renderer.intermediate_format();
        let size = RenderSize::new(64, 48).expect("size");
        let pixels = (0..64 * 48)
            .flat_map(|index| {
                let (x, y) = (index % 64, index / 64);
                [
                    u8::try_from(x * 4).expect("small"),
                    u8::try_from(y * 5).expect("small"),
                    160,
                    255,
                ]
            })
            .collect::<Vec<_>>();
        let gradient = rgba_image_texture(&renderer, size, &pixels);
        let gradient_view = gradient.create_view(&wgpu::TextureViewDescriptor::default());
        let badge = rgba_texture(&renderer, [200, 40, 20, 255]);
        let badge_view = badge.create_view(&wgpu::TextureViewDescriptor::default());
        let background = Color::rgba(0.2, 0.3, 0.4, 1.0);
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        // (texture, scale, opacity, blend mode) from the bottom up.
        let render = |layers: &[(&wgpu::TextureView, f32, f32, BlendMode)]| {
            let mut graph = RenderGraph::new(size);
            let layers = layers
                .iter()
                .map(|(texture, scale, opacity, blend_mode)| GraphLayer {
                    source: graph.add_source(SourceView::from_wgpu(texture)),
                    visual: LayerTransform {
                        position: [0.3, 0.6],
                        ..visual([*scale, *scale], *opacity)
                    },
                    blend_mode: *blend_mode,
                })
                .collect();
            let output = graph
                .add_composite_over(layers, background)
                .expect("composite");
            graph.set_output(output).expect("output");
            let before = graph_counts(&renderer);
            let pixels = read_rgba8(
                &renderer,
                &renderer.execute_graph(&graph).expect("executes"),
            );
            (pixels, renderer.diagnostics().graph_passes - before.1)
        };
        let full = (&gradient_view, 2.0, 1.0, BlendMode::Normal);
        let hidden = (&gradient_view, 2.0, 0.0, BlendMode::Multiply);
        let small = (&badge_view, 0.2, 1.0, BlendMode::Normal);
        let hidden_small = (&badge_view, 0.25, 0.0, BlendMode::Screen);
        let (shown, shown_passes) = render(&[full, small]);
        let (with_hidden, passes) = render(&[full, hidden, hidden_small, small]);
        assert_eq!(shown_passes, 2, "{format:?}");
        assert_eq!(passes, 2, "{format:?}: the invisible layers drew passes");
        assert!(
            with_hidden == shown,
            "{format:?}: invisible layers changed the picture"
        );
        // Nothing visible: the canvas is the background, as with no layers at all.
        let (only_hidden, passes) = render(&[hidden, hidden_small]);
        let (empty, _) = render(&[]);
        assert_eq!(passes, 1, "{format:?}: one clear");
        assert!(only_hidden == empty, "{format:?}");
        assert!(pollster::block_on(scope.pop()).is_none());
    }
}

#[test]
fn layer_bounds_cover_the_transformed_rectangle_and_cull_what_misses() {
    let size = RenderSize::new(100, 50).expect("size");
    let visual = |position: [f32; 2], scale: f32, rotation_degrees: f32| LayerTransform {
        position,
        anchor: [0.5, 0.5],
        scale: [scale, scale],
        rotation_degrees,
        opacity: 1.0,
    };
    // The bounds hold the transformed rectangle plus the 4-pixel margin, and at most one
    // more pixel of rounding on each side.
    let assert_bounds = |visual, (left, top, right, bottom): (u32, u32, u32, u32)| {
        let bounds = crate::composite::layer_bounds(visual, size).expect("on the target");
        let (x1, y1) = (bounds.x + bounds.width, bounds.y + bounds.height);
        assert!(
            bounds.x <= left && bounds.y <= top && x1 >= right && y1 >= bottom,
            "{bounds:?} misses {:?}",
            (left, top, right, bottom)
        );
        assert!(
            left - bounds.x <= 1 && top - bounds.y <= 1 && x1 - right <= 1 && y1 - bottom <= 1,
            "{bounds:?} is looser than {:?}",
            (left, top, right, bottom)
        );
    };
    // A centred fifth of the frame: 20 by 10 pixels.
    assert_bounds(visual([0.5, 0.5], 0.2, 0.0), (36, 16, 64, 34));
    // Turned a quarter, 10 by 20 pixels: the scale is per normalized axis, so rotating in
    // pixel space swaps the extents.
    assert_bounds(visual([0.5, 0.5], 0.2, 90.0), (41, 11, 59, 39));
    assert_bounds(visual([0.5, 0.5], 3.0, 0.0), (0, 0, 100, 50));
    assert_eq!(
        crate::composite::layer_bounds(visual([-0.5, 0.5], 0.2, 0.0), size),
        None
    );
}

#[test]
fn a_composite_over_a_background_treats_it_as_the_bottom_layer() {
    let _lock = gpu_test_lock();
    for gpu in gpus_by_intermediate() {
        composite_over_a_background_on(gpu);
    }
}

fn composite_over_a_background_on(gpu: GpuContext) {
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(2, 2).expect("size");
    // 0.25 and 0.6 store 63.75 and 153, clear of a half step on every GPU.
    let background = Color::rgba(0.25, 0.6, 1.0, 1.0);

    let mut empty = RenderGraph::new(size);
    let output = empty
        .add_composite_over(Vec::new(), background)
        .expect("no dependencies");
    empty.set_output(output).expect("output exists");
    let pixels = read_rgba8(
        &renderer,
        &renderer.execute_graph(&empty).expect("executes"),
    );
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [64, 153, 255, 255])
    );

    // A multiplied layer blends against the background, not against nothing.
    let gray = rgba_image_texture(&renderer, size, &[188, 188, 188, 255].repeat(4));
    let gray_view = gray.create_view(&wgpu::TextureViewDescriptor::default());
    let mut multiplied = RenderGraph::new(size);
    let source = multiplied.add_source(SourceView::from_wgpu(&gray_view));
    let layers = vec![GraphLayer {
        source,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Multiply,
    }];
    let output = multiplied
        .add_composite_over(layers.clone(), background)
        .expect("dependency exists");
    multiplied.set_output(output).expect("output exists");
    let pixels = read_rgba8(
        &renderer,
        &renderer.execute_graph(&multiplied).expect("executes"),
    );
    let decode = |code: u8| {
        let value = f32::from(code) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let encode = |value: f32| {
        let value = if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        quantize_unorm8(value)
    };
    let expected = [64, 153, 255].map(|code| encode(decode(code) * decode(188)));
    assert!(
        pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255
            && pixel[..3]
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1)),
        "{pixels:?} != {expected:?}"
    );

    // The background is part of what the node computes, so it is part of its key; an
    // out-of-range one is refused before it reaches the GPU.
    let mut transparent = RenderGraph::new(size);
    let source = transparent.add_source(SourceView::from_wgpu(&gray_view));
    let output = transparent
        .add_composite(vec![GraphLayer {
            source,
            ..layers[0]
        }])
        .expect("dependency exists");
    transparent.set_output(output).expect("output exists");
    assert_ne!(
        renderer.graph_cache_key(&transparent),
        renderer.graph_cache_key(&multiplied)
    );
    assert_eq!(
        RenderGraph::new(size).add_composite_over(Vec::new(), Color::rgba(2.0, 0.0, 0.0, 1.0)),
        Err(GraphError::Render(RenderError::InvalidColor))
    );
}

#[test]
fn an_empty_canvas_stores_the_background_as_a_canvas_with_layers_does() {
    // The clear must store the background as `stored_background` quantized it: a device-quantized
    // clear differs by a code on ties (0.7 is 178.49999 in f32) on GPUs that round ties down,
    // and the canvas would step at every clip boundary.
    let _lock = gpu_test_lock();
    for gpu in gpus_by_intermediate() {
        let renderer = Renderer::new(gpu);
        let format = renderer.intermediate_format();
        let size = RenderSize::new(8, 8).expect("size");
        let white = rgba_image_texture(&renderer, size, &[255; 4].repeat(64));
        let white_view = white.create_view(&wgpu::TextureViewDescriptor::default());
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        for level in [0.3, 0.5, 0.7, 0.9, 0.2, 0.4] {
            let background = Color::rgba(level, level * 0.5, 1.0 - level, 1.0);
            let canvas = |layered: bool| {
                let mut graph = RenderGraph::new(size);
                let layers = if layered {
                    // A quarter-size layer in the middle; the corner is background.
                    vec![GraphLayer {
                        source: graph.add_source(SourceView::from_wgpu(&white_view)),
                        visual: visual([0.25, 0.25], 1.0),
                        blend_mode: BlendMode::Normal,
                    }]
                } else {
                    Vec::new()
                };
                let output = graph
                    .add_composite_over(layers, background)
                    .expect("composite");
                graph.set_output(output).expect("output");
                read_rgba8(
                    &renderer,
                    &renderer.execute_graph(&graph).expect("executes"),
                )
            };
            let (empty, layered) = (canvas(false), canvas(true));
            assert_eq!(
                layered[4 * 27..4 * 28],
                [255; 4],
                "{format:?}: the layer is drawn"
            );
            assert_eq!(
                empty[..4],
                layered[..4],
                "{format:?}, background {level}: an empty canvas and the corner beside a layer"
            );
            assert!(
                empty
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel == &empty[..4]),
                "{format:?}: the empty canvas is one color"
            );
        }
        assert!(pollster::block_on(scope.pop()).is_none());
    }
}
