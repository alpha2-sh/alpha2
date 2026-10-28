use super::*;

#[test]
fn transparent_builtin_transitions_preserve_associated_alpha() {
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
            if i % 5 == 0 {
                [0, 32, 64, 64]
            } else {
                [255; 4]
            }
        })
        .collect();
    let from = rgba_image_texture(&renderer, size, &from);
    let to = rgba_image_texture(&renderer, size, &to);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    for id in &ids {
        for progress in [0.001, 0.3, 0.5, 0.999] {
            let out = renderer
                .render_effect(
                    id,
                    &from_view,
                    Some(&to_view),
                    size,
                    &EffectParams::new(),
                    EffectRuntime {
                        progress,
                        ..Default::default()
                    },
                )
                .unwrap();
            for pixel in read_rgba8(&renderer, &out).as_chunks::<4>().0 {
                assert!(
                    pixel[..3].iter().all(|c| *c <= pixel[3].saturating_add(1)),
                    "{} p={progress}: {pixel:?}",
                    id.as_str()
                );
            }
        }
    }
}

#[test]
fn color_phase_preserves_color_when_coverage_changes() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let id = ids
        .iter()
        .find(|id| id.as_str().ends_with(".color-phase"))
        .unwrap();
    let from = rgba_texture(&renderer, [64; 4]);
    let to = rgba_texture(&renderer, [255; 4]);
    let out = renderer
        .render_effect(
            id,
            &from.create_view(&wgpu::TextureViewDescriptor::default()),
            Some(&to.create_view(&wgpu::TextureViewDescriptor::default())),
            RenderSize::new(1, 1).unwrap(),
            &EffectParams::new(),
            EffectRuntime {
                progress: 0.3,
                ..Default::default()
            },
        )
        .unwrap();
    let rgba = read_rgba8(&renderer, &out);
    assert_rgba8_near(&rgba, &[105; 4], 1);
}

#[test]
fn color_parameters_and_degenerate_ramps_have_defined_results() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let from = rgba_texture(&renderer, [255, 0, 0, 255]);
    let to = rgba_texture(&renderer, [0, 0, 255, 255]);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let tint = ParamValue::Color([1.0, 0.25, 0.0, 0.5]);
    let cases = [
        (
            "fade-color",
            EffectParams::new()
                .with("color", tint.clone())
                .with("color_phase", ParamValue::Float(1.0)),
            [128, 32, 0, 128],
        ),
        (
            "circle",
            EffectParams::new()
                .with("back_color", tint.clone())
                .with("center", ParamValue::Vec2([0.0, 0.0])),
            [128, 32, 0, 128],
        ),
        (
            "circle-crop",
            EffectParams::new().with("back_color", tint),
            [128, 32, 0, 128],
        ),
        (
            "color-phase",
            EffectParams::new()
                .with("from_step", ParamValue::Vec4([0.5; 4]))
                .with("to_step", ParamValue::Vec4([0.5; 4])),
            [0, 0, 255, 255],
        ),
        // Spatial zero-softness ramps integrate pixel coverage instead of choosing one side
        // of a hard edge. They are tested at useful image resolutions by
        // antialiasing::geometric_transition_edges_have_partial_coverage.
        (
            "fade-grayscale",
            EffectParams::new().with("intensity", ParamValue::Float(1.0)),
            [36, 36, 36, 255],
        ),
    ];
    for (name, params, expected) in cases {
        let id = ids
            .iter()
            .find(|id| id.as_str() == format!("alpha2.transition.{name}"))
            .unwrap();
        let out = renderer
            .render_effect(
                id,
                &from_view,
                Some(&to_view),
                RenderSize::new(1, 1).unwrap(),
                &params,
                EffectRuntime {
                    progress: 0.5,
                    ..Default::default()
                },
            )
            .unwrap();
        let actual = read_rgba8(&renderer, &out);
        assert!(
            actual.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 1),
            "{name}: {actual:?} != {expected:?}"
        );
    }
}

