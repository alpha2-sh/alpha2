//! CPU pictures do not depend on where band seams fall or on how many threads draw the bands.

use std::sync::Arc;

use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{CanvasSize, EvaluatedGeometry, EvaluatedItem, build_scene};
use alpha2_media::{
    BitDepth, Chroma, ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange,
    Orientation, PixelLayout, PlaneLayout, Rotation, YuvEncoding, YuvFormat,
};
use alpha2_model::{FillRule, GradientStop, LineCap, LineJoin, Paint, Rgba, Spread};

use super::cpu_determinism::draw;
use super::cpu_parity::test_card;
use super::cpu_presentation::{frame, sdr};
use crate::{
    BlendMode, Color, ColorPolicy, CpuImage, CpuRenderer, CpuSource, EffectId, EffectParams,
    EffectRuntime, GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, ParamValue, PlaneTarget,
    RenderGraph, RenderNodeId, RenderSize, SourceAlpha, SourcePresentation, SourceView,
    Yuv420pTargets,
};

fn size() -> RenderSize {
    RenderSize::new(100, 70).expect("size")
}

fn layer(source: RenderNodeId, scale: f32, rotation_degrees: f32) -> GraphLayer {
    GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.45, 0.55],
            anchor: [0.5, 0.5],
            scale: [scale, scale],
            rotation_degrees,
            opacity: 0.9,
        },
        blend_mode: BlendMode::Normal,
    }
}

fn bits(image: &CpuImage) -> Vec<u8> {
    image
        .stored_bits()
        .iter()
        .flatten()
        .flat_map(|bits| bits.to_le_bytes())
        .collect()
}

