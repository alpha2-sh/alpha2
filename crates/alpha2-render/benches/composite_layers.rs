//! Composites N layers of one texture, each covering a tenth of the frame at
//! scattered positions, and N full-frame layers, at 1080p and 4K. Every frame
//! waits for the GPU, so the time per frame is dominated by the composite
//! passes. Prints one `composite_layers` line per case. Times are recorded,
//! never gated.

use std::time::Instant;

use alpha2_gpu::GpuContext;
use alpha2_render::{BlendMode, CompositeLayer, LayerTransform, RenderSize, Renderer};

const FRAMES: u32 = 60;

fn main() {
    let gpu = pollster::block_on(GpuContext::request_product()).expect("GPU adapter");
    // The adapter first: a time means nothing without the GPU it was measured on.
    let adapter = gpu.capabilities();
    println!(
        "adapter: {} on {} ({} {})",
        adapter.adapter_name, adapter.backend, adapter.driver, adapter.driver_info
    );
    let device = gpu.device().clone();
    let renderer = Renderer::new(gpu);
    let source = renderer
        .render_solid(
            RenderSize::new(512, 512).expect("size"),
            alpha2_render::Color::rgba(0.8, 0.3, 0.1, 0.9),
        )
        .expect("source");
    let view = source
        .texture()
        .create_view(&wgpu::TextureViewDescriptor::default());
    for (width, height) in [(1_920, 1_080), (3_840, 2_160)] {
        let size = RenderSize::new(width, height).expect("size");
        for (kind, scale) in [("small", 0.1_f32.sqrt()), ("full", 1.0)] {
            for count in [1_usize, 2, 4, 8, 16] {
                let layers = (0..count)
                    .map(|index| {
                        #[allow(clippy::cast_precision_loss)]
                        let step = index as f32 / count as f32;
                        CompositeLayer {
                            texture: &view,
                            visual: LayerTransform {
                                position: if kind == "small" {
                                    [0.2 + 0.6 * step, 0.25 + 0.5 * ((step * 7.0) % 1.0)]
                                } else {
                                    [0.5, 0.5]
                                },
                                anchor: [0.5, 0.5],
                                scale: [scale, scale],
                                rotation_degrees: 0.0,
                                opacity: 0.9,
                            },
                            blend_mode: BlendMode::Normal,
                        }
                    })
                    .collect::<Vec<_>>();
                let frame = || {
                    drop(renderer.render_layers(size, &layers).expect("composite"));
                    device
                        .poll(wgpu::PollType::wait_indefinitely())
                        .expect("GPU idle");
                };
                for _ in 0..10 {
                    frame();
                }
                let started = Instant::now();
                for _ in 0..FRAMES {
                    frame();
                }
                let per_frame = started.elapsed().as_secs_f64() / f64::from(FRAMES);
                println!(
                    "composite_layers size={width}x{height} layers={count} kind={kind} \
                     ms_per_frame={:.3}",
                    per_frame * 1e3
                );
            }
        }
    }
}