#[test]
fn morph_displacement_is_independent_of_uniform_source_opacity() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let id = ids
        .iter()
        .find(|id| id.as_str() == "alpha2.transition.morph")
        .unwrap();
    let size = RenderSize::new(16, 16).unwrap();
    let from: Vec<u8> = (0_u16..256)
        .flat_map(|i| {
            let v = u8::try_from((i % 16) * 16).unwrap();
            [v, v, v, 255]
        })
        .collect();
    let to: Vec<u8> = from
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [255 - p[0], 255 - p[1], 255 - p[2], 255])
        .collect();
    let render = |from: &[u8], to: &[u8]| {
        let a = rgba_image_texture(&renderer, size, from);
        let b = rgba_image_texture(&renderer, size, to);
        let out = renderer
            .render_effect(
                id,
                &a.create_view(&wgpu::TextureViewDescriptor::default()),
                Some(&b.create_view(&wgpu::TextureViewDescriptor::default())),
                size,
                &EffectParams::new().with("strength", ParamValue::Float(0.8)),
                EffectRuntime {
                    progress: 0.3,
                    ..EffectRuntime::default()
                },
            )
            .unwrap();
        read_rgba8(&renderer, &out)
    };
    let half = |bytes: &[u8]| -> Vec<u8> { bytes.iter().map(|v| v.div_ceil(2)).collect() };
    let opaque = render(&from, &to);
    let translucent = render(&half(&from), &half(&to));
    assert_rgba8_near(&translucent, &half(&opaque), 2);
}

#[test]
fn colour_distance_compares_color_instead_of_coverage() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let ids = renderer.register_builtin_transitions().unwrap();
    let id = ids
        .iter()
        .find(|id| id.as_str().ends_with(".colour-distance"))
        .unwrap();
    let from = rgba_texture(&renderer, [26, 38, 51, 64]);
    let to = rgba_texture(&renderer, [77, 115, 154, 192]);
    let out = renderer
        .render_effect(
            id,
            &from.create_view(&wgpu::TextureViewDescriptor::default()),
            Some(&to.create_view(&wgpu::TextureViewDescriptor::default())),
            RenderSize::new(1, 1).unwrap(),
            &EffectParams::new(),
            EffectRuntime {
                progress: 0.2,
                ..EffectRuntime::default()
            },
        )
        .unwrap();
    assert_rgba8_near(&read_rgba8(&renderer, &out), &[77, 115, 154, 192], 1);
}

#[test]
fn yuv_intermediates_match_srgb_uploads_through_composition() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 2).unwrap();
    for matrix in [
        ColorMatrix::Bt601,
        ColorMatrix::Bt709,
        ColorMatrix::Bt2020Ncl,
    ] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            for transfer in [TransferFunction::Bt709, TransferFunction::Srgb] {
                for (y, u, v) in [(126_u8, 128_u8, 128_u8), (126, 102, 157)] {
                    let metadata = ColorMetadata {
                        matrix,
                        range,
                        transfer,
                        ..ColorMetadata::default()
                    };
                    let (linear, _) = crate::convert_yuv_to_linear_rgba(
                        crate::YuvSample {
                            y: f32::from(y) / 255.0,
                            u: f32::from(u) / 255.0,
                            v: f32::from(v) / 255.0,
                            alpha: 1.0,
                        },
                        8,
                        Some(metadata),
                        crate::ColorPolicy::RejectHdr,
                    )
                    .unwrap();
                    let encoded = crate::encode_linear_rgb(
                        [linear[0], linear[1], linear[2]],
                        crate::SdrOutputTransfer::Srgb,
                    );
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let expected = [encoded[0], encoded[1], encoded[2], 1.0]
                        .map(|c| (c * 0.5 * 255.0).round() as u8);
                    let (texture, _) = renderer
                        .render_yuv420p(
                            Yuv420Frame {
                                y: &[y; 4],
                                u: &[u],
                                v: &[v],
                                y_stride: 2,
                                chroma_stride: 1,
                                size,
                                alpha: 0.5,
                                color: Some(metadata),
                            },
                            crate::ColorPolicy::RejectHdr,
                        )
                        .unwrap();
                    let view = texture
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let out = renderer
                        .render_layers(
                            size,
                            &[CompositeLayer {
                                texture: &view,
                                visual: visual([1.0, 1.0], 1.0),
                                blend_mode: BlendMode::Normal,
                            }],
                        )
                        .unwrap();
                    for pixel in read_rgba8(&renderer, &out).as_chunks::<4>().0 {
                        assert_rgba8_near(pixel, &expected, 1);
                    }
                }
            }
        }
    }
}
