//! Standalone benchmark for export readback packing and GPU pool reuse.
//!
//! Needs a GPU adapter.
//! - `readback`: CPU time to pack one mapped I420 frame, through
//!   `finish` (allocating), `finish_into` (caller-owned planes), and
//!   `finish_mapped` copying the staging bytes whole, as browser export does.
//! - `texture_churn`: textures created, reused and evicted over 100 frames
//!   under a budget that holds only another size.
//! - `uniform_churn` and `uniform_churn_presented`: uniform buffers created,
//!   reused and discarded over 100 composites, the latter with presented YUV
//!   sources and a varying layer count.
//!
//! Times are recorded, never gated. Pool counters are deterministic.

use std::{hint::black_box, time::Instant};

use alpha2_gpu::GpuContext;
use alpha2_media::{ColorMetadata, VideoCodec, YuvEncoding};
use alpha2_render::{
    BlendMode, Color, ColorPolicy, CompositeLayer, LayerTransform, PlaneTarget, RenderSize,
    Renderer, SourcePresentation, Yuv420Frame, Yuv420pTargets,
};

const FRAMES: u32 = 30;

fn main() {
    let gpu = pollster::block_on(GpuContext::request_product()).expect("GPU adapter");
    // The adapter first: a time means nothing without the GPU it was measured on.
    let adapter = gpu.capabilities();
    println!(
        "adapter: {} on {} ({} {})",
        adapter.adapter_name, adapter.backend, adapter.driver, adapter.driver_info
    );
    let renderer = Renderer::new(gpu);
    for (width, height) in [(1_920, 1_080), (3_840, 2_160)] {
        readback(&renderer, RenderSize::new(width, height).expect("size"));
    }
    texture_churn(&renderer);
    uniform_churn(&renderer);
    uniform_churn_presented(&renderer);
}

fn color() -> Color {
    Color {
        red: 0.2,
        green: 0.5,
        blue: 0.8,
        alpha: 1.0,
    }
}

fn readback(renderer: &Renderer, size: RenderSize) {
    let (width, height) = (size.width as usize, size.height as usize);
    let (mut y, mut u, mut v) = (
        vec![0_u8; width * height],
        vec![0_u8; width * height / 4],
        vec![0_u8; width * height / 4],
    );
    let mut totals = [0.0; 3];
    for frame in 0..FRAMES * 3 {
        let target = renderer.render_solid(size, color()).expect("render");
        let readback = renderer
            .begin_yuv420p_readback(&target, YuvEncoding::export(VideoCodec::H264))
            .expect("readback");
        readback.wait_ready().expect("mapped");
        let started = Instant::now();
        let path = frame % 3;
        match path {
            0 => {
                black_box(readback.finish().expect("finish"));
            }
            1 => readback
                .finish_into(&mut Yuv420pTargets {
                    y: PlaneTarget::new(&mut y, width),
                    u: PlaneTarget::new(&mut u, width / 2),
                    v: PlaneTarget::new(&mut v, width / 2),
                })
                .expect("finish"),
            _ => {
                black_box(
                    readback
                        .finish_mapped(|mapped| mapped.to_vec())
                        .expect("finish"),
                );
            }
        }
        totals[path as usize] += started.elapsed().as_secs_f64();
    }
    let [allocating, into, mapped] = totals.map(|total| total / f64::from(FRAMES) * 1e3);
    println!(
        "readback size={}x{} finish_ms={allocating:.3} finish_into_ms={into:.3} finish_mapped_ms={mapped:.3}",
        size.width, size.height,
    );
}

