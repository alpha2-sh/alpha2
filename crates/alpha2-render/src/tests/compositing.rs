//! Compositing: layer transforms, the first-layer specialization, uniform arenas, and blend modes against the W3C reference.

use super::*;

#[test]
fn premultiplied_source_over_matches_porter_duff() {
    let destination = Color::rgba(0.0, 0.0, 0.5, 0.5);
    let source = Color::rgba(0.5, 0.0, 0.0, 0.5);
    assert_eq!(
        destination.source_over(source),
        Color::rgba(0.5, 0.0, 0.25, 0.75)
    );
}

#[test]
fn quad_shader_pipeline_submits_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let renderer = Renderer::new(gpu);
    assert_eq!(renderer.pipeline_cache_stats().builtins, 0);
    let size = RenderSize::new(4, 4).expect("non-zero size");
    let target = renderer
        .render_quad(size, [64, 128, 192, 255])
        .expect("quad");
    let pixels = read_rgba8(&renderer, &target);
    assert_rgba8_golden("texture_quad", size, &pixels, 0);
    assert_eq!(
        renderer.pipeline_cache_stats().builtins,
        1 + rgba_readback_pipeline_count(&renderer)
    );
    let _second = renderer
        .render_quad(size, [64, 128, 192, 255])
        .expect("quad");
    assert_eq!(
        renderer.pipeline_cache_stats().builtins,
        1 + rgba_readback_pipeline_count(&renderer)
    );
}

#[test]
fn solid_pass_writes_expected_rgba_pixels_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(4, 4).expect("non-zero size");
    // No channel on a half step: 0.5 would store 127.5, which GPUs round either way.
    let target = renderer
        .render_solid(size, Color::rgba(1.0, 0.6, 0.0, 1.0))
        .expect("valid clear pass");
    let pixels = read_rgba8(&renderer, &target);
    assert_rgba8_golden("solid_rgba", size, &pixels, 0);
}

#[test]
fn transformed_layers_match_the_composition_golden_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let renderer = Renderer::new(gpu);
    let red = rgba_texture(&renderer, [255, 0, 0, 255]);
    let blue = rgba_texture(&renderer, [0, 0, 255, 255]);
    let red_view = red.create_view(&wgpu::TextureViewDescriptor::default());
    let blue_view = blue.create_view(&wgpu::TextureViewDescriptor::default());
    let identity = visual([1.0, 1.0], 1.0);
    let centered_half = visual([0.5, 0.5], 0.5);
    let size = RenderSize::new(4, 4).expect("non-zero size");
    let target = renderer
        .render_layers(
            size,
            &[
                CompositeLayer {
                    texture: &red_view,
                    visual: identity,
                    blend_mode: BlendMode::Normal,
                },
                CompositeLayer {
                    texture: &blue_view,
                    visual: centered_half,
                    blend_mode: BlendMode::Normal,
                },
            ],
        )
        .expect("valid layers render");
    let pixels = read_rgba8(&renderer, &target);
    assert_rgba8_golden("layer_transform_opacity", size, &pixels, 1);
}

#[test]
fn first_layer_specialization_matches_compositing_over_transparent() {
    let _lock = gpu_test_lock();
    let size = RenderSize::new(4, 3).expect("non-zero size");
    let source_pixels = [
        [1, 0, 0, 1],
        [32, 16, 8, 64],
        [64, 32, 16, 128],
        [255, 128, 32, 255],
    ]
    .into_iter()
    .cycle()
    .take((size.width * size.height) as usize)
    .flatten()
    .collect::<Vec<_>>();
    let transparent_pixels = vec![0; source_pixels.len()];

    for (gpu, blend_space) in gpus_by_intermediate().into_iter().flat_map(|gpu| {
        [BlendSpace::LinearLight, BlendSpace::SrgbEncoded].map(|space| (gpu.clone(), space))
    }) {
        let renderer = Renderer::new(gpu).with_blend_space(blend_space);
        let source = rgba_image_texture(&renderer, size, &source_pixels);
        let transparent = rgba_image_texture(&renderer, size, &transparent_pixels);
        let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
        let transparent_view = transparent.create_view(&wgpu::TextureViewDescriptor::default());
        let transparent_layer = CompositeLayer {
            texture: &transparent_view,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        };

        for blend_mode in [
            BlendMode::Normal,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
        ] {
            let mut transformed = visual([0.75, 0.6], 0.5);
            transformed.rotation_degrees = 17.0;
            let source_layer = CompositeLayer {
                texture: &source_view,
                visual: transformed,
                blend_mode,
            };

            let before = graph_counts(&renderer);
            let specialized = renderer
                .render_layers(size, &[source_layer])
                .expect("specialized first layer renders");
            assert_graph_delta(&renderer, before, 1, 1);

            let before = graph_counts(&renderer);
            let reference = renderer
                .render_layers(size, &[transparent_layer, source_layer])
                .expect("transparent backdrop reference renders");
            assert_graph_delta(&renderer, before, 1, 2);

            assert_rgba8_near(
                &read_rgba8(&renderer, &specialized),
                &read_rgba8(&renderer, &reference),
                1,
            );
        }
    }
}

