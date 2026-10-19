//! Layers composited several to a pass store what a pass per layer stores,
//! byte for byte, in every blend mode and space, over a background or none,
//! in both intermediate formats, on every device: a small layer draws with
//! the loop pipeline whether alone or in a batch, and a large layer, which
//! never shares a pass, with the one-layer pipeline either way.

use super::{gpu_test_lock, gpus_by_intermediate, rgba_image_texture};
use crate::composite::{
    COMPOSITE_BATCH_LAYERS, CompositeProgram, LayerTier, composite_batches, layer_bounds,
};
use crate::{
    BlendMode, BlendSpace, Color, CompositeLayer, GraphLayer, LayerTransform, RenderGraph,
    RenderSize, RenderedTexture, Renderer, SourceView,
};
use std::sync::atomic::Ordering;

/// Reads a texture back as the intermediate format stores it.
fn read_stored(renderer: &Renderer, target: &RenderedTexture) -> Vec<u8> {
    let texel = match target.format {
        wgpu::TextureFormat::Rgba16Float => 8,
        wgpu::TextureFormat::Rgba8Unorm => 4,
        other => panic!("unexpected intermediate {other:?}"),
    };
    let device = renderer.gpu.device();
    let row = target.size.width * texel;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("alpha2 test stored readback"),
        size: u64::from(padded * target.size.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(target.size.height),
            },
        },
        wgpu::Extent3d {
            width: target.size.width,
            height: target.size.height,
            depth_or_array_layers: 1,
        },
    );
    renderer.gpu.submit([encoder.finish()]);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, |result| result.expect("map"));
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("poll");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    mapped
        .chunks(padded as usize)
        .flat_map(|line| line[..row as usize].iter().copied())
        .collect()
}

/// The largest per-channel difference between two stored texels, in 8-bit
/// codes (a half float's value times 255), for a failure's message.
fn channel_step(format: wgpu::TextureFormat, left: &[u8], right: &[u8]) -> f32 {
    match format {
        wgpu::TextureFormat::Rgba16Float => left
            .chunks(2)
            .zip(right.chunks(2))
            .map(|(a, b)| {
                let value = |bytes: &[u8]| half::f16::from_le_bytes([bytes[0], bytes[1]]).to_f32();
                (value(a) - value(b)).abs() * 255.0
            })
            .fold(0.0, f32::max),
        _ => left
            .iter()
            .zip(right)
            .map(|(a, b)| f32::from(a.abs_diff(*b)))
            .fold(0.0, f32::max),
    }
}

