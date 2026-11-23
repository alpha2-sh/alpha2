//! Device half-float stores against the CPU model (`cpu/half.rs` for the device's conversion,
//! `cpu/vector.rs` for the premultiplied `over` blend), and what passes store. 8-bit targets are
//! excluded: their tie rounding differs between GPUs by design, and every 8-bit pass rounds to a
//! code first.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::unreadable_literal
)]

use std::sync::Arc;

use super::device_precision::{Probe, TEXELS, VALUES};
use super::{adapter_gpu, gpu_test_lock, read_stored, rgba_image_texture, visual};
use crate::cpu::{Pixels, half};
use crate::{
    BlendMode, BlendSpace, CompositeLayer, CpuImage, CpuRenderer, CpuSource, EffectId,
    EffectParams, EffectRuntime, GraphLayer, ParamValue, RenderGraph, RenderNodeId, RenderSize,
    Renderer, SourceView,
};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }

    /// A float in [2^(low - 127), 2^(high - 126)) with 23 random fraction bits, so truncation and
    /// round-to-nearest differ for about half.
    fn float(&mut self, low: u32, high: u32) -> f32 {
        let exponent = low + self.next() % (high - low + 1);
        f32::from_bits((exponent << 23) | (self.next() & 0x007f_ffff))
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / 16_777_216.0
    }
}

/// Inputs where rounding modes and range handling differ, with the truncated half.
const SPECIALS: [(&str, u32); 33] = [
    ("one", 0x3f80_0000),
    ("half", 0x3f00_0000),
    ("largest finite", 0x477f_e000),
    ("smallest normal", 0x3880_0000),
    ("smallest subnormal", 0x3380_0000),
    ("tie to even", 0x3f80_1000),
    ("tie to odd", 0x3f80_3000),              // 0x3c01; to nearest 0x3c02
    ("past a tie", 0x3f80_1001),              // 0x3c00; to nearest 0x3c01
    ("short of a tie", 0x3f80_0fff),          // 0x3c00
    ("negative past a tie", 0xbf80_1001),     // 0xbc00; to nearest 0xbc01
    ("negative tie to odd", 0xbf80_3000),     // 0xbc01; to nearest 0xbc02
    ("0.7", 0x3f33_3333),                     // 0x3999; to nearest 0x399a
    ("0.3", 0x3e99_999a),                     // 0x34cc; to nearest 0x34cd
    ("0.2", 0x3e4c_cccd),                     // 0x3266
    ("0.35", 0x3eb3_3333),                    // 0x3599; to nearest 0x359a
    ("0.6", 0x3f19_999a),                     // 0x38cc; to nearest 0x38cd
    ("1.5 subnormal steps", 0x33c0_0000),     // 0x0001; to nearest 0x0002
    ("0.75 subnormal steps", 0x3340_0000),    // 0x0000; to nearest 0x0001
    ("17.5 subnormal steps", 0x358c_0000),    // 0x0011; to nearest 0x0012
    ("17.75 subnormal steps", 0x358e_0000),   // 0x0011; to nearest 0x0012
    ("largest subnormal range", 0x387b_a882), // 0x03ee; to nearest 0x03ef
    ("negative 1.5 subnormal steps", 0xb3c0_0000), // 0x8001; to nearest 0x8002
    ("65519", 0x477f_ef00),                   // 0x7bff
    ("65520", 0x477f_f000),                   // 0x7bff; to nearest infinity
    ("1e5", 0x47c3_5000),                     // 0x7bff; to nearest infinity
    ("-1e5", 0xc7c3_5000),                    // 0xfbff; to nearest -infinity
    ("infinity", 0x7f80_0000),                // 0x7c00
    ("-infinity", 0xff80_0000),               // 0xfc00
    ("quiet NaN", 0x7fc0_0000),
    ("signalling NaN", 0x7f80_0001),
    ("f32 subnormal", 0x0001_16c2), // 0x0000
    ("-0", 0x8000_0000),
    ("0", 0x0000_0000),
];

fn is_nan(bits: u16) -> bool {
    bits & 0x7c00 == 0x7c00 && bits & 0x03ff != 0
}

fn halves(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect()
}