#[test]
fn first_layer_specialization_preserves_out_of_range_effect_math() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let id = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, _src: vec4f) -> vec4f { return vec4f(2.0, -1.0, 0.5, 0.25); }",
    )))
    .expect("out-of-range effect compiles");
    let size = RenderSize::new(2, 2).expect("non-zero size");
    let input = rgba_texture(&renderer, [255; 4]);
    let input_view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let effect_output = renderer
        .render_effect(
            &id,
            &input_view,
            None,
            size,
            &EffectParams::new(),
            EffectRuntime {
                time: alpha2_time::MediaTime::ZERO,
                progress: 0.0,
                ratio: 1.0,
            },
        )
        .expect("out-of-range effect renders");
    let effect_view = effect_output
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let transparent = rgba_image_texture(&renderer, size, &[0; 16]);
    let transparent_view = transparent.create_view(&wgpu::TextureViewDescriptor::default());
    let source_layer = CompositeLayer {
        texture: &effect_view,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Overlay,
    };
    let transparent_layer = CompositeLayer {
        texture: &transparent_view,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    };

    let specialized = renderer
        .render_layers(size, &[source_layer])
        .expect("specialized effect layer renders");
    let reference = renderer
        .render_layers(size, &[transparent_layer, source_layer])
        .expect("transparent backdrop effect reference renders");
    assert_rgba8_near(
        &read_rgba8(&renderer, &specialized),
        &read_rgba8(&renderer, &reference),
        1,
    );
}

#[test]
fn graph_uniform_arena_reuses_only_submitted_buffers() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = rgba_texture(&renderer, [255, 64, 16, 255]);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let layer = CompositeLayer {
        texture: &source_view,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    };
    let size = RenderSize::new(2, 2).expect("non-zero size");

    let first = renderer
        .render_layers(size, &[layer, layer])
        .expect("first graph submits");
    let first_pixels = read_rgba8(&renderer, &first);
    let first_stats = renderer.uniform_buffers.stats();
    assert_eq!(first_stats.created, 1);
    assert_eq!(first_stats.reused, 0);
    assert_eq!(first_stats.active, 0);
    assert_eq!(first_stats.available, 1);

    let second = renderer
        .render_layers(size, &[layer, layer])
        .expect("second graph submits");
    assert_eq!(read_rgba8(&renderer, &second), first_pixels);
    let second_stats = renderer.uniform_buffers.stats();
    assert_eq!(second_stats.created, 1);
    assert_eq!(second_stats.reused, 1);
    assert_eq!(second_stats.discarded, 0);

    let invalid = CompositeLayer {
        visual: visual([1.0, 1.0], 2.0),
        ..layer
    };
    assert!(matches!(
        renderer.render_layers(size, &[invalid]),
        Err(RenderError::InvalidVisual)
    ));
    let failed_stats = renderer.uniform_buffers.stats();
    assert_eq!(failed_stats.active, 0);
    assert_eq!(failed_stats.discarded, 1);
}