/// A premultiplied source with soft alpha and colour ramps, so edges,
/// blending and every rounding step show.
fn source(renderer: &Renderer, side: u32, seed: u32) -> wgpu::Texture {
    let pixels = (0..side * side)
        .flat_map(|index| {
            let (x, y) = (index % side, index / side);
            let ramp = |value: u32| u8::try_from((value * 255 / side).min(255)).expect("small");
            let alpha = ramp((x + seed * 7) % side).max(ramp(y) / 3);
            let color = [
                ramp((x + y + seed * 13) % side),
                ramp((y * 3 + seed) % side),
                ramp((side - 1 - x + seed * 5) % side),
            ];
            let premultiplied = color
                .map(|value| u8::try_from(u16::from(value) * u16::from(alpha) / 255).expect("u8"));
            [premultiplied[0], premultiplied[1], premultiplied[2], alpha]
        })
        .collect::<Vec<_>>();
    rgba_image_texture(
        renderer,
        RenderSize::new(side, side).expect("size"),
        &pixels,
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn batched_layers_store_what_a_pass_per_layer_stores() {
    let _lock = gpu_test_lock();
    for gpu in gpus_by_intermediate() {
        let mut renderer = Renderer::new(gpu);
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        // Magnified, about 1:1, and minified (several filter samples) sources.
        let textures = [
            source(&renderer, 24, 1),
            source(&renderer, 64, 2),
            source(&renderer, 256, 3),
        ];
        let views = textures
            .iter()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect::<Vec<_>>();
        #[allow(clippy::cast_precision_loss)]
        let layer = |index: usize, small: bool| {
            let step = index as f32;
            let spread = |value: f32| (value * 0.618_034).fract();
            CompositeLayer {
                texture: &views[index % views.len()],
                visual: LayerTransform {
                    position: [
                        0.1 + 0.8 * spread(step),
                        0.1 + 0.8 * spread(step * 1.7 + 0.3),
                    ],
                    anchor: [0.5, 0.4],
                    scale: if small {
                        [
                            0.15 + 0.2 * spread(step * 2.3),
                            0.2 + 0.15 * spread(step * 3.1),
                        ]
                    } else {
                        [1.1, 0.9 + 0.3 * spread(step)]
                    },
                    rotation_degrees: 37.0 * step - 60.0,
                    opacity: 0.35 + 0.65 * spread(step * 4.7),
                },
                blend_mode: BlendMode::ALL[index % BlendMode::ALL.len()],
            }
        };
        let scenes: Vec<Vec<CompositeLayer<'_>>> = vec![
            // Many small overlapping layers in every mode: three batches.
            (0..19).map(|index| layer(index, true)).collect(),
            // Full-frame layers between small ones, so batches alternate bounded and not.
            (0..12).map(|index| layer(index, index % 4 == 1)).collect(),
            // Exactly one batch, and one layer more.
            (0..8).map(|index| layer(index + 3, true)).collect(),
            (0..9).map(|index| layer(index + 5, index != 4)).collect(),
            vec![layer(2, false), layer(7, true)],
            vec![layer(11, true)],
        ];
        // An odd size puts the target's last quads half outside it.
        let sizes = [
            RenderSize::new(96, 64).expect("size"),
            RenderSize::new(77, 51).expect("size"),
        ];
        let format = renderer.intermediate_format();
        for size in sizes {
            for blend_space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
                renderer.blend_space = blend_space;
                for (index, scene) in scenes.iter().enumerate() {
                    for background in [None, Some(Color::rgba(0.2, 0.4, 0.6, 1.0))] {
                        let render = |limit: usize| {
                            renderer
                                .composite_batch_limit
                                .store(limit, Ordering::Relaxed);
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
                            let stored = read_stored(&renderer, &output);
                            (output, stored)
                        };
                        let (reference_texture, reference) = render(1);
                        renderer.clear_texture_pool();
                        // Batched as a GPU batches, on a software rasterizer too.
                        let (batched_texture, batched) = render(COMPOSITE_BATCH_LAYERS);
                        assert_ne!(reference_texture.texture, batched_texture.texture);
                        drop((reference_texture, batched_texture));
                        let texel = reference.len() / (size.width * size.height) as usize;
                        let differences = reference
                            .chunks(texel)
                            .zip(batched.chunks(texel))
                            .enumerate()
                            .filter(|(_, (left, right))| left != right)
                            .map(|(pixel, (left, right))| {
                                (
                                    pixel % size.width as usize,
                                    pixel / size.width as usize,
                                    left.to_vec(),
                                    right.to_vec(),
                                )
                            })
                            .collect::<Vec<_>>();
                        assert!(
                            differences.is_empty(),
                            "{format:?} {size:?} {blend_space:?}, scene {index}, background {background:?}: {} pixels differ, by up to {} codes, first {:?}",
                            differences.len(),
                            differences
                                .iter()
                                .map(|(_, _, left, right)| channel_step(format, left, right))
                                .fold(0.0, f32::max),
                            &differences[..differences.len().min(4)]
                        );
                    }
                }
            }
        }
        renderer.composite_batch_limit.store(0, Ordering::Relaxed);
        assert!(pollster::block_on(scope.pop()).is_none());
    }
}