#[test]
fn rgba16float_targets_convert_as_the_cpu_truncating_model() {
    let _lock = gpu_test_lock();
    let gpu = adapter_gpu();
    let probe = Probe::new(gpu);
    let mut random = Lcg(0x5c23_1630_0000_0001);
    let mut inputs = SPECIALS.iter().map(|(_, bits)| *bits).collect::<Vec<u32>>();
    while inputs.len() < VALUES {
        let magnitude = random.float(113, 127);
        let negative = random.next().is_multiple_of(8);
        inputs.push(if negative { -magnitude } else { magnitude }.to_bits());
    }
    let stored = halves(&probe.draw("store", wgpu::TextureFormat::Rgba16Float, &inputs, None));
    let mismatches = inputs
        .iter()
        .zip(&stored)
        .enumerate()
        .filter(|(_, (bits, device))| {
            let model = half::store(f32::from_bits(**bits));
            **device != model && !(is_nan(**device) && is_nan(model))
        })
        .map(|(index, (bits, device))| {
            let name = SPECIALS.get(index).map_or("random", |(name, _)| *name);
            format!(
                "{name} 0x{bits:08x}: device 0x{device:04x}, model 0x{:04x}",
                half::store(f32::from_bits(*bits))
            )
        })
        .collect::<Vec<_>>();
    assert!(
        mismatches.is_empty(),
        "{} of {VALUES} stored halves differ from the CPU model, first {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(8)]
    );
}

#[test]
fn premultiplied_over_stays_within_sixteen_half_ulps_of_the_cpu_model() {
    let _lock = gpu_test_lock();
    let gpu = adapter_gpu();
    if gpu.capabilities().intermediate_format != alpha2_gpu::IntermediateFormat::Rgba16Float {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::HalfFloatBlend,
            "the adapter's intermediate is not half float"
        );
        return;
    }
    let probe = Probe::new(gpu);
    let mut random = Lcg(0x5c23_1630_0000_0002);
    // Destination of stored halves and a full-precision source.
    let mut under = Vec::with_capacity(VALUES);
    let mut over = Vec::with_capacity(VALUES);
    let mut cases = Vec::with_capacity(VALUES);
    for _ in 0..TEXELS {
        let under_alpha = half::load(half::store(random.unit()));
        let over_alpha = random.unit().max(1.0 / 1024.0);
        let mut destination = [0.0_f32; 4];
        let mut source = [0.0_f32; 4];
        for channel in 0..3 {
            destination[channel] = half::load(half::store(under_alpha * random.unit()));
            source[channel] = over_alpha * random.unit();
        }
        destination[3] = under_alpha;
        source[3] = over_alpha;
        let mut model = destination;
        crate::cpu::premultiplied_over(&mut model, source);
        for channel in 0..4 {
            under.push(destination[channel].to_bits());
            over.push(source[channel].to_bits());
            cases.push(model[channel]);
        }
    }
    let blended = halves(&probe.draw(
        "store",
        wgpu::TextureFormat::Rgba16Float,
        &over,
        Some((&under, wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)),
    ));
    let mut matches = 0;
    let mut worst_ulps = 0_u16;
    let mut worst_codes = 0.0_f32;
    for (model, device) in cases.iter().zip(&blended) {
        let model_bits = half::store(*model);
        matches += usize::from(model_bits == *device);
        // Every value is in 0..=1, where a half's bits order as its value.
        worst_ulps = worst_ulps.max(model_bits.abs_diff(*device));
        worst_codes = worst_codes.max((half::load(*device) - model).abs() * 255.0);
    }
    eprintln!(
        "premultiplied over: {matches} of {VALUES} equal the CPU model, worst {worst_ulps} half \
         ULPs, {worst_codes:.4} codes"
    );
    // Measured: exact on software rasterizers and some GPUs, up to 16 half ULPs (worst 2 at the top
    // of the range) on others, under a quarter of a code.
    assert!(
        worst_ulps <= 16 && worst_codes <= 0.25,
        "the device's premultiplied over is {worst_ulps} half ULPs ({worst_codes} codes) from \
         the CPU model; at most 16 and 0.25 allowed"
    );
}

/// An opaque picture whose channels walk through many half-float exponents.
fn gradient(size: RenderSize) -> Vec<u8> {
    let mut state = 0x2f6b_u32;
    (0..size.height)
        .flat_map(|y| (0..size.width).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            [
                x as u8,
                ((x * 3 + y * 5) % 256) as u8,
                (state >> 24) as u8,
                255,
            ]
        })
        .collect()
}