#[test]
fn uniform_arena_is_shared_across_source_effect_and_pack_submissions() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let chroma_key = pollster::block_on(renderer.register_builtin_chroma_key())
        .expect("built-in chroma key compiles");
    let size = RenderSize::new(2, 2).expect("non-zero size");
    let source = renderer
        .upload_rgba8(size, &[128, 64, 32, 255].repeat(4))
        .expect("RGBA source uploads");
    let presented = renderer
        .render_rgba_presented(
            source.texture(),
            size,
            SourcePresentation::identity(size),
            crate::SourceAlpha::Straight,
        )
        .expect("source presentation submits");
    let first_stats = renderer.uniform_buffers.stats();
    assert_eq!(first_stats.created, 1);
    assert_eq!(first_stats.reused, 0);

    let presented_view = presented
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let key = |source: &wgpu::TextureView| {
        renderer
            .render_effect(
                &chroma_key,
                source,
                None,
                size,
                &chroma_key_params([0.0, 1.0, 0.0], 0.1, 0.1, 1.0),
                EffectRuntime::default(),
            )
            .expect("effect submits")
    };
    // An effect's graph leases a larger arena than a presentation's: one of its own.
    let keyed = key(&presented_view);
    let effect_stats = renderer.uniform_buffers.stats();
    assert_eq!(effect_stats.created, 2);
    assert_eq!(effect_stats.reused, 0);

    renderer
        .begin_yuv420p_readback(&keyed, centred(ColorMetadata::default()))
        .expect("YUV pack submits")
        .wait()
        .expect("YUV pack maps");
    assert_eq!(renderer.uniform_buffers.stats().reused, 1);

    let keyed_view = keyed
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    drop(key(&keyed_view));
    let final_stats = renderer.uniform_buffers.stats();
    assert_eq!(final_stats.created, 2);
    assert_eq!(final_stats.reused, 2);
    assert_eq!(final_stats.discarded, 0);
    assert_eq!(final_stats.active, 0);
    assert_eq!(final_stats.available, 2);
}

#[test]
fn rotation_preserves_pixel_geometry_on_non_square_outputs() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = rgba_texture(&renderer, [255, 255, 255, 255]);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());

    for size in [
        RenderSize::new(160, 90).expect("landscape size"),
        RenderSize::new(90, 160).expect("portrait size"),
    ] {
        for scale in [[0.4, 0.4], [0.4, 0.2]] {
            let render = |rotation_degrees| {
                let mut transform = visual(scale, 1.0);
                transform.rotation_degrees = rotation_degrees;
                let target = renderer
                    .render_layers(
                        size,
                        &[CompositeLayer {
                            texture: &source_view,
                            visual: transform,
                            blend_mode: BlendMode::Normal,
                        }],
                    )
                    .expect("rotated layer renders");
                alpha_bounds(size, &read_rgba8(&renderer, &target))
            };

            let unrotated = render(0.0);
            let quarter_turn = render(90.0);
            assert_near(quarter_turn.0, unrotated.1, 2);
            assert_near(quarter_turn.1, unrotated.0, 2);

            let diagonal = render(45.0);
            assert_near(diagonal.0, diagonal.1, 2);
            let expected_diagonal =
                ((f64::from(unrotated.0) + f64::from(unrotated.1)) / 2.0_f64.sqrt()).round();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let expected_diagonal = expected_diagonal as u32;
            assert_near(diagonal.0, expected_diagonal, 2);
            assert_near(diagonal.1, expected_diagonal, 2);
        }
    }
}

