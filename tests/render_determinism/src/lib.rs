//! CPU renderer frames whose hashes must match on every target, natively and as wasm.
//!
//! Each hash is pinned from linux-x64. `scripts/test-render-determinism.mjs` compares the native
//! values with this crate built for wasm32 and with the packaged browser binding.

use std::sync::Arc;

use alpha2_engine::{
    EngineProject, FrameProvider, FrameRenderer, FramesWithText, QualityProfile, TextTextures,
};
use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedPath, EvaluatedSubpath, EvaluatedVertex,
    build_scene,
};
use alpha2_media::{
    BitDepth, Chroma, ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange,
    DynamicRange, Orientation, PixelLayout, PlaneLayout, Rect, SourceId, TransferFunction,
    VideoCodec, YuvEncoding, YuvFormat,
};
use alpha2_model::{FillRule, GradientStop, LineCap, LineJoin, Paint, Project, Rgba, Spread};
use alpha2_render::{
    BlendMode, BlendSpace, Color, ColorPolicy, CpuImage, CpuRenderer, CpuSource, EffectId,
    EffectParams, EffectRuntime, GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, Lut3dData,
    ParamValue, PlaneTarget, RenderControls, RenderGraph, RenderNodeId, RenderSize, SourceAlpha,
    SourcePlane, SourcePresentation, SourceView, Yuv420pTargets, YuvFrame, YuvPlanes,
    hdr::HdrToneMap,
};
use alpha2_time::MediaTime;

/// [`graph_hash`] on linux-x64; equals the renderer's own determinism test pin.
pub const PINNED_GRAPH_HASH: u64 = 4_158_224_015_832_628_688;

/// [`blur_and_stroke_hash`] on linux-x64.
pub const PINNED_BLUR_AND_STROKE_HASH: u64 = 12_803_358_345_075_414_110;

/// [`project_hash`] on linux-x64.
pub const PINNED_PROJECT_HASH: u64 = 8_340_324_065_314_071_693;

/// [`text_fit_hash`] on linux-x64.
pub const PINNED_TEXT_FIT_HASH: u64 = 2_492_912_094_599_430_155;

/// [`sources_hash`] on linux-x64.
pub const PINNED_SOURCES_HASH: u64 = 2_463_240_201_688_655_318;

/// Captions, text generators and a layout with fitted font sizes and clamped lines.
pub const TEXT_FIT_PROJECT_JSON: &str = include_str!("../../cases/text-fit-project.json");

/// The font [`text_fit_hash`] draws with.
const FIXTURE_FONT: &[u8] = include_bytes!("../../fonts/Alpha2Fixture.ttf");

/// The times [`text_fit_hash`] draws, in ticks. The layout's font size animates between them.
pub const TEXT_FIT_TIMES: [i64; 3] = [0, 36_000, 108_000];

/// The host parity project [`project_hash`] renders.
pub const PROJECT_JSON: &str = include_str!("../../cases/host-parity-effects-project.json");

/// The 3D LUT the project's `grade` resource names.
pub const GRADE_CUBE: &str = include_str!("../../corpus/parity-grade.cube");

fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// A premultiplied RGBA8 test card, with a translucent corner unless `opaque`.
/// `scripts/test-render-determinism.mjs` mirrors it in JavaScript.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // Every value is at most 255.
pub fn test_card(size: RenderSize, opaque: bool) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    let mut state = 0x1234_5678_u32;
    for y in 0..size.height {
        for x in 0..size.width {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 24) as u8;
            let ramp = (x * 255 / size.width.max(1)) as u8;
            let fall = (y * 255 / size.height.max(1)) as u8;
            let checker = if (x / 8 + y / 8) % 2 == 0 { 230 } else { 40 };
            let mut pixel = [
                ramp,
                fall,
                if x < size.width / 2 { checker } else { noise },
                255,
            ];
            if !opaque && x > size.width * 3 / 4 && y > size.height * 3 / 4 {
                let alpha = 96_u16;
                pixel = [
                    (u16::from(pixel[0]) * alpha / 255) as u8,
                    (u16::from(pixel[1]) * alpha / 255) as u8,
                    (u16::from(pixel[2]) * alpha / 255) as u8,
                    alpha as u8,
                ];
            }
            pixels.extend_from_slice(&pixel);
        }
    }
    pixels
}

