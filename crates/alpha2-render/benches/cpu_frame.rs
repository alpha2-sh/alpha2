//! The CPU renderer on whole export frames, re-presenting sources every frame as an export does.
//!
//! `CPU_FRAME_CASE` lists cases (default `grid`: four 1080p I420 sources at half scale in
//! quadrants and four text-sized labels); `all` runs every case. Prints one `cpu_frame` line per
//! case and thread count with wall and process CPU time per frame (`cpu_ms=n/a` where
//! unsupported) and the renderer's time per stage; not gated. Thread counts default to 1, 4,
//! half and all cores unless `CPU_FRAME_THREADS` lists others.

use std::sync::Arc;
use std::time::Instant;

use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedTransform, VectorScene, build_scene,
};
use alpha2_media::{
    BitDepth, Chroma, ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange,
    DynamicRange, Orientation, PixelLayout, PlaneLayout, Rotation, TransferFunction, VideoCodec,
    YuvEncoding, YuvFormat,
};
use alpha2_model::{LineCap, LineJoin, Paint, Rgba};
use alpha2_render::{
    BlendMode, ColorPolicy, CpuImage, CpuRenderStats, CpuRenderer, CpuSource, EffectId,
    EffectParams, EffectRuntime, GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, ParamValue,
    PlaneTarget, RenderGraph, RenderNodeId, RenderSize, SourceAlpha, SourcePlane,
    SourcePresentation, SourceView, Yuv420pTargets, YuvFrame, YuvPlanes, hdr::HdrToneMap,
};

const FRAMES: u32 = 20;

const CASES: [&str; 20] = [
    "grid",
    "clip",
    "portrait",
    "minify",
    "rgba-still",
    "hlg",
    "pq",
    "bt2020-sdr",
    "yuva",
    "blur",
    "blur-wide",
    "fade",
    "opacity",
    "rotated",
    "swirl",
    "lens-blur",
    "grade",
    "vector",
    "vector-moving",
    "pack422p10",
];

fn frames() -> u32 {
    std::env::var("CPU_FRAME_COUNT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(FRAMES)
}

fn size(width: u32, height: u32) -> RenderSize {
    RenderSize::new(width, height).expect("size")
}

#[derive(Debug)]
struct Planes {
    format: YuvFormat,
    data: Vec<(Vec<u8>, u32)>,
    alpha: Option<(Vec<u8>, u32)>,
    size: RenderSize,
    color: ColorMetadata,
}

impl YuvPlanes for Planes {
    fn frame(&self) -> YuvFrame<'_> {
        fn plane((bytes, stride): &(Vec<u8>, u32)) -> SourcePlane<'_> {
            SourcePlane {
                bytes,
                stride: *stride,
            }
        }
        YuvFrame {
            format: self.format,
            planes: [
                plane(&self.data[0]),
                plane(&self.data[1]),
                self.data.get(2).map_or(
                    SourcePlane {
                        bytes: &[],
                        stride: 0,
                    },
                    plane,
                ),
            ],
            alpha_plane: self.alpha.as_ref().map(plane),
            size: self.size,
            alpha: 1.0,
            color: Some(self.color),
        }
    }
}

/// Ramps with noise: 8-bit codes, or 10-bit codes in little-endian words (high-aligned when the
/// format says so).
fn noise(count: usize, format: YuvFormat, state: &mut u32) -> Vec<u8> {
    let wide = format.bytes_per_sample() == 2;
    let mut bytes = Vec::with_capacity(count * if wide { 2 } else { 1 });
    for index in 0..count {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        #[allow(clippy::cast_possible_truncation)]
        let code = ((index % 251) as u8).wrapping_add((*state >> 28) as u8);
        if wide {
            let code = (u16::from(code) << 2) | u16::from(code >> 6);
            let code = if format.msb_aligned { code << 6 } else { code };
            bytes.extend_from_slice(&code.to_le_bytes());
        } else {
            bytes.push(code);
        }
    }
    bytes
}