#[test]
fn a_zero_scale_layer_draws_nothing_and_opacity_out_of_range_is_rejected() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let renderer = Renderer::new(gpu);
    let texture = rgba_texture(&renderer, [255, 255, 255, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let layer = |visual| CompositeLayer {
        texture: &view,
        visual,
        blend_mode: BlendMode::Normal,
    };
    // A scale animated through zero is an edge-on instant, not an error.
    for scale in [[0.0, 1.0], [1.0, 0.0], [-0.0, -0.0]] {
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let output = renderer
            .render_layers(size, &[layer(visual(scale, 1.0))])
            .expect("a degenerate layer renders");
        assert_eq!(
            read_rgba8(&renderer, &output),
            vec![0, 0, 0, 0],
            "{scale:?}"
        );
        assert!(pollster::block_on(scope.pop()).is_none(), "{scale:?}");
    }
    let output = renderer
        .render_layers(
            size,
            &[
                layer(visual([1.0, 1.0], 1.0)),
                layer(visual([0.0, 1.0], 1.0)),
            ],
        )
        .expect("a visible layer under a degenerate one");
    assert_eq!(read_rgba8(&renderer, &output), vec![255, 255, 255, 255]);

    for opacity in [-0.01, 1.01, f32::NAN] {
        assert!(matches!(
            renderer.render_layers(size, &[layer(visual([1.0, 1.0], opacity))]),
            Err(RenderError::InvalidVisual)
        ));
    }
}

#[test]
fn blend_space_ab_outputs_are_explicit_and_distinct() {
    let _lock = gpu_test_lock();
    let linear_renderer = Renderer::new(gpu()).with_blend_space(BlendSpace::LinearLight);
    let linear = composite_pixel(
        &linear_renderer,
        [0, 0, 0, 255],
        [255, 255, 255, 255],
        0.5,
        BlendMode::Normal,
    );
    let encoded_renderer = Renderer::new(gpu()).with_blend_space(BlendSpace::SrgbEncoded);
    let encoded = composite_pixel(
        &encoded_renderer,
        [0, 0, 0, 255],
        [255, 255, 255, 255],
        0.5,
        BlendMode::Normal,
    );
    assert_eq!(linear_renderer.blend_space(), BlendSpace::LinearLight);
    assert_eq!(encoded_renderer.blend_space(), BlendSpace::SrgbEncoded);
    assert!(linear[0] > encoded[0]);
    let pixels = [linear, encoded].concat();
    assert_rgba8_golden(
        "blend_space_ab",
        RenderSize::new(2, 1).expect("non-zero size"),
        &pixels,
        1,
    );
}

#[test]
fn artistic_blend_modes_match_srgb_reference() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu()).with_blend_space(BlendSpace::SrgbEncoded);
    let backdrop = [128, 64, 32, 255];
    let source = [64, 128, 192, 255];
    let modes = &BlendMode::ALL[1..];
    let pixels = modes
        .iter()
        .flat_map(|mode| composite_pixel(&renderer, backdrop, source, 1.0, *mode))
        .collect::<Vec<_>>();
    assert_rgba8_golden(
        "blend_modes",
        RenderSize::new(u32::try_from(modes.len()).expect("small"), 1).expect("non-zero size"),
        &pixels,
        1,
    );
}

/// Every blend mode on the GPU against the CPU reference of W3C Compositing
/// and Blending Level 1 (`blend_reference.rs`), in both blend spaces, over
/// ordinary colors and each mode's singular inputs, opaque and at half
/// opacity.
#[test]
fn every_blend_mode_matches_the_w3c_reference_on_the_gpu() {
    use super::blend_reference::composite_opaque;

    let _lock = gpu_test_lock();
    let pairs: [([u8; 3], [u8; 3]); 7] = [
        ([128, 64, 32], [64, 128, 192]),
        ([230, 26, 153], [77, 204, 51]),
        ([51, 179, 102], [153, 89, 242]),
        // Color dodge under white, burn under black.
        ([0, 128, 255], [255, 255, 255]),
        ([255, 128, 0], [0, 0, 0]),
        // A gray source: no saturation for the non-separable modes.
        ([204, 51, 102], [128, 128, 128]),
        // The soft-light branch at a backdrop of 0.25 and below.
        ([32, 64, 16], [200, 220, 240]),
    ];
    let unit = |pixel: [u8; 3]| pixel.map(|c| f64::from(c) / 255.0);
    for (gpu, space) in gpus_by_intermediate().into_iter().flat_map(|gpu| {
        [BlendSpace::SrgbEncoded, BlendSpace::LinearLight].map(|space| (gpu.clone(), space))
    }) {
        let renderer = Renderer::new(gpu).with_blend_space(space);
        let format = renderer.intermediate_format();
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        for mode in BlendMode::ALL {
            for (backdrop, source) in pairs {
                let blended = composite_opaque(mode, space, unit(backdrop), unit(source));
                for opacity in [1.0_f32, 0.5] {
                    let pixel = composite_pixel(
                        &renderer,
                        [backdrop[0], backdrop[1], backdrop[2], 255],
                        [source[0], source[1], source[2], 255],
                        opacity,
                        mode,
                    );
                    // Half opacity mixes the blend result with the backdrop in the blend space.
                    let expected = if opacity < 1.0 {
                        let mix =
                            composite_opaque(BlendMode::Normal, space, unit(backdrop), blended);
                        mixed_half(space, unit(backdrop), mix)
                    } else {
                        blended
                    };
                    for channel in 0..3 {
                        let got = f64::from(pixel[channel]) / 255.0;
                        // Compared in the blend space: near black the sRGB curve magnifies a float
                        // storage step thirteenfold, so encoded codes would measure the storage.
                        let error = match space {
                            BlendSpace::SrgbEncoded => (got - expected[channel]).abs(),
                            BlendSpace::LinearLight => (srgb_to_linear_f64(got)
                                - srgb_to_linear_f64(expected[channel]))
                            .abs(),
                        };
                        assert!(
                            error <= 2.0 / 255.0,
                            "{format:?} {space:?} {mode:?} {backdrop:?} under {source:?} at {opacity}: \
                             channel {channel} is {}, reference {:.2}",
                            pixel[channel],
                            expected[channel] * 255.0
                        );
                    }
                    assert_eq!(pixel[3], 255);
                }
            }
        }
        assert!(pollster::block_on(scope.pop()).is_none(), "{space:?}");
    }
}