fn texture_churn(renderer: &Renderer) {
    // Fill the idle pool with one size, then cap the budget at what it holds.
    let held = (0..4)
        .map(|_| {
            renderer
                .render_solid(RenderSize::new(64, 64).expect("size"), color())
                .expect("render")
        })
        .collect::<Vec<_>>();
    drop(held);
    let idle = renderer.texture_pool_stats().available_bytes;
    renderer.set_texture_budget_bytes(idle);
    let before = renderer.texture_pool_stats();
    for _ in 0..100 {
        black_box(
            renderer
                .render_solid(RenderSize::new(32, 32).expect("size"), color())
                .expect("render"),
        );
    }
    let after = renderer.texture_pool_stats();
    println!(
        "texture_churn frames=100 created={} reused={} evicted={}",
        after.created - before.created,
        after.reused - before.reused,
        after.evicted - before.evicted,
    );
    renderer.set_texture_budget_bytes(u64::MAX);
}

fn uniform_churn(renderer: &Renderer) {
    let size = RenderSize::new(256, 256).expect("size");
    let sources = (0..4)
        .map(|_| renderer.render_solid(size, color()).expect("source"))
        .collect::<Vec<_>>();
    let views = sources
        .iter()
        .map(|source| {
            source
                .texture()
                .create_view(&wgpu::TextureViewDescriptor::default())
        })
        .collect::<Vec<_>>();
    let layers = views
        .iter()
        .map(|view| CompositeLayer {
            texture: view,
            visual: LayerTransform {
                position: [0.5, 0.5],
                anchor: [0.5, 0.5],
                scale: [0.5, 0.5],
                rotation_degrees: 0.0,
                opacity: 0.8,
            },
            blend_mode: BlendMode::Normal,
        })
        .collect::<Vec<_>>();
    let frame = || black_box(renderer.render_layers(size, &layers).expect("composite"));
    for _ in 0..10 {
        frame();
    }
    let before = renderer.diagnostics().uniform_buffers;
    for _ in 0..100 {
        frame();
    }
    let after = renderer.diagnostics().uniform_buffers;
    println!(
        "uniform_churn layers=4 frames=100 created={} reused={} discarded={}",
        after.created - before.created,
        after.reused - before.reused,
        after.discarded - before.discarded,
    );
}

fn uniform_churn_presented(renderer: &Renderer) {
    let size = RenderSize::new(64, 36).expect("size");
    let y = vec![16_u8; 64 * 36];
    let chroma = vec![128_u8; 32 * 18];
    let frame = Yuv420Frame {
        y: &y,
        u: &chroma,
        v: &chroma,
        y_stride: 64,
        chroma_stride: 32,
        size,
        alpha: 1.0,
        color: Some(ColorMetadata::default()),
    };
    let render = |layers: usize| {
        let sources = (0..5)
            .map(|_| {
                renderer
                    .prepare_yuv420p_presented(
                        frame,
                        SourcePresentation::identity(size),
                        ColorPolicy::RejectHdr,
                    )
                    .expect("source")
                    .0
                    .into_parts()
            })
            .collect::<Vec<_>>();
        for (_, pending) in &sources {
            renderer.submit_pending_source(pending.as_ref().expect("pending"));
        }
        let views = sources
            .iter()
            .map(|(texture, _)| {
                texture
                    .texture()
                    .create_view(&wgpu::TextureViewDescriptor::default())
            })
            .collect::<Vec<_>>();
        let composite = views
            .iter()
            .take(layers)
            .map(|view| CompositeLayer {
                texture: view,
                visual: LayerTransform {
                    position: [0.5, 0.5],
                    anchor: [0.5, 0.5],
                    scale: [0.5, 0.5],
                    rotation_degrees: 0.0,
                    opacity: 0.8,
                },
                blend_mode: BlendMode::Normal,
            })
            .collect::<Vec<_>>();
        black_box(renderer.render_layers(size, &composite).expect("composite"));
    };
    for layers in 1..=5 {
        render(layers);
    }
    let before = renderer.diagnostics().uniform_buffers;
    for frame in 0..100 {
        render(frame % 5 + 1);
    }
    let after = renderer.diagnostics().uniform_buffers;
    println!(
        "uniform_churn_presented sources=5 frames=100 created={} reused={} discarded={}",
        after.created - before.created,
        after.reused - before.reused,
        after.discarded - before.discarded,
    );
}