fn planes(format: YuvFormat, size: RenderSize, color: ColorMetadata, seed: u32) -> Arc<Planes> {
    let mut state = seed;
    let bytes = format.bytes_per_sample() as usize;
    let (width, height) = (size.width as usize, size.height as usize);
    let (chroma_width, chroma_height) = format.chroma_size(size.width, size.height);
    let (chroma_width, chroma_height) = (chroma_width as usize, chroma_height as usize);
    let stride = |width: usize| u32::try_from(width * bytes).expect("stride");
    let mut planes = vec![(noise(width * height, format, &mut state), stride(width))];
    if format.interleaved_chroma {
        planes.push((
            noise(chroma_width * chroma_height * 2, format, &mut state),
            stride(chroma_width * 2),
        ));
    } else {
        for _ in 0..2 {
            planes.push((
                noise(chroma_width * chroma_height, format, &mut state),
                stride(chroma_width),
            ));
        }
    }
    let alpha = format.alpha.then(|| (overlay_alpha(size), stride(width)));
    Arc::new(Planes {
        format,
        data: planes,
        alpha,
        size,
        color,
    })
}

/// An 8-bit alpha plane opaque in a centred box of a fifth of the frame, with 16-pixel soft
/// edges, and transparent elsewhere.
fn overlay_alpha(size: RenderSize) -> Vec<u8> {
    let (width, height) = (f64::from(size.width), f64::from(size.height));
    let (half_width, half_height) = (width * 0.25, height * 0.2);
    let mut alpha = Vec::with_capacity(size.width as usize * size.height as usize);
    for y in 0..size.height {
        for x in 0..size.width {
            let dx = (f64::from(x) + 0.5 - width / 2.0).abs() - half_width;
            let dy = (f64::from(y) + 0.5 - height / 2.0).abs() - half_height;
            let outside = dx.max(dy);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            alpha.push(((0.5 - outside / 16.0).clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    alpha
}

fn bt2020(transfer: TransferFunction) -> ColorMetadata {
    ColorMetadata {
        primaries: ColorPrimaries::Bt2020,
        transfer,
        matrix: ColorMatrix::Bt2020Ncl,
        range: ColorRange::Limited,
        dynamic_range: if matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg) {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        },
    }
}

fn default_threads() -> Vec<usize> {
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let mut threads = vec![1, 4, (available / 2).max(1), available];
    threads.sort_unstable();
    threads.dedup();
    threads
}

fn whole(source: RenderNodeId, opacity: f32, rotation_degrees: f32) -> GraphLayer {
    GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [1.0, 1.0],
            rotation_degrees,
            opacity,
        },
        blend_mode: BlendMode::Normal,
    }
}

fn black() -> alpha2_render::Color {
    alpha2_render::Color::rgba(0.0, 0.0, 0.0, 1.0)
}

fn over_black(graph: &mut RenderGraph<'_>, layers: Vec<GraphLayer>) -> RenderNodeId {
    graph
        .add_composite_over(layers, black())
        .expect("composite")
}

/// 300 stroked ellipses on a 1080p canvas; `moved` shifts the first one.
fn shapes(moved: f64) -> Arc<VectorScene> {
    let items = (0..300_u32)
        .map(|index| {
            let (column, row) = (f64::from(index % 20), f64::from(index / 20));
            let shift = if index == 0 { moved } else { 0.0 };
            EvaluatedItem::Group {
                items: Arc::from(vec![
                    EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                        center: Point::new(0.025 + column * 0.05 + shift, 0.033 + row * 0.066),
                        size: Vec2::new(0.04, 0.05),
                        rotation_degrees: f64::from(index * 7),
                    }),
                    EvaluatedItem::Stroke {
                        paint: Paint::Solid {
                            color: Rgba::new(0.9, 0.6, 0.2, 1.0),
                        },
                        width: 0.004,
                        opacity: 0.9,
                        join: LineJoin::Round,
                        miter_limit: 4.0,
                        cap: LineCap::Round,
                        dash: Vec::new(),
                        dash_offset: 0.0,
                    },
                ]),
                transform: EvaluatedTransform::default(),
            }
        })
        .collect::<Vec<_>>();
    Arc::new(build_scene(&items, CanvasSize::new(1_920, 1_080)))
}