/// Every blend mode on the GPU against the W3C general compositing formula
/// (`blend_reference::composite`) over a backdrop at alpha 0.25 and 0.5, in
/// both blend spaces and intermediates, for an opaque and a half-opacity
/// source. A composition's merge has no background, so an artistic layer
/// over a translucent lower layer composites exactly this; over an opaque
/// backdrop the `as * (1 - ab) * Cs` term is zero, and over a transparent one
/// the blend term is, so neither checks how the two are weighted.
#[test]
fn every_blend_mode_matches_the_w3c_reference_over_a_translucent_backdrop() {
    use super::blend_reference::composite;

    let _lock = gpu_test_lock();
    let pairs: [([u8; 3], [u8; 3]); 4] = [
        ([128, 64, 32], [64, 128, 192]),
        ([230, 26, 153], [77, 204, 51]),
        ([51, 179, 102], [153, 89, 242]),
        ([204, 51, 102], [128, 128, 128]),
    ];
    let unit = |value: u8| f64::from(value) / 255.0;
    let mut worst = 0.0_f64;
    for (gpu, space) in gpus_by_intermediate().into_iter().flat_map(|gpu| {
        [BlendSpace::SrgbEncoded, BlendSpace::LinearLight].map(|space| (gpu.clone(), space))
    }) {
        let renderer = Renderer::new(gpu).with_blend_space(space);
        let format = renderer.intermediate_format();
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        // Compared as the blend ran: straight colors in the blend space, weighted by coverage.
        let in_space = |pixel: [f64; 4], channel: usize| {
            if pixel[3] <= 0.0 {
                return 0.0;
            }
            let straight = (pixel[channel] / pixel[3]).clamp(0.0, 1.0);
            pixel[3]
                * match space {
                    BlendSpace::SrgbEncoded => straight,
                    BlendSpace::LinearLight => srgb_to_linear_f64(straight),
                }
        };
        for mode in BlendMode::ALL {
            for (backdrop, source) in pairs {
                for backdrop_alpha in [64_u8, 128] {
                    // The backdrop texture holds premultiplied codes, as the intermediate does.
                    let premultiplied = backdrop.map(|channel| {
                        u8::try_from((u16::from(channel) * u16::from(backdrop_alpha) + 127) / 255)
                            .expect("a code")
                    });
                    let backdrop_pixel = [
                        premultiplied[0],
                        premultiplied[1],
                        premultiplied[2],
                        backdrop_alpha,
                    ];
                    for opacity in [1.0_f32, 0.5] {
                        let pixel = composite_pixel(
                            &renderer,
                            backdrop_pixel,
                            [source[0], source[1], source[2], 255],
                            opacity,
                            mode,
                        );
                        let o = f64::from(opacity);
                        let expected = composite(
                            mode,
                            space,
                            backdrop_pixel.map(unit),
                            [
                                unit(source[0]) * o,
                                unit(source[1]) * o,
                                unit(source[2]) * o,
                                o,
                            ],
                        );
                        let got = [0, 1, 2, 3].map(|channel| unit(pixel[channel]));
                        let context = || {
                            format!(
                                "{format:?} {space:?} {mode:?} {backdrop:?} at alpha {backdrop_alpha} \
                                 under {source:?} at {opacity}: {pixel:?}, reference {:?}",
                                expected.map(|value| value * 255.0)
                            )
                        };
                        assert!(
                            (got[3] - expected[3]).abs() <= 1.0 / 255.0,
                            "alpha: {}",
                            context()
                        );
                        for channel in 0..3 {
                            let error =
                                (in_space(got, channel) - in_space(expected, channel)).abs();
                            worst = worst.max(error);
                            assert!(error <= 2.0 / 255.0, "channel {channel}: {}", context());
                        }
                    }
                }
            }
        }
        assert!(pollster::block_on(scope.pop()).is_none(), "{space:?}");
    }
    eprintln!("largest error {:.3} codes", worst * 255.0);
}