#[test]
fn sixteen_small_layers_composite_in_two_passes_on_a_gpu() {
    // A pass per layer costs a backdrop copy and a barrier each.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(super::gpu());
    let software = renderer.gpu.capabilities().software_rasterizer;
    let source = source(&renderer, 32, 4);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    #[allow(clippy::cast_precision_loss)]
    let layers = (0..16)
        .map(|index| {
            let step = index as f32 / 16.0;
            CompositeLayer {
                texture: &view,
                visual: LayerTransform {
                    position: [0.2 + 0.6 * step, 0.25 + 0.5 * ((step * 7.0) % 1.0)],
                    anchor: [0.5, 0.5],
                    scale: [0.1_f32.sqrt(), 0.1_f32.sqrt()],
                    rotation_degrees: 0.0,
                    opacity: 0.9,
                },
                blend_mode: BlendMode::Normal,
            }
        })
        .collect::<Vec<_>>();
    let size = RenderSize::new(192, 108).expect("size");
    let passes = |limit: usize| {
        renderer
            .composite_batch_limit
            .store(limit, Ordering::Relaxed);
        let before = renderer.diagnostics().graph_passes;
        drop(renderer.render_layers(size, &layers).expect("composite"));
        renderer.diagnostics().graph_passes - before
    };
    // A software rasterizer pays for every step of a pass on every pixel of it, so it
    // keeps a pass per layer; a GPU batches eight.
    assert_eq!(
        renderer.composite_batch_layers(),
        if software { 1 } else { COMPOSITE_BATCH_LAYERS }
    );
    assert_eq!(passes(0), if software { 16 } else { 2 });
    assert_eq!(passes(COMPOSITE_BATCH_LAYERS), 2);
    assert_eq!(passes(1), 16);
    renderer.composite_batch_limit.store(0, Ordering::Relaxed);
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_small_layer_draws_with_the_batch_pipeline_alone_and_a_large_one_with_one_step() {
    // Compilers optimize a one-layer pass and a batch apart (contraction), and Metal, D3D12 and
    // GL store a batch's last bit differently, so a large layer never batches.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(super::gpu());
    let software = renderer.gpu.capabilities().software_rasterizer;
    let source = source(&renderer, 32, 5);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    #[allow(clippy::cast_precision_loss)]
    let layer = |index: usize, small: bool| CompositeLayer {
        texture: &view,
        visual: LayerTransform {
            position: [0.2 + 0.06 * index as f32, 0.3 + 0.04 * index as f32],
            anchor: [0.5, 0.5],
            scale: if small { [0.25, 0.25] } else { [1.2, 1.2] },
            rotation_degrees: 0.0,
            opacity: 0.8,
        },
        blend_mode: BlendMode::Normal,
    };
    // A first pass, then bounded and full-frame passes, batched or one layer each.
    let layers = (0..12)
        .map(|index| layer(index, index % 5 != 2))
        .collect::<Vec<_>>();
    let size = RenderSize::new(96, 64).expect("size");
    let draw = |limit: usize| {
        renderer
            .composite_batch_limit
            .store(limit, Ordering::Relaxed);
        let before = renderer.composite_pipelines.lock().expect("log").passes;
        drop(renderer.render_layers(size, &layers).expect("composite"));
        renderer.composite_pipelines.lock().expect("log").passes - before
    };
    // As a GPU composites, on any device: small layers a pass each, two and eight to a
    // pass, then a pass each again.
    let passes = [1, 2, COMPOSITE_BATCH_LAYERS, 1].map(draw);
    assert_eq!(passes, [12, 7, 5, 12]);
    let cached = renderer.composite_pipeline();
    let looped = renderer.composite_program_pipeline(cached, CompositeProgram::Loop);
    let one_layer = renderer.composite_program_pipeline(cached, CompositeProgram::OneLayer);
    assert_ne!(looped, one_layer);
    {
        let log = renderer.composite_pipelines.lock().expect("log");
        let mut created = log.created.clone();
        created.sort_by_key(|program| *program == CompositeProgram::OneLayer);
        assert_eq!(
            created,
            [CompositeProgram::Loop, CompositeProgram::OneLayer]
        );
        let mut drawn = log
            .drawn
            .iter()
            .map(|(program, pipeline, tier, several)| {
                (*program, pipeline == looped, *tier, *several)
            })
            .collect::<Vec<_>>();
        drawn.sort_by_key(|entry| format!("{entry:?}"));
        // The very pipeline object: a small layer alone and a batch of them; a large
        // layer always alone, with the other.
        assert_eq!(
            drawn,
            [
                (CompositeProgram::Loop, true, LayerTier::Small, false),
                (CompositeProgram::Loop, true, LayerTier::Small, true),
                (CompositeProgram::OneLayer, false, LayerTier::Large, false),
            ]
        );
    }
    // As this device composites: a software rasterizer never batches, and draws every
    // layer with the one-step pipeline.
    renderer
        .composite_pipelines
        .lock()
        .expect("log")
        .drawn
        .clear();
    assert_eq!(draw(0), if software { 12 } else { 5 });
    let log = renderer.composite_pipelines.lock().expect("log");
    assert_eq!(log.created.len(), 2);
    for (program, pipeline, tier, several) in &log.drawn {
        if software {
            assert_eq!(
                (*program, pipeline, *several),
                (CompositeProgram::OneLayer, one_layer, false)
            );
        } else {
            let expected = if *tier == LayerTier::Small {
                looped
            } else {
                one_layer
            };
            assert_eq!(pipeline, expected);
        }
    }
    drop(log);
    renderer.composite_batch_limit.store(0, Ordering::Relaxed);
}

#[test]
fn a_layer_shares_a_pass_exactly_when_its_tier_is_small() {
    // Batching and pipeline choice are one decision, its tier, so a batchable layer never draws
    // with the one-step pipeline a batch would not match bit for bit.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(super::gpu());
    renderer
        .composite_batch_limit
        .store(COMPOSITE_BATCH_LAYERS, Ordering::Relaxed);
    let size = RenderSize::new(96, 64).expect("size");
    let half = u64::from(size.width) * u64::from(size.height) / 2;
    #[allow(clippy::cast_precision_loss)]
    let visual = |scale: f32| LayerTransform {
        position: [0.5, 0.5],
        anchor: [0.5, 0.5],
        scale: [scale, scale],
        rotation_degrees: 0.0,
        opacity: 1.0,
    };
    let mut tiers = [0, 0];
    #[allow(clippy::cast_precision_loss)]
    for step in 0..200 {
        let scale = 0.55 + 0.002 * step as f32;
        let bounds = layer_bounds(visual(scale), size).expect("on the target");
        let tier = LayerTier::of(bounds, size);
        assert_eq!(tier == LayerTier::Small, bounds.area() < half, "{bounds:?}");
        tiers[usize::from(tier == LayerTier::Large)] += 1;
        let drawn = renderer.drawn_layers([visual(scale); 3].into_iter(), size);
        assert!(
            drawn
                .iter()
                .all(|(_, other_bounds, other)| (*other_bounds, *other) == (bounds, tier))
        );
        let batches = composite_batches(&drawn, COMPOSITE_BATCH_LAYERS);
        let expected_passes = if tier == LayerTier::Small { 1 } else { 3 };
        assert_eq!(batches.len(), expected_passes, "{bounds:?} {tier:?}");
        let expected_program = if tier == LayerTier::Small {
            CompositeProgram::Loop
        } else {
            CompositeProgram::OneLayer
        };
        assert_eq!(renderer.composite_program(tier), expected_program);
    }
    assert!(tiers[0] > 0 && tiers[1] > 0, "{tiers:?}");
    renderer.composite_batch_limit.store(0, Ordering::Relaxed);
}