/// `graph`'s output as stored, after `build` adds its nodes to a graph holding `sources`.
fn drawn(
    renderer: &CpuRenderer,
    sources: &[&CpuSource],
    build: impl FnOnce(&mut RenderGraph<'_>, &[RenderNodeId]) -> RenderNodeId,
) -> Arc<CpuImage> {
    let mut graph = RenderGraph::new(size());
    let nodes = sources
        .iter()
        .map(|source| graph.add_source(SourceView::cpu(source)))
        .collect::<Vec<_>>();
    let output = build(&mut graph, &nodes);
    graph.set_output(output).expect("output");
    renderer.execute_graph(&graph).expect("draws")
}

fn packed(picture: &CpuImage, layout: PixelLayout) -> Vec<u8> {
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
    crate::pack_cpu_image(
        picture,
        YuvEncoding::new(ColorMetadata::EXPORT, ChromaLocation::TopLeft),
        layout,
        &mut targets,
    )
    .expect("pack");
    bytes.concat()
}

/// Every kind of pass the renderer splits into bands, as named bytes.
#[allow(clippy::too_many_lines)]
fn pictures(renderer: &CpuRenderer) -> Vec<(&'static str, Vec<u8>)> {
    let size = size();
    let mut out = vec![(
        "determinism graph",
        draw(renderer, size, Color::rgba(0.1, 0.2, 0.3, 1.0)),
    )];
    let card = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(size, &test_card(size, false)).expect("card"),
    ));
    let blurred = drawn(renderer, &[&card], |graph, nodes| {
        graph
            .add_effect(
                EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
                nodes[0],
                EffectParams::new().with("radius", ParamValue::Float(0.04)),
                EffectRuntime::default(),
            )
            .expect("blur")
    });
    out.push(("blur", bits(&blurred)));
    let blurred = CpuSource::image(blurred);
    let crossed = drawn(renderer, &[&card, &blurred], |graph, nodes| {
        graph
            .add_transition(
                EffectId::from_static("alpha2.transition.directional-wipe", 1),
                nodes[0],
                nodes[1],
                EffectParams::new(),
                EffectRuntime {
                    progress: 0.4,
                    ratio: 100.0 / 70.0,
                    ..EffectRuntime::default()
                },
            )
            .expect("transition")
    });
    out.push(("transition", bits(&crossed)));
    let composite = drawn(renderer, &[&card, &blurred], |graph, nodes| {
        graph
            .add_composite_over(
                vec![
                    layer(nodes[0], 1.0, 0.0),
                    layer(nodes[1], 0.3, 0.0),
                    layer(nodes[0], 0.6, 17.0),
                ],
                Color::rgba(0.2, 0.1, 0.0, 1.0),
            )
            .expect("composite")
    });
    out.push(("composite", bits(&composite)));
    let bt709 = ColorMetadata::EXPORT;
    let bt2020 = sdr(
        ColorMatrix::Bt2020Ncl,
        ColorRange::Limited,
        ColorPrimaries::Bt2020,
    );
    let placed = |source: RenderSize| SourcePresentation {
        output_size: size,
        ..SourcePresentation::identity(source)
    };
    let turned = SourcePresentation {
        orientation: Orientation {
            rotation: Rotation::Deg90,
            mirror: true,
        },
        ..placed(size)
    };
    let large = RenderSize::new(300, 210).expect("size");
    let presentations = [
        ("upright", YuvFormat::I420, size, bt709, placed(size)),
        ("turned", YuvFormat::I420, size, bt709, turned),
        ("minified", YuvFormat::I420, large, bt709, placed(large)),
        ("bt2020", YuvFormat::I420, size, bt2020, placed(size)),
        ("nv12", YuvFormat::NV12, size, bt709, placed(size)),
        ("p010", YuvFormat::P010, size, bt709, placed(size)),
        (
            "alpha plane",
            YuvFormat::I420.with_alpha(),
            size,
            bt709,
            placed(size),
        ),
    ];
    for (name, format, frame_size, color, presentation) in presentations {
        let (source, _) = renderer
            .present_yuv(
                Arc::new(frame(format, frame_size, color)),
                presentation,
                ColorPolicy::RejectHdr,
            )
            .expect("present");
        let picture = drawn(renderer, &[&source], |graph, nodes| {
            graph
                .add_composite(vec![layer(nodes[0], 1.0, 0.0)])
                .expect("composite")
        });
        out.push((name, bits(&picture)));
    }
    let rgba = renderer
        .present_rgba8(
            Arc::new(test_card(large, false)),
            large,
            0,
            placed(large),
            SourceAlpha::Straight,
        )
        .expect("present");
    let picture = drawn(renderer, &[&rgba], |graph, nodes| {
        graph
            .add_composite(vec![layer(nodes[0], 1.0, 0.0)])
            .expect("composite")
    });
    out.push(("rgba", bits(&picture)));
    let scene = Arc::new(build_scene(
        &[
            EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                center: Point::new(0.48, 0.52),
                size: Vec2::new(0.7, 0.5),
                rotation_degrees: 21.0,
            }),
            EvaluatedItem::Fill {
                paint: Paint::LinearGradient {
                    start: alpha2_model::Vec2::new(0.2, 0.3),
                    end: alpha2_model::Vec2::new(0.8, 0.6),
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: Rgba::new(0.9, 0.3, 0.1, 1.0),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: Rgba::new(0.1, 0.4, 0.9, 0.6),
                        },
                    ],
                    spread: Spread::Pad,
                },
                opacity: 1.0,
                rule: FillRule::Nonzero,
            },
            EvaluatedItem::Stroke {
                paint: Paint::Solid {
                    color: Rgba::new(1.0, 1.0, 0.8, 1.0),
                },
                width: 0.03,
                opacity: 0.8,
                join: LineJoin::Round,
                miter_limit: 4.0,
                cap: LineCap::Round,
                dash: Vec::new(),
                dash_offset: 0.0,
            },
        ],
        CanvasSize::new(size.width, size.height),
    ));
    let shapes = drawn(renderer, &[], |graph, _| graph.add_vector(scene));
    out.push(("vector", bits(&shapes)));
    for (name, layout) in [
        (
            "pack 10-bit 4:2:2",
            PixelLayout {
                chroma: Chroma::Yuv422,
                bit_depth: BitDepth::Ten,
                alpha: false,
                planes: PlaneLayout::Planar,
            },
        ),
        (
            "pack 8-bit 4:2:0 with alpha",
            PixelLayout {
                chroma: Chroma::Yuv420,
                bit_depth: BitDepth::Eight,
                alpha: true,
                planes: PlaneLayout::Planar,
            },
        ),
    ] {
        out.push((name, packed(&composite, layout)));
    }
    let (width, height) = (size.width as usize, size.height as usize);
    let mut planes = vec![0_u8; width * height * 3 / 2];
    let (y, chroma) = planes.split_at_mut(width * height);
    let (u, v) = chroma.split_at_mut(width * height / 4);
    let composite = CpuSource::image(composite);
    let mut graph = RenderGraph::new(size);
    let node = graph.add_source(SourceView::cpu(&composite));
    let output = graph
        .add_composite(vec![layer(node, 1.0, 0.0)])
        .expect("composite");
    graph.set_output(output).expect("output");
    renderer
        .render_graph_into_yuv420p(
            &graph,
            YuvEncoding::new(ColorMetadata::EXPORT, ChromaLocation::TopLeft),
            &mut Yuv420pTargets {
                y: PlaneTarget::new(y, width),
                u: PlaneTarget::new(u, width / 2),
                v: PlaneTarget::new(v, width / 2),
            },
        )
        .expect("render");
    out.push(("pack 8-bit 4:2:0", planes));
    out
}

#[test]
fn cpu_pictures_do_not_depend_on_band_seams_or_threads() {
    let one_band = pictures(&CpuRenderer::with_band_rows(1, usize::MAX));
    for (threads, rows) in [(1, 3), (5, 3), (5, usize::MAX)] {
        let other = pictures(&CpuRenderer::with_band_rows(threads, rows));
        for ((name, expected), (_, actual)) in one_band.iter().zip(&other) {
            assert!(
                expected == actual,
                "{name} differs on {threads} threads with bands of {rows} rows"
            );
        }
    }
}