fn i420(size: RenderSize) -> Vec<u8> {
    let luma = size.width as usize * size.height as usize;
    vec![0; luma + luma / 2]
}

fn targets(planes: &mut [u8], size: RenderSize) -> Yuv420pTargets<'_> {
    let width = size.width as usize;
    let luma = width * size.height as usize;
    let (y, chroma) = planes.split_at_mut(luma);
    let (u, v) = chroma.split_at_mut(luma / 4);
    Yuv420pTargets {
        y: PlaneTarget::new(y, width),
        u: PlaneTarget::new(u, width / 2),
        v: PlaneTarget::new(v, width / 2),
    }
}

/// FNV-1a hash of the fixed graph's I420 frame (blur, transition, blended transformed layers).
///
/// # Panics
/// Panics if the renderer refuses the fixed graph.
#[must_use]
pub fn graph_hash() -> u64 {
    let size = RenderSize::new(160, 90).expect("size");
    let renderer = CpuRenderer::new(1);
    let card = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(size, &test_card(size, false)).expect("card"),
    ));
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(BlendSpace::LinearLight);
    let source = graph.add_source(SourceView::cpu(&card));
    let blurred = graph
        .add_effect(
            EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
            source,
            EffectParams::new().with("radius", ParamValue::Float(0.04)),
            EffectRuntime::default(),
        )
        .expect("blur");
    let crossed = graph
        .add_transition(
            EffectId::from_static("alpha2.transition.cross-warp", 1),
            source,
            blurred,
            EffectParams::new(),
            EffectRuntime {
                progress: 0.4,
                ratio: 160.0 / 90.0,
                ..EffectRuntime::default()
            },
        )
        .expect("transition");
    let layer = |source, rotation: f32, scale: f32, blend_mode| GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.45, 0.55],
            anchor: [0.5, 0.5],
            scale: [scale, scale * 0.9],
            rotation_degrees: rotation,
            opacity: 0.8,
        },
        blend_mode,
    };
    let output = graph
        .add_composite_over(
            vec![
                layer(crossed, 0.0, 1.0, BlendMode::Normal),
                layer(source, 17.0, 0.6, BlendMode::SoftLight),
                layer(blurred, -8.0, 0.35, BlendMode::Hue),
            ],
            Color::rgba(0.1, 0.2, 0.3, 1.0),
        )
        .expect("composite");
    graph.set_output(output).expect("output");
    let mut planes = i420(size);
    renderer
        .render_graph_into_yuv420p(
            &graph,
            YuvEncoding::new(ColorMetadata::EXPORT, alpha2_media::ChromaLocation::Left),
            &mut targets(&mut planes, size),
        )
        .expect("render");
    fnv(FNV_OFFSET, &planes)
}