fn stacked_size() -> RenderSize {
    RenderSize::new(256, 64).expect("size")
}

fn gpu_stack(renderer: &Renderer, transparent_layers: usize, scale: f32) -> Vec<u8> {
    let size = stacked_size();
    let picture = rgba_image_texture(renderer, size, &gradient(size));
    let picture = picture.create_view(&wgpu::TextureViewDescriptor::default());
    let clear = rgba_image_texture(
        renderer,
        size,
        &vec![0; size.width as usize * size.height as usize * 4],
    );
    let clear = clear.create_view(&wgpu::TextureViewDescriptor::default());
    let mut layers = vec![CompositeLayer {
        texture: &picture,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    }];
    layers.extend((0..transparent_layers).map(|_| CompositeLayer {
        texture: &clear,
        visual: visual([scale, scale], 1.0),
        blend_mode: BlendMode::Normal,
    }));
    let output = renderer.render_layers(size, &layers).expect("draws");
    read_stored(renderer, &output)
}

fn cpu_stack(cpu: &CpuRenderer, transparent_layers: usize, space: BlendSpace) -> Vec<[u16; 4]> {
    let size = stacked_size();
    let picture = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(size, &gradient(size)).expect("picture"),
    ));
    let clear = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(
            size,
            &vec![0; size.width as usize * size.height as usize * 4],
        )
        .expect("picture"),
    ));
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(space);
    let mut layers = vec![GraphLayer {
        source: graph.add_source(SourceView::cpu(&picture)),
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    }];
    for _ in 0..transparent_layers {
        layers.push(GraphLayer {
            source: graph.add_source(SourceView::cpu(&clear)),
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        });
    }
    let output = graph.add_composite(layers).expect("composite");
    graph.set_output(output).expect("output");
    match cpu.execute_graph(&graph).expect("CPU draws").pixels() {
        Pixels::Half(pixels) => pixels.clone(),
        Pixels::Unorm8(_) => panic!("a composite stores halves"),
    }
}

#[test]
fn transparent_layers_leave_the_stored_backdrop_unchanged() {
    let _lock = gpu_test_lock();
    for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
        let mut renderer = Renderer::new(adapter_gpu());
        if renderer.intermediate_format() != wgpu::TextureFormat::Rgba16Float {
            eprintln!("skipped: the adapter's intermediate is not half float");
            return;
        }
        renderer.blend_space = space;
        // Canvas-size layers draw one pass each; smaller ones batch through the loop program.
        for scale in [1.0, 0.6] {
            let base = gpu_stack(&renderer, 0, scale);
            for count in [1, 4, 8, 16, 32] {
                let stacked = gpu_stack(&renderer, count, scale);
                let changed = base
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .zip(stacked.as_chunks::<8>().0)
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(
                    changed, 0,
                    "{space:?} scale {scale}: {count} transparent layers changed {changed} stored pixels"
                );
            }
        }
    }
}

#[test]
fn transparent_layers_leave_the_cpu_backdrop_unchanged() {
    let cpu = CpuRenderer::new(1);
    for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
        let base = cpu_stack(&cpu, 0, space);
        for count in [1, 4, 8, 16, 32] {
            assert_eq!(
                base,
                cpu_stack(&cpu, count, space),
                "{space:?} {count} layers"
            );
        }
    }
}

fn canvas_sizes() -> [RenderSize; 3] {
    [(5, 5), (160, 90), (1920, 1080)]
        .map(|(width, height)| RenderSize::new(width, height).expect("size"))
}

fn flat_picture(size: RenderSize, code: u8) -> Vec<u8> {
    [code, code, code, 255].repeat(size.width as usize * size.height as usize)
}

fn stored_picture(rgba8: &[u8]) -> Vec<u16> {
    rgba8
        .iter()
        .map(|code| half::store_nearest(half::unorm8_value(*code)))
        .collect()
}

fn changed_halves(stored: &[u16], picture: &[u8]) -> usize {
    stored
        .iter()
        .zip(stored_picture(picture))
        .filter(|(stored, expected)| **stored != *expected)
        .count()
}

fn largest_step(stored: &[u16], picture: &[u8]) -> u16 {
    stored
        .iter()
        .zip(stored_picture(picture))
        .map(|(stored, expected)| stored.abs_diff(expected))
        .max()
        .unwrap_or(0)
}