type GraphFn = fn(&mut RenderGraph<'_>, &[RenderNodeId], u32) -> RenderNodeId;

/// What a case draws each frame.
struct Case {
    output: RenderSize,
    yuv: Vec<Arc<Planes>>,
    presentations: Vec<SourcePresentation>,
    policy: ColorPolicy,
    rgba: Option<(Arc<Vec<u8>>, RenderSize)>,
    label: Option<Arc<CpuImage>>,
    graph: GraphFn,
    pack422: bool,
}

impl Case {
    fn yuv(output: RenderSize, sources: Vec<Arc<Planes>>) -> Self {
        let presentations = sources
            .iter()
            .map(|planes| SourcePresentation {
                output_size: output,
                ..SourcePresentation::identity(planes.size)
            })
            .collect();
        Self {
            output,
            yuv: sources,
            presentations,
            policy: ColorPolicy::RejectHdr,
            rgba: None,
            label: None,
            graph: |graph, sources, _| over_black(graph, vec![whole(sources[0], 1.0, 0.0)]),
            pack422: false,
        }
    }

    fn with_graph(mut self, graph: GraphFn) -> Self {
        self.graph = graph;
        self
    }
}

fn effect(
    graph: &mut RenderGraph<'_>,
    source: RenderNodeId,
    id: &'static str,
    params: EffectParams<'static>,
) -> RenderNodeId {
    graph
        .add_effect(
            EffectId::from_static(id, 1),
            source,
            params,
            EffectRuntime {
                ratio: 16.0 / 9.0,
                ..EffectRuntime::default()
            },
        )
        .expect("effect")
}

fn grid(graph: &mut RenderGraph<'_>, sources: &[RenderNodeId], _: u32) -> RenderNodeId {
    // `CPU_FRAME_LAYER_SCALE=0.01` leaves the presentation and the pack alone.
    let scale = std::env::var("CPU_FRAME_LAYER_SCALE")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(0.5);
    let layers = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            #[allow(clippy::cast_precision_loss)]
            let (column, row) = ((index % 2) as f32, (index % 4 / 2) as f32);
            GraphLayer {
                source: *source,
                visual: if index < 4 {
                    LayerTransform {
                        position: [0.25 + 0.5 * column, 0.25 + 0.5 * row],
                        anchor: [0.5, 0.5],
                        scale: [scale, scale],
                        rotation_degrees: 0.0,
                        opacity: 1.0,
                    }
                } else {
                    LayerTransform {
                        position: [0.02 + 0.5 * column, 0.03 + 0.5 * row],
                        anchor: [0.0, 0.0],
                        scale: [260.0 / 1_920.0, 64.0 / 1_080.0],
                        rotation_degrees: 0.0,
                        opacity: 1.0,
                    }
                },
                blend_mode: BlendMode::Normal,
            }
        })
        .collect();
    over_black(graph, layers)
}