/// FNV-1a hash of a 1920x1080 I420 frame through the paths that use transcendental math: a
/// multi-pass blur, a rotated layer, and strokes with a cusp and arcs.
///
/// # Panics
/// Panics if the renderer refuses the graph.
#[must_use]
pub fn blur_and_stroke_hash() -> u64 {
    let size = RenderSize::new(1920, 1080).expect("size");
    // Output does not depend on the thread count.
    let renderer = CpuRenderer::new(4);
    let card = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(size, &test_card(size, false)).expect("card"),
    ));
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(BlendSpace::LinearLight);
    let source = graph.add_source(SourceView::cpu(&card));
    let blurred = graph
        .add_gaussian_pixels(source, 40.0, EffectRuntime::default())
        .expect("blur");
    let vertex = |x, y, in_tangent: [f64; 2], out_tangent: [f64; 2]| EvaluatedVertex {
        point: Point::new(x, y),
        in_tangent: Vec2::new(in_tangent[0], in_tangent[1]),
        out_tangent: Vec2::new(out_tangent[0], out_tangent[1]),
    };
    // Control points mirrored across the chord put a cusp at the segment's middle.
    let cusp = EvaluatedPath {
        subpaths: vec![EvaluatedSubpath {
            closed: false,
            vertices: vec![
                vertex(0.3, 0.6, [0.0, 0.0], [0.4, -0.2]),
                vertex(0.7, 0.6, [-0.4, -0.2], [0.0, 0.0]),
            ],
        }],
    };
    let stroke = |width, color: [f32; 4]| EvaluatedItem::Stroke {
        paint: Paint::Solid {
            color: Rgba::new(color[0], color[1], color[2], color[3]),
        },
        width,
        opacity: 0.9,
        join: LineJoin::Round,
        miter_limit: 4.0,
        cap: LineCap::Round,
        dash: Vec::new(),
        dash_offset: 0.0,
    };
    let scene = Arc::new(build_scene(
        &[
            EvaluatedItem::Geometry(EvaluatedGeometry::Path(Arc::new(cusp))),
            stroke(0.03, [0.95, 0.8, 0.2, 1.0]),
            EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                center: Point::new(0.62, 0.38),
                size: Vec2::new(0.37, 0.23),
                rotation_degrees: 21.0,
            }),
            stroke(0.011, [0.2, 0.6, 0.95, 0.8]),
        ],
        CanvasSize::new(size.width, size.height),
    ));
    let shapes = graph.add_vector(scene);
    let layer = |source, rotation: f32, scale: f32| GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [scale, scale],
            rotation_degrees: rotation,
            opacity: 1.0,
        },
        blend_mode: BlendMode::Normal,
    };
    let output = graph
        .add_composite_over(
            vec![
                layer(blurred, 0.0, 1.0),
                layer(source, 23.0, 0.45),
                layer(shapes, 0.0, 1.0),
            ],
            Color::rgba(0.05, 0.05, 0.08, 1.0),
        )
        .expect("composite");
    graph.set_output(output).expect("output");
    let mut planes = i420(size);
    renderer
        .render_graph_into_yuv420p(
            &graph,
            YuvEncoding::new(ColorMetadata::EXPORT, alpha2_media::ChromaLocation::Left),
            &mut targets(&mut planes, size),
        )
        .expect("render");
    fnv(FNV_OFFSET, &planes)
}

/// Decoded planes owned by the test.
#[derive(Debug)]
struct OwnedPlanes {
    format: YuvFormat,
    data: Vec<(Vec<u8>, u32)>,
    alpha: Option<(Vec<u8>, u32)>,
    size: RenderSize,
    color: ColorMetadata,
}