fn gpu_graph_halves<'a>(
    renderer: &Renderer,
    size: RenderSize,
    view: &'a wgpu::TextureView,
    build: impl Fn(&mut RenderGraph<'a>, RenderNodeId) -> RenderNodeId,
) -> Vec<u16> {
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(BlendSpace::SrgbEncoded);
    let source = graph.add_source(SourceView::from_wgpu(view));
    let output = build(&mut graph, source);
    graph.set_output(output).expect("output");
    let drawn = renderer.execute_graph(&graph).expect("GPU draws");
    halves(&read_stored(renderer, &drawn))
}

fn cpu_graph_halves<'a>(
    cpu: &CpuRenderer,
    size: RenderSize,
    source: &'a CpuSource,
    build: impl Fn(&mut RenderGraph<'a>, RenderNodeId) -> RenderNodeId,
) -> Vec<u16> {
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(BlendSpace::SrgbEncoded);
    let source = graph.add_source(SourceView::cpu(source));
    let output = build(&mut graph, source);
    graph.set_output(output).expect("output");
    match cpu.execute_graph(&graph).expect("CPU draws").pixels() {
        Pixels::Half(pixels) => pixels.as_flattened().to_vec(),
        Pixels::Unorm8(_) => panic!("a pass stores halves"),
    }
}

fn canvas_size_layer(graph: &mut RenderGraph<'_>, source: RenderNodeId) -> RenderNodeId {
    graph
        .add_composite(vec![GraphLayer {
            source,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        }])
        .expect("composite")
}

fn unmoved_offset(graph: &mut RenderGraph<'_>, source: RenderNodeId) -> RenderNodeId {
    graph
        .add_effect(
            EffectId::new("alpha2.effect.offset".into(), 1),
            source,
            EffectParams::new().with("edge", ParamValue::Enum(0)),
            EffectRuntime::default(),
        )
        .expect("effect")
}

const FLAT_CODES: [u8; 6] = [16, 64, 118, 128, 200, 235];

#[test]
fn an_opaque_canvas_size_layer_stores_its_picture_unchanged_at_any_canvas_size() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(adapter_gpu());
    if renderer.intermediate_format() != wgpu::TextureFormat::Rgba16Float {
        eprintln!("skipped: the adapter's intermediate is not half float");
        return;
    }
    for size in canvas_sizes() {
        for code in FLAT_CODES {
            let picture = flat_picture(size, code);
            let texture = rgba_image_texture(&renderer, size, &picture);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let stored = gpu_graph_halves(&renderer, size, &view, canvas_size_layer);
            let changed = changed_halves(&stored, &picture);
            assert_eq!(
                changed, 0,
                "{}x{} code {code}: {changed} stored halves differ from the picture's",
                size.width, size.height
            );
        }
    }
}

#[test]
fn an_opaque_canvas_size_layer_stores_its_cpu_picture_unchanged_at_any_canvas_size() {
    let cpu = CpuRenderer::new(1);
    for size in canvas_sizes() {
        for code in FLAT_CODES {
            let picture = flat_picture(size, code);
            let source = CpuSource::image(Arc::new(
                CpuImage::from_premultiplied_rgba8(size, &picture).expect("picture"),
            ));
            let stored = cpu_graph_halves(&cpu, size, &source, canvas_size_layer);
            let changed = changed_halves(&stored, &picture);
            assert_eq!(
                changed, 0,
                "{}x{} code {code}: {changed} stored halves differ from the picture's",
                size.width, size.height
            );
        }
    }
}

#[test]
fn an_offset_that_moves_nothing_stores_its_source_unchanged_at_any_canvas_size() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(adapter_gpu());
    if renderer.intermediate_format() != wgpu::TextureFormat::Rgba16Float {
        eprintln!("skipped: the adapter's intermediate is not half float");
        return;
    }
    renderer.register_builtins().expect("built-ins register");
    let cpu = CpuRenderer::new(1);
    for size in canvas_sizes() {
        for code in FLAT_CODES {
            let picture = flat_picture(size, code);
            let texture = rgba_image_texture(&renderer, size, &picture);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let device_changed = changed_halves(
                &gpu_graph_halves(&renderer, size, &view, unmoved_offset),
                &picture,
            );
            let source = CpuSource::image(Arc::new(
                CpuImage::from_premultiplied_rgba8(size, &picture).expect("picture"),
            ));
            let cpu_changed = changed_halves(
                &cpu_graph_halves(&cpu, size, &source, unmoved_offset),
                &picture,
            );
            assert_eq!(
                (device_changed, cpu_changed),
                (0, 0),
                "{}x{} code {code}: stored halves differing from the source's on the GPU and on the CPU",
                size.width,
                size.height
            );
        }
    }
}