#[allow(clippy::too_many_lines)]
fn case(name: &str) -> Case {
    let hd = size(1_920, 1_080);
    let i420 = |seed| planes(YuvFormat::I420, hd, ColorMetadata::EXPORT, seed);
    match name {
        "grid" => {
            let mut case = Case::yuv(hd, (1..=4).map(i420).collect()).with_graph(grid);
            case.label = Some(Arc::new(
                CpuImage::from_premultiplied_rgba8(
                    size(260, 64),
                    &[150_u8, 150, 150, 153].repeat(260 * 64),
                )
                .expect("label"),
            ));
            case
        }
        "clip" => Case::yuv(hd, vec![i420(1)]),
        "portrait" => {
            let mut case = Case::yuv(size(1_080, 1_920), vec![i420(1)]);
            case.presentations[0].orientation = Orientation::rotated(Rotation::Deg90);
            case
        }
        "minify" => Case::yuv(
            size(1_280, 720),
            vec![planes(
                YuvFormat::I420,
                size(3_840, 2_160),
                ColorMetadata::EXPORT,
                1,
            )],
        ),
        "rgba-still" => {
            let still = size(4_000, 3_000);
            let mut state = 7;
            let pixels = noise(4_000 * 3_000 * 4, YuvFormat::I420, &mut state);
            let mut case = Case::yuv(hd, Vec::new());
            case.rgba = Some((Arc::new(pixels), still));
            case.presentations = vec![SourcePresentation {
                output_size: hd,
                ..SourcePresentation::identity(still)
            }];
            case
        }
        "hlg" | "pq" => {
            let transfer = if name == "hlg" {
                TransferFunction::Hlg
            } else {
                TransferFunction::Pq
            };
            let mut case = Case::yuv(hd, vec![planes(YuvFormat::P010, hd, bt2020(transfer), 1)]);
            case.policy = ColorPolicy::ToneMapHdr(HdrToneMap::default());
            case
        }
        "bt2020-sdr" => Case::yuv(
            hd,
            vec![planes(
                YuvFormat::I420,
                hd,
                bt2020(TransferFunction::Bt709),
                1,
            )],
        ),
        "yuva" => Case::yuv(
            hd,
            vec![
                i420(1),
                planes(YuvFormat::I420.with_alpha(), hd, ColorMetadata::EXPORT, 2),
            ],
        )
        .with_graph(|graph, sources, _| {
            over_black(
                graph,
                vec![whole(sources[0], 1.0, 0.0), whole(sources[1], 1.0, 0.0)],
            )
        }),
        "blur" => Case::yuv(hd, vec![i420(1)]).with_graph(|graph, sources, _| {
            effect(
                graph,
                sources[0],
                GAUSSIAN_BLUR_ID,
                EffectParams::new().with("radius", ParamValue::Float(0.04)),
            )
        }),
        "blur-wide" => Case::yuv(hd, vec![i420(1)]).with_graph(|graph, sources, _| {
            graph
                .add_gaussian_pixels(sources[0], 40.0, EffectRuntime::default())
                .expect("blur")
        }),
        "fade" => Case::yuv(hd, vec![i420(1), i420(2)]).with_graph(|graph, sources, _| {
            graph
                .add_transition(
                    EffectId::from_static("alpha2.transition.fade", 1),
                    sources[0],
                    sources[1],
                    EffectParams::new(),
                    EffectRuntime {
                        progress: 0.5,
                        ratio: 16.0 / 9.0,
                        ..EffectRuntime::default()
                    },
                )
                .expect("transition")
        }),
        "opacity" => Case::yuv(hd, vec![i420(1), i420(2)]).with_graph(|graph, sources, _| {
            over_black(
                graph,
                vec![whole(sources[0], 1.0, 0.0), whole(sources[1], 0.5, 0.0)],
            )
        }),
        "rotated" => Case::yuv(hd, vec![i420(1)])
            .with_graph(|graph, sources, _| over_black(graph, vec![whole(sources[0], 1.0, 7.0)])),
        "swirl" => Case::yuv(hd, vec![i420(1)]).with_graph(|graph, sources, _| {
            effect(
                graph,
                sources[0],
                "alpha2.effect.swirl",
                EffectParams::new(),
            )
        }),
        "lens-blur" => Case::yuv(hd, vec![i420(1)]).with_graph(|graph, sources, _| {
            effect(
                graph,
                sources[0],
                "alpha2.effect.lens-blur",
                EffectParams::new(),
            )
        }),
        "grade" => Case::yuv(hd, vec![i420(1)]).with_graph(|graph, sources, _| {
            effect(
                graph,
                sources[0],
                "alpha2.effect.color-controls",
                EffectParams::new()
                    .with("exposure", ParamValue::Float(0.3))
                    .with("contrast", ParamValue::Float(1.2))
                    .with("saturation", ParamValue::Float(1.1)),
            )
        }),
        "vector" => Case::yuv(hd, Vec::new()).with_graph(|graph, _, _| {
            static SCENE: std::sync::OnceLock<Arc<VectorScene>> = std::sync::OnceLock::new();
            let shapes = graph.add_vector(Arc::clone(SCENE.get_or_init(|| shapes(0.0))));
            over_black(graph, vec![whole(shapes, 1.0, 0.0)])
        }),
        "vector-moving" => Case::yuv(hd, Vec::new()).with_graph(|graph, _, frame| {
            let shapes = graph.add_vector(shapes(f64::from(frame % 50) * 0.001));
            over_black(graph, vec![whole(shapes, 1.0, 0.0)])
        }),
        "pack422p10" => {
            let mut case = Case::yuv(hd, vec![i420(1)]);
            case.pack422 = true;
            case
        }
        _ => panic!("unknown case {name}; cases: {}", CASES.join(",")),
    }
}

/// Milliseconds per frame of each stage between two stats.
fn stages(before: CpuRenderStats, after: CpuRenderStats, frames: u32) -> String {
    let per = |before: u64, after: u64| {
        #[allow(clippy::cast_precision_loss)]
        let ms = after.saturating_sub(before) as f64 / 1_000.0 / f64::from(frames);
        format!("{ms:.1}")
    };
    format!(
        "present_ms={} kernel_ms={} composite_ms={} vector_ms={} pack_ms={}",
        per(before.present_micros, after.present_micros),
        per(before.kernel_micros, after.kernel_micros),
        per(before.composite_micros, after.composite_micros),
        per(before.vector_micros, after.vector_micros),
        per(before.pack_micros, after.pack_micros),
    )
}