impl YuvPlanes for OwnedPlanes {
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

/// `count` samples of ramps with noise: 8-bit codes, or 10-bit codes in little-endian words
/// (high-aligned when `format` says so).
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

/// Planes of `format` with a soft alpha ramp across the middle third when the format has alpha.
fn synthetic_planes(format: YuvFormat, size: RenderSize, color: ColorMetadata) -> OwnedPlanes {
    let mut state = 0x2545_f491;
    let bytes = format.bytes_per_sample() as usize;
    let (width, height) = (size.width as usize, size.height as usize);
    let (chroma_width, chroma_height) = format.chroma_size(size.width, size.height);
    let (chroma_width, chroma_height) = (chroma_width as usize, chroma_height as usize);
    let stride = |samples: usize| u32::try_from(samples * bytes).expect("stride");
    let mut data = vec![(noise(width * height, format, &mut state), stride(width))];
    if format.interleaved_chroma {
        let samples = chroma_width * 2;
        data.push((
            noise(samples * chroma_height, format, &mut state),
            stride(samples),
        ));
    } else {
        for _ in 0..2 {
            data.push((
                noise(chroma_width * chroma_height, format, &mut state),
                stride(chroma_width),
            ));
        }
    }
    let alpha = format.alpha.then(|| {
        let third = width / 3;
        let plane = (0..height)
            .flat_map(|_| {
                (0..width).map(move |x| {
                    #[allow(clippy::cast_possible_truncation)]
                    let alpha = (x.saturating_sub(third) * 255 / third).min(255) as u8;
                    alpha
                })
            })
            .collect::<Vec<u8>>();
        (plane, stride(width))
    });
    OwnedPlanes {
        format,
        data,
        alpha,
        size,
        color,
    }
}

/// The identity layer of [`sources_hash`]'s pictures.
fn identity(source: RenderNodeId) -> GraphLayer {
    GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [1.0, 1.0],
            rotation_degrees: 0.0,
            opacity: 1.0,
        },
        blend_mode: BlendMode::Normal,
    }
}

/// `picture` packed into `layout`'s planes.
fn packed(
    renderer: &CpuRenderer,
    picture: &CpuImage,
    layout: PixelLayout,
    encoding: YuvEncoding,
) -> Vec<u8> {
    let size = picture.size();
    let planes = layout.planes(size.width, size.height);
    let mut bytes = planes
        .iter()
        .map(|&(width, rows, sample)| vec![0_u8; (width * sample * rows) as usize])
        .collect::<Vec<_>>();
    let mut targets = bytes
        .iter_mut()
        .zip(&planes)
        .map(|(plane, &(width, _, sample))| PlaneTarget::new(plane, (width * sample) as usize))
        .collect::<Vec<_>>();
    renderer
        .pack_image(picture, encoding, layout, &mut targets)
        .expect("pack");
    bytes.concat()
}

