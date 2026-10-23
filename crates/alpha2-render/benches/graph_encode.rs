//! CPU time a render graph frame spends on the calling thread: building its
//! passes and submitting them, without waiting for the GPU. The GPU is
//! drained between frames, outside the timed span, so each frame starts from
//! the same idle device and warm pools.
//!
//! Cases, at 1080p and 4K:
//! - `audit`: two sources, a runtime effect on each, two composited layers
//!   (one full frame, one at half scale): the encode cost of a frame of
//!   two clips with an effect each.
//! - `chain`: the same with four effects on each source.
//! - `audit_yuv`: the `audit` graph submitted with its YUV 4:2:0 readback,
//!   as an export frame is; the map is awaited outside the timed span.
//!
//! Prints one `graph_encode` line per case, the median and the mean over
//! the frames. Times are recorded, never gated.

use std::time::{Duration, Instant};

use alpha2_gpu::{CapabilityTier, GpuContext};
use alpha2_media::{VideoCodec, YuvEncoding};
use alpha2_render::{
    BlendMode, Color, EffectDef, EffectId, EffectKind, EffectParams, EffectRuntime, GraphLayer,
    LayerTransform, ParamSpec, ParamType, ParamUnit, ParamValue, RenderGraph, RenderSize, Renderer,
    SourceView,
};

const WARMUP: usize = 20;
const FRAMES: usize = 300;

fn tint_effect() -> EffectDef {
    EffectDef {
        id: "bench.tint".into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![ParamSpec {
            name: "tint".into(),
            param_type: ParamType::Color,
            default: ParamValue::Color([1.0; 4]),
            min: None,
            max: None,
            step: None,
            unit: ParamUnit::Color,
            enum_values: Vec::new(),
        }],
        resources: Vec::new(),
        wgsl_body: "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }"
            .into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    }
}

fn layer(scale: f32, opacity: f32) -> LayerTransform {
    LayerTransform {
        position: [0.5, 0.5],
        anchor: [0.5, 0.5],
        scale: [scale, scale],
        rotation_degrees: 0.0,
        opacity,
    }
}

fn graph<'a>(
    size: RenderSize,
    sources: [&'a wgpu::TextureView; 2],
    tint: &EffectId,
    effects: usize,
) -> RenderGraph<'a> {
    let mut graph = RenderGraph::new(size);
    let tinted = sources.map(|view| {
        let mut node = graph.add_source(SourceView::from_wgpu(view));
        for index in 0..effects {
            #[allow(clippy::cast_precision_loss)]
            let shade = 1.0 - index as f32 * 0.05;
            node = graph
                .add_effect(
                    tint.clone(),
                    node,
                    EffectParams::new().with("tint", ParamValue::Color([shade, 1.0, 0.9, 1.0])),
                    EffectRuntime::default(),
                )
                .expect("dependency exists");
        }
        node
    });
    let output = graph
        .add_composite(vec![
            GraphLayer {
                source: tinted[0],
                visual: layer(1.0, 1.0),
                blend_mode: BlendMode::Normal,
            },
            GraphLayer {
                source: tinted[1],
                visual: layer(0.5, 0.8),
                blend_mode: BlendMode::Normal,
            },
        ])
        .expect("dependencies exist");
    graph.set_output(output).expect("output exists");
    graph
}

fn report(case: &str, size: RenderSize, mut times: Vec<Duration>) {
    times.sort_unstable();
    let median = times[times.len() / 2];
    #[allow(clippy::cast_precision_loss)]
    let mean = times.iter().sum::<Duration>().as_secs_f64() / times.len() as f64;
    println!(
        "graph_encode case={case} size={}x{} frames={} median_us={:.1} mean_us={:.1}",
        size.width,
        size.height,
        times.len(),
        median.as_secs_f64() * 1e6,
        mean * 1e6,
    );
}

fn main() {
    let gpu = pollster::block_on(GpuContext::request_high_performance()).expect("GPU adapter");
    let device = gpu.device().clone();
    let mut renderer = Renderer::new(gpu);
    let tint = pollster::block_on(renderer.register_effect(tint_effect())).expect("tint compiles");
    let idle = || {
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("GPU idle");
    };
    for (width, height) in [(1_920, 1_080), (3_840, 2_160)] {
        let size = RenderSize::new(width, height).expect("size");
        let solids = [
            Color::rgba(0.8, 0.3, 0.1, 1.0),
            Color::rgba(0.1, 0.4, 0.9, 0.9),
        ]
        .map(|color| renderer.render_solid(size, color).expect("source"));
        let views = [&solids[0], &solids[1]].map(|solid| {
            solid
                .texture()
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
        let sources = [&views[0], &views[1]];
        for (case, effects) in [("audit", 1), ("chain", 4)] {
            let graph = graph(size, sources, &tint, effects);
            let mut times = Vec::with_capacity(FRAMES);
            for frame in 0..WARMUP + FRAMES {
                let started = Instant::now();
                let output = renderer.execute_graph(&graph).expect("graph renders");
                let elapsed = started.elapsed();
                drop(output);
                idle();
                if frame >= WARMUP {
                    times.push(elapsed);
                }
            }
            report(case, size, times);
        }
        let graph = graph(size, sources, &tint, 1);
        let mut times = Vec::with_capacity(FRAMES);
        for frame in 0..WARMUP + FRAMES {
            let started = Instant::now();
            let readback = renderer
                .render_graph_yuv420p(&graph, &[], YuvEncoding::export(VideoCodec::H264))
                .expect("frame submits");
            let elapsed = started.elapsed();
            readback.wait().expect("frame reads back");
            idle();
            if frame >= WARMUP {
                times.push(elapsed);
            }
        }
        report("audit_yuv", size, times);
    }
}