/// Color and Luminosity take a gray to its target luminosity, which in
/// floating point can leave it a rounding error outside 0..1 with nothing to
/// clip. Over a transparent backdrop the blend's weight is zero, so the
/// source has to come through as it is, gray included.
#[test]
fn color_and_luminosity_pass_a_gray_through_a_transparent_backdrop() {
    let _lock = gpu_test_lock();
    let size = RenderSize::new(256, 1).expect("non-zero size");
    let grays: Vec<u8> = (0..=255_u8).flat_map(|g| [g, g, g, 255]).collect();
    for (gpu, space) in gpus_by_intermediate().into_iter().flat_map(|gpu| {
        [BlendSpace::SrgbEncoded, BlendSpace::LinearLight].map(|space| (gpu.clone(), space))
    }) {
        let renderer = Renderer::new(gpu).with_blend_space(space);
        let format = renderer.intermediate_format();
        let clear = rgba_image_texture(&renderer, size, &[0; 4].repeat(256));
        let source = rgba_image_texture(&renderer, size, &grays);
        let clear_view = clear.create_view(&wgpu::TextureViewDescriptor::default());
        let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
        for mode in [BlendMode::Color, BlendMode::Luminosity] {
            let target = renderer
                .render_layers(
                    size,
                    &[
                        CompositeLayer {
                            texture: &clear_view,
                            visual: visual([1.0, 1.0], 1.0),
                            blend_mode: BlendMode::Normal,
                        },
                        CompositeLayer {
                            texture: &source_view,
                            visual: visual([1.0, 1.0], 1.0),
                            blend_mode: mode,
                        },
                    ],
                )
                .expect("the layers render");
            let out = read_rgba8(&renderer, &target);
            for (gray, pixel) in out.as_chunks::<4>().0.iter().enumerate() {
                assert!(
                    pixel[..3]
                        .iter()
                        .all(|c| usize::from(*c).abs_diff(gray) <= 1)
                        && pixel[3] == 255,
                    "{format:?} {space:?} {mode:?}: gray {gray} came out {pixel:?}"
                );
            }
        }
    }
}

fn srgb_to_linear_f64(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Half of `top` over `backdrop`, mixed in `space` and stored sRGB-encoded.
fn mixed_half(space: BlendSpace, backdrop: [f64; 3], top: [f64; 3]) -> [f64; 3] {
    let to_linear = srgb_to_linear_f64;
    let to_srgb = |c: f64| {
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    [0, 1, 2].map(|channel| match space {
        BlendSpace::SrgbEncoded => 0.5 * top[channel] + 0.5 * backdrop[channel],
        BlendSpace::LinearLight => {
            to_srgb(0.5 * to_linear(top[channel]) + 0.5 * to_linear(backdrop[channel]))
        }
    })
}

fn composite_pixel(
    renderer: &Renderer,
    backdrop: [u8; 4],
    source: [u8; 4],
    opacity: f32,
    blend_mode: BlendMode,
) -> Vec<u8> {
    let backdrop = rgba_texture(renderer, backdrop);
    let source = rgba_texture(renderer, source);
    let backdrop_view = backdrop.create_view(&wgpu::TextureViewDescriptor::default());
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let target = renderer
        .render_layers(
            size,
            &[
                CompositeLayer {
                    texture: &backdrop_view,
                    visual: visual([1.0, 1.0], 1.0),
                    blend_mode: BlendMode::Normal,
                },
                CompositeLayer {
                    texture: &source_view,
                    visual: visual([1.0, 1.0], opacity),
                    blend_mode,
                },
            ],
        )
        .expect("valid pixel layers render");
    read_rgba8(renderer, &target)
}

fn alpha_bounds(size: RenderSize, pixels: &[u8]) -> (u32, u32) {
    let mut minimum = (size.width, size.height);
    let mut maximum = (0_u32, 0_u32);
    let mut found = false;
    for y in 0..size.height {
        for x in 0..size.width {
            let pixel = usize::try_from((y * size.width + x) * 4).expect("pixel offset");
            if pixels[pixel + 3] > 127 {
                minimum.0 = minimum.0.min(x);
                minimum.1 = minimum.1.min(y);
                maximum.0 = maximum.0.max(x);
                maximum.1 = maximum.1.max(y);
                found = true;
            }
        }
    }
    assert!(found, "rotated layer must cover output pixels");
    (maximum.0 - minimum.0 + 1, maximum.1 - minimum.1 + 1)
}