/// FNV-1a hash of decoded sources through every presentation path: I420 upright, in all eight
/// orientations, cropped and anamorphic, minified 3x and 5x; NV12; P010 and BT.2020 SDR; YUVA
/// with a soft edge; HLG and PQ tone-mapped; straight RGBA8 minified; a gradient fill. Each
/// picture is packed as 10-bit 4:4:4 with alpha, then all are composited and packed as 8-bit
/// 4:2:0, 10-bit 4:2:2 and 8-bit 4:4:4 with alpha.
///
/// # Panics
/// Panics if the renderer refuses a source or the graph.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn sources_hash() -> u64 {
    let canvas = RenderSize::new(192, 108).expect("size");
    let sized = |width, height| RenderSize::new(width, height).expect("size");
    let renderer = CpuRenderer::new(3);
    let sdr = |primaries, matrix| ColorMetadata {
        primaries,
        transfer: TransferFunction::Bt709,
        matrix,
        range: ColorRange::Limited,
        dynamic_range: DynamicRange::Sdr,
    };
    let hdr = |transfer| ColorMetadata {
        primaries: ColorPrimaries::Bt2020,
        transfer,
        matrix: ColorMatrix::Bt2020Ncl,
        range: ColorRange::Limited,
        dynamic_range: DynamicRange::Hdr,
    };
    let bt709 = ColorMetadata::EXPORT;
    let placed = |source: RenderSize| SourcePresentation {
        output_size: canvas,
        ..SourcePresentation::identity(source)
    };
    let small = sized(160, 96);
    let mut cases = vec![(YuvFormat::I420, canvas, bt709, placed(canvas))];
    for exif in 1..=8 {
        cases.push((
            YuvFormat::I420,
            small,
            bt709,
            SourcePresentation {
                orientation: Orientation::from_exif(exif).expect("orientation"),
                ..placed(small)
            },
        ));
    }
    cases.extend([
        (
            YuvFormat::I420,
            small,
            bt709,
            SourcePresentation {
                visible_rect: Rect {
                    x: 6,
                    y: 4,
                    width: 140,
                    height: 84,
                },
                sample_aspect_ratio: (4, 3),
                chroma_siting: ChromaLocation::Left,
                ..placed(small)
            },
        ),
        (
            YuvFormat::I420,
            sized(576, 324),
            bt709,
            placed(sized(576, 324)),
        ),
        (
            YuvFormat::I420,
            sized(960, 540),
            bt709,
            placed(sized(960, 540)),
        ),
        (YuvFormat::NV12, canvas, bt709, placed(canvas)),
        (YuvFormat::P010, canvas, bt709, placed(canvas)),
        (
            YuvFormat::I420,
            canvas,
            sdr(ColorPrimaries::Bt2020, ColorMatrix::Bt2020Ncl),
            placed(canvas),
        ),
        (YuvFormat::I420.with_alpha(), canvas, bt709, placed(canvas)),
        (
            YuvFormat::P010,
            canvas,
            hdr(TransferFunction::Hlg),
            placed(canvas),
        ),
        (
            YuvFormat::P010,
            canvas,
            hdr(TransferFunction::Pq),
            placed(canvas),
        ),
    ]);
    let mut sources = cases
        .into_iter()
        .map(|(format, size, color, presentation)| {
            let policy = if color.is_hdr() {
                ColorPolicy::ToneMapHdr(HdrToneMap::default())
            } else {
                ColorPolicy::RejectHdr
            };
            renderer
                .present_yuv(
                    Arc::new(synthetic_planes(format, size, color)),
                    presentation,
                    policy,
                )
                .expect("present")
                .0
        })
        .collect::<Vec<_>>();
    let still = sized(576, 324);
    let mut state = 0x6c07_8965;
    let mut straight = noise(576 * 324 * 4, YuvFormat::I420, &mut state);
    // Alpha falls from opaque on the left to clear on the right.
    for (index, pixel) in straight.chunks_mut(4).enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let alpha = 255 - (index % 576 * 255 / 575) as u8;
        pixel[3] = alpha;
    }
    sources.push(
        renderer
            .present_rgba8(
                Arc::new(straight),
                still,
                0,
                placed(still),
                SourceAlpha::Straight,
            )
            .expect("present"),
    );
    let wash = Arc::new(build_scene(
        &[
            EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
                center: Point::new(0.5, 0.5),
                size: Vec2::new(0.8, 0.7),
                corner_radius: 0.2,
                rotation_degrees: 9.0,
            }),
            EvaluatedItem::Fill {
                paint: Paint::LinearGradient {
                    start: alpha2_model::Vec2::new(0.13, 0.21),
                    end: alpha2_model::Vec2::new(0.87, 0.74),
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: Rgba::new(0.9, 0.2, 0.1, 1.0),
                        },
                        GradientStop {
                            offset: 0.6,
                            color: Rgba::new(0.1, 0.7, 0.9, 0.5),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: Rgba::new(0.2, 0.1, 0.8, 1.0),
                        },
                    ],
                    spread: Spread::Pad,
                },
                opacity: 1.0,
                rule: FillRule::Nonzero,
            },
        ],
        CanvasSize::new(canvas.width, canvas.height),
    ));
    let alpha444 = PixelLayout {
        chroma: Chroma::Yuv444,
        bit_depth: BitDepth::Ten,
        alpha: true,
        planes: PlaneLayout::Planar,
    };
    let encoding = YuvEncoding::new(bt709, ChromaLocation::Left);
    let mut hash = FNV_OFFSET;
    let mut pictures = Vec::new();
    for index in 0..=sources.len() {
        let mut graph = RenderGraph::new(canvas);
        let node = match sources.get(index) {
            Some(source) => graph.add_source(SourceView::cpu(source)),
            None => graph.add_vector(Arc::clone(&wash)),
        };
        let output = graph
            .add_composite(vec![identity(node)])
            .expect("composite");
        graph.set_output(output).expect("output");
        let picture = renderer.execute_graph(&graph).expect("render");
        hash = fnv(hash, &packed(&renderer, &picture, alpha444, encoding));
        pictures.push(CpuSource::image(picture));
    }
    let mut graph = RenderGraph::new(canvas);
    let layers = pictures
        .iter()
        .enumerate()
        .map(|(index, picture)| {
            #[allow(clippy::cast_precision_loss)]
            let (column, row) = ((index % 5) as f32, (index / 5) as f32);
            let mut layer = identity(graph.add_source(SourceView::cpu(picture)));
            layer.visual = LayerTransform {
                position: [0.1 + column * 0.2, 0.125 + row * 0.25],
                scale: [0.3, 0.3],
                rotation_degrees: 3.0 * column,
                opacity: 0.9,
                ..layer.visual
            };
            layer
        })
        .collect();
    let output = graph
        .add_composite_over(layers, Color::rgba(0.2, 0.15, 0.1, 1.0))
        .expect("composite");
    graph.set_output(output).expect("output");
    let picture = renderer.execute_graph(&graph).expect("render");
    for layout in [
        PixelLayout {
            chroma: Chroma::Yuv422,
            bit_depth: BitDepth::Ten,
            alpha: false,
            planes: PlaneLayout::Planar,
        },
        PixelLayout {
            chroma: Chroma::Yuv444,
            bit_depth: BitDepth::Eight,
            alpha: true,
            planes: PlaneLayout::Planar,
        },
    ] {
        hash = fnv(hash, &packed(&renderer, &picture, layout, encoding));
    }
    let mut planes = i420(canvas);
    renderer
        .render_graph_into_yuv420p(&graph, encoding, &mut targets(&mut planes, canvas))
        .expect("render");
    fnv(hash, &planes)
}