#[allow(clippy::too_many_lines)]
fn run(name: &str, threads: usize) {
    let case = case(name);
    let output = case.output;
    let (width, height) = (output.width as usize, output.height as usize);
    let mut y = vec![0_u8; width * height];
    let mut u = vec![0_u8; width * height / 4];
    let mut v = vec![0_u8; width * height / 4];
    // 10-bit 4:2:2: two bytes per sample, chroma at half width and full height.
    let mut wide = [
        vec![0_u8; width * height * 2],
        vec![0_u8; width * height],
        vec![0_u8; width * height],
    ];
    let layout = PixelLayout {
        chroma: Chroma::Yuv422,
        bit_depth: BitDepth::Ten,
        alpha: false,
        planes: PlaneLayout::Planar,
    };
    let renderer = CpuRenderer::new(threads);
    let pack_micros = std::cell::Cell::new(0_u64);
    let mut frame = |index: u32| {
        let mut presented = case
            .yuv
            .iter()
            .zip(&case.presentations)
            .map(|(planes, presentation)| {
                renderer
                    .present_yuv(
                        Arc::clone(planes) as Arc<dyn YuvPlanes>,
                        *presentation,
                        case.policy,
                    )
                    .expect("present")
                    .0
            })
            .collect::<Vec<_>>();
        if let Some((pixels, size)) = &case.rgba {
            presented.push(
                renderer
                    .present_rgba8(
                        Arc::clone(pixels) as Arc<dyn alpha2_render::RgbaPixels>,
                        *size,
                        0,
                        case.presentations[0],
                        SourceAlpha::Premultiplied,
                    )
                    .expect("present"),
            );
        }
        if let Some(label) = &case.label {
            presented.extend((0..4).map(|_| CpuSource::image(Arc::clone(label))));
        }
        let mut graph = RenderGraph::new(output);
        let sources = presented
            .iter()
            .map(|source| graph.add_source(SourceView::cpu(source)))
            .collect::<Vec<_>>();
        let node = (case.graph)(&mut graph, &sources, index);
        graph.set_output(node).expect("output");
        if case.pack422 {
            let picture = renderer.execute_graph(&graph).expect("render");
            let started = Instant::now();
            let [y, u, v] = &mut wide;
            alpha2_render::pack_cpu_image(
                &picture,
                YuvEncoding::export(VideoCodec::ProRes),
                layout,
                &mut [
                    PlaneTarget::new(y, width * 2),
                    PlaneTarget::new(u, width),
                    PlaneTarget::new(v, width),
                ],
            )
            .expect("pack");
            pack_micros.set(
                pack_micros.get()
                    + u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            );
            return;
        }
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
    frame(0);
    let frames = frames();
    let before = renderer.stats();
    let pack_before = pack_micros.get();
    let cpu_started = alpha2_os::process_cpu_time();
    let started = Instant::now();
    for index in 1..=frames {
        frame(index);
    }
    let wall = started.elapsed().as_secs_f64() * 1_000.0 / f64::from(frames);
    let cpu = match (cpu_started, alpha2_os::process_cpu_time()) {
        (Some(before), Some(after)) => format!(
            "{:.1}",
            after.saturating_sub(before).as_secs_f64() * 1_000.0 / f64::from(frames)
        ),
        _ => "n/a".to_owned(),
    };
    let mut after = renderer.stats();
    after.pack_micros += pack_micros.get() - pack_before;
    println!(
        "cpu_frame case={name} size={}x{} threads={threads} wall_ms={wall:.1} cpu_ms={cpu} {}",
        output.width,
        output.height,
        stages(before, after, frames)
    );
}

fn main() {
    let threads = std::env::var("CPU_FRAME_THREADS")
        .ok()
        .map_or_else(default_threads, |list| {
            list.split(',')
                .filter_map(|value| value.parse().ok())
                .collect::<Vec<usize>>()
        });
    let names = std::env::var("CPU_FRAME_CASE").unwrap_or_else(|_| "grid".to_owned());
    let names = if names == "all" {
        CASES.to_vec()
    } else {
        names.split(',').collect()
    };
    for name in names {
        for &count in &threads {
            run(name, count);
        }
    }
}
