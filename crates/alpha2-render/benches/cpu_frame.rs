//! The CPU renderer on the grid frame of the v0.4.2 benchmark: four 1080p
//! I420 sources each shown at half scale in one quadrant, four text-sized
//! labels over them, packed into I420. Each frame presents its sources anew,
//! as an export does. Prints one `cpu_frame` line per thread count with the
//! wall time per frame and the CPU time per frame. Times are recorded, never
//! gated.

use std::sync::Arc;
use std::time::Instant;

use alpha2_media::{ChromaLocation, ColorMetadata, YuvEncoding, YuvFormat};
use alpha2_render::{
    BlendMode, ColorPolicy, CpuImage, CpuRenderer, CpuSource, GraphLayer, LayerTransform,
    PlaneTarget, RenderGraph, RenderSize, SourcePlane, SourcePresentation, SourceView,
    Yuv420pTargets, YuvFrame, YuvPlanes,
};

const FRAMES: u32 = 20;

fn frames() -> u32 {
    std::env::var("CPU_FRAME_COUNT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(FRAMES)
}

#[derive(Debug)]
struct Planes {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    size: RenderSize,
}

impl YuvPlanes for Planes {
    fn frame(&self) -> YuvFrame<'_> {
        YuvFrame {
            format: YuvFormat::I420,
            planes: [
                SourcePlane {
                    bytes: &self.y,
                    stride: self.size.width,
                },
                SourcePlane {
                    bytes: &self.u,
                    stride: self.size.width / 2,
                },
                SourcePlane {
                    bytes: &self.v,
                    stride: self.size.width / 2,
                },
            ],
            alpha_plane: None,
            size: self.size,
            alpha: 1.0,
            color: Some(ColorMetadata::EXPORT),
        }
    }
}

fn planes(size: RenderSize, seed: u32) -> Arc<Planes> {
    let (width, height) = (size.width as usize, size.height as usize);
    let mut state = seed;
    let mut noise = |count: usize| {
        (0..count)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                #[allow(clippy::cast_possible_truncation)]
                let ramp = (index % 251) as u8;
                ramp.wrapping_add((state >> 28) as u8)
            })
            .collect::<Vec<u8>>()
    };
    Arc::new(Planes {
        y: noise(width * height),
        u: noise(width * height / 4),
        v: noise(width * height / 4),
        size,
    })
}

fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map_or(Vec::new(), |(_, rest)| rest.split_whitespace().collect());
    let ticks = |index: usize| {
        fields
            .get(index)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) / 100.0
}

#[allow(clippy::too_many_lines)]
fn main() {
    let size = RenderSize::new(1_920, 1_080).expect("size");
    let sources = (0..4)
        .map(|seed| planes(size, seed + 1))
        .collect::<Vec<_>>();
    let label_size = RenderSize::new(260, 64).expect("size");
    let label = Arc::new(
        CpuImage::from_premultiplied_rgba8(label_size, &[150_u8, 150, 150, 153].repeat(260 * 64))
            .expect("label"),
    );
    let threads = std::env::var("CPU_FRAME_THREADS").ok().map_or_else(
        || vec![1, 4, 8, 24],
        |list| {
            list.split(',')
                .filter_map(|value| value.parse().ok())
                .collect::<Vec<usize>>()
        },
    );
    let (width, height) = (size.width as usize, size.height as usize);
    let mut y = vec![0_u8; width * height];
    let mut u = vec![0_u8; width * height / 4];
    let mut v = vec![0_u8; width * height / 4];
    // `CPU_FRAME_LAYER_SCALE=0.01` leaves the presentation and the pack alone.
    let layer_scale = std::env::var("CPU_FRAME_LAYER_SCALE")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(0.5);
    for count in threads {
        let renderer = CpuRenderer::new(count);
        let mut frame = || {
            let presented = sources
                .iter()
                .map(|planes| {
                    renderer
                        .present_yuv(
                            Arc::clone(planes) as Arc<dyn YuvPlanes>,
                            SourcePresentation::identity(size),
                            ColorPolicy::RejectHdr,
                        )
                        .expect("present")
                        .0
                })
                .collect::<Vec<_>>();
            let labels = (0..4)
                .map(|_| CpuSource::image(Arc::clone(&label)))
                .collect::<Vec<_>>();
            let mut graph = RenderGraph::new(size);
            let mut layers = Vec::new();
            for (index, source) in presented.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let (column, row) = ((index % 2) as f32, (index / 2) as f32);
                layers.push(GraphLayer {
                    source: graph.add_source(SourceView::cpu(source)),
                    visual: LayerTransform {
                        position: [0.25 + 0.5 * column, 0.25 + 0.5 * row],
                        anchor: [0.5, 0.5],
                        scale: [layer_scale, layer_scale],
                        rotation_degrees: 0.0,
                        opacity: 1.0,
                    },
                    blend_mode: BlendMode::Normal,
                });
            }
            for (index, label) in labels.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let (column, row) = ((index % 2) as f32, (index / 2) as f32);
                layers.push(GraphLayer {
                    source: graph.add_source(SourceView::cpu(label)),
                    visual: LayerTransform {
                        position: [0.02 + 0.5 * column, 0.03 + 0.5 * row],
                        anchor: [0.0, 0.0],
                        scale: [260.0 / 1_920.0, 64.0 / 1_080.0],
                        rotation_degrees: 0.0,
                        opacity: 1.0,
                    },
                    blend_mode: BlendMode::Normal,
                });
            }
            let output = graph
                .add_composite_over(layers, alpha2_render::Color::rgba(0.0, 0.0, 0.0, 1.0))
                .expect("composite");
            graph.set_output(output).expect("output");
            renderer
                .render_graph_into_yuv420p(
                    &graph,
                    YuvEncoding::new(ColorMetadata::EXPORT, ChromaLocation::Left),
                    &mut Yuv420pTargets {
                        y: PlaneTarget::new(&mut y, width),
                        u: PlaneTarget::new(&mut u, width / 2),
                        v: PlaneTarget::new(&mut v, width / 2),
                    },
                )
                .expect("render");
        };
        frame();
        let cpu_started = cpu_seconds();
        let started = Instant::now();
        for _ in 0..frames() {
            frame();
        }
        let wall = started.elapsed().as_secs_f64() * 1_000.0 / f64::from(frames());
        let cpu = (cpu_seconds() - cpu_started) * 1_000.0 / f64::from(frames());
        println!("cpu_frame grid 1080p threads={count} wall_ms={wall:.1} cpu_ms={cpu:.1}");
    }
}