/// Translucent card for the video source, opaque card for the still, at every time.
struct Cards {
    video: CpuSource,
    still: CpuSource,
}

impl FrameProvider for Cards {
    fn texture_for(&self, source: &SourceId, _time: MediaTime) -> Option<SourceView<'_>> {
        match source.as_str() {
            "video" => Some(SourceView::cpu(&self.video)),
            "still" => Some(SourceView::cpu(&self.still)),
            _ => None,
        }
    }
}

/// Supplies no source frames, since the text fit project draws only generators and captions.
struct NoFrames;

impl FrameProvider for NoFrames {
    fn texture_for(&self, _: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        None
    }
}

/// FNV-1a hash of the text fit project's frames at [`TEXT_FIT_TIMES`], drawn on the CPU with
/// the fixture font. The fit search, the clamp and the ellipsis all take part.
///
/// # Panics
/// Panics if the engine refuses the fixed project or its font.
#[must_use]
pub fn text_fit_hash() -> u64 {
    let project = EngineProject::new(Project::from_json(TEXT_FIT_PROJECT_JSON).expect("project"))
        .expect("snapshot");
    let settings = &project.project().settings;
    let size = RenderSize::new(settings.width, settings.height).expect("size");
    let renderer = CpuRenderer::new(1);
    let mut texts = TextTextures::new();
    texts
        .add_font("Alpha2 Fixture", FIXTURE_FONT, 0)
        .expect("font");
    let mut hash = FNV_OFFSET;
    for ticks in TEXT_FIT_TIMES {
        let frame = project.evaluate(MediaTime(ticks)).expect("frame");
        texts.prepare(&renderer, &frame, size).expect("text");
        let output = renderer
            .render_frame(size, &frame, &FramesWithText::new(&NoFrames, &texts))
            .expect("render");
        hash = fnv(hash, &output.texture.expect("complete").read_rgba8());
    }
    hash
}