#[test]
fn a_gaussian_blur_moves_a_flat_picture_at_most_one_step_and_not_again() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(adapter_gpu());
    if renderer.intermediate_format() != wgpu::TextureFormat::Rgba16Float {
        eprintln!("skipped: the adapter's intermediate is not half float");
        return;
    }
    renderer.register_builtins().expect("built-ins register");
    let cpu = CpuRenderer::new(1);
    let size = RenderSize::new(64, 64).expect("size");
    for sigma_pixels in [4.32_f32, 15.66, 16.74] {
        let blurs = |count: usize| {
            move |graph: &mut RenderGraph<'_>, source: RenderNodeId| {
                (0..count).fold(source, |node, _| {
                    graph
                        .add_gaussian_pixels(node, sigma_pixels, EffectRuntime::default())
                        .expect("blur")
                })
            }
        };
        for code in FLAT_CODES {
            let picture = flat_picture(size, code);
            let texture = rgba_image_texture(&renderer, size, &picture);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let source = CpuSource::image(Arc::new(
                CpuImage::from_premultiplied_rgba8(size, &picture).expect("picture"),
            ));
            for (renderer_name, once, twice) in [
                (
                    "GPU",
                    gpu_graph_halves(&renderer, size, &view, blurs(1)),
                    gpu_graph_halves(&renderer, size, &view, blurs(2)),
                ),
                (
                    "CPU",
                    cpu_graph_halves(&cpu, size, &source, blurs(1)),
                    cpu_graph_halves(&cpu, size, &source, blurs(2)),
                ),
            ] {
                let step = largest_step(&once, &picture);
                assert!(
                    step <= 1,
                    "{renderer_name} deviation {sigma_pixels} code {code}: one blur moved a flat level {step} half steps"
                );
                assert!(
                    once == twice,
                    "{renderer_name} deviation {sigma_pixels} code {code}: a second blur moved the level again"
                );
            }
        }
    }
}

fn every_level(size: RenderSize, as_alpha: bool) -> Vec<[u16; 4]> {
    let one = half::store(1.0);
    (0..size.width * size.height)
        .map(|index| u16::try_from(index).map_or(one, |bits| bits.min(one)))
        .map(|level| {
            if as_alpha {
                [0, 0, 0, level]
            } else {
                [level, level, level, one]
            }
        })
        .collect()
}

#[test]
fn effects_at_a_unit_exponent_store_every_level_unchanged() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(adapter_gpu());
    if renderer.intermediate_format() != wgpu::TextureFormat::Rgba16Float {
        eprintln!("skipped: the adapter's intermediate is not half float");
        return;
    }
    renderer.register_builtins().expect("built-ins register");
    let cpu = CpuRenderer::new(1);
    let size = RenderSize::new(128, 121).expect("size");
    for (id, as_alpha) in [
        ("alpha2.effect.levels", false),
        ("alpha2.effect.lift-gamma-gain", false),
        ("alpha2.effect.alpha-adjust", true),
    ] {
        let levels = every_level(size, as_alpha);
        let effect = |graph: &mut RenderGraph<'_>, source: RenderNodeId| {
            graph
                .add_effect(
                    EffectId::new(id.into(), 1),
                    source,
                    EffectParams::new(),
                    EffectRuntime::default(),
                )
                .expect("effect")
        };
        let upload = renderer
            .upload_linear_rgba16f(size, levels.as_flattened())
            .expect("upload");
        let device = gpu_graph_halves(&renderer, size, upload.view(), effect);
        let source = CpuSource::image(Arc::new(CpuImage::new(
            size,
            crate::cpu::PixelBox::whole(size),
            Pixels::Half(levels.clone()),
        )));
        let host = cpu_graph_halves(&cpu, size, &source, effect);
        for (renderer_name, stored) in [("GPU", device), ("CPU", host)] {
            let moved = stored
                .iter()
                .zip(levels.as_flattened())
                .filter(|(stored, level)| stored != level)
                .count();
            assert_eq!(
                moved, 0,
                "{renderer_name} {id}: {moved} stored halves moved"
            );
        }
    }
}