/// Every frame time of the parity project, in ticks.
///
/// # Panics
/// Panics if the fixed project does not load.
#[must_use]
pub fn project_frame_times() -> Vec<i64> {
    let project = Project::from_json(PROJECT_JSON).expect("project");
    let end = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .map(|clip| clip.timeline_range.end().ticks())
        .max()
        .unwrap_or(0);
    let rate = project.settings.frame_rate;
    let mut times = Vec::new();
    for frame in 0_i64.. {
        let ticks = rate.frame_start(frame).expect("frame time").ticks();
        if ticks >= end {
            break;
        }
        times.push(ticks);
    }
    times
}

/// FNV-1a hash of every parity project frame rendered on the CPU and packed as an H.264 export.
///
/// # Panics
/// Panics if the engine refuses the fixed project.
#[must_use]
pub fn project_hash() -> u64 {
    let project =
        EngineProject::new(Project::from_json(PROJECT_JSON).expect("project")).expect("snapshot");
    let settings = &project.project().settings;
    let quality = QualityProfile::export();
    let size = quality.export_size(settings).expect("size");
    let renderer = CpuRenderer::new(1);
    renderer
        .register_lut3d("grade", &Lut3dData::from_cube(GRADE_CUBE).expect("cube"))
        .expect("lut");
    renderer.set_texture_budget_bytes(quality.texture_budget_bytes);
    renderer.set_quality_controls(quality.max_filter_samples);
    let card = |opaque| {
        CpuSource::image(Arc::new(
            CpuImage::from_premultiplied_rgba8(size, &test_card(size, opaque)).expect("card"),
        ))
    };
    let cards = Cards {
        video: card(false),
        still: card(true),
    };
    let mut hash = FNV_OFFSET;
    for ticks in project_frame_times() {
        let frame = project.evaluate(MediaTime(ticks)).expect("frame");
        let mut planes = i420(size);
        let report = renderer
            .render_frame_into_yuv420p(
                size,
                &frame,
                &cards,
                YuvEncoding::export(VideoCodec::H264),
                &mut targets(&mut planes, size),
            )
            .expect("render");
        assert!(report.is_complete(), "every source is supplied");
        hash = fnv(hash, &planes);
    }
    hash
}

/// The wasm entry point for [`graph_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuGraphHash)]
#[must_use]
pub fn cpu_graph_hash() -> u64 {
    graph_hash()
}

/// The wasm entry point for [`blur_and_stroke_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuBlurAndStrokeHash)]
#[must_use]
pub fn cpu_blur_and_stroke_hash() -> u64 {
    blur_and_stroke_hash()
}

/// The wasm entry point for [`text_fit_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuTextFitHash)]
#[must_use]
pub fn cpu_text_fit_hash() -> u64 {
    text_fit_hash()
}

/// The wasm entry point for [`project_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuProjectHash)]
#[must_use]
pub fn cpu_project_hash() -> u64 {
    project_hash()
}

/// The wasm entry point for [`sources_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuSourcesHash)]
#[must_use]
pub fn cpu_sources_hash() -> u64 {
    sources_hash()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_fixed_graph_draws_its_pinned_frame() {
        assert_eq!(super::graph_hash(), super::PINNED_GRAPH_HASH);
    }

    #[test]
    fn a_multi_pass_blur_and_a_stroke_hash_to_their_pin() {
        assert_eq!(
            super::blur_and_stroke_hash(),
            super::PINNED_BLUR_AND_STROKE_HASH
        );
    }

    #[test]
    fn the_every_effect_project_hashes_to_its_pin() {
        assert_eq!(super::project_hash(), super::PINNED_PROJECT_HASH);
    }

    #[test]
    fn presented_sources_hash_to_their_pin() {
        assert_eq!(super::sources_hash(), super::PINNED_SOURCES_HASH);
    }

    #[test]
    fn fitted_and_clamped_text_hashes_to_its_pin() {
        assert_eq!(super::text_fit_hash(), super::PINNED_TEXT_FIT_HASH);
    }
}
