//! The CPU composite and pack against the GPU's.

use super::cpu_parity::{Parity, Side, Tolerance, assert_close, test_card};
use crate::{BlendMode, BlendSpace, Color, GraphLayer, LayerTransform, RenderSize, Yuv420pTargets};

fn size() -> RenderSize {
    RenderSize::new(96, 64).expect("size")
}

fn visual(position: [f32; 2], scale: [f32; 2], rotation: f32, opacity: f32) -> LayerTransform {
    LayerTransform {
        position,
        anchor: [0.5, 0.5],
        scale,
        rotation_degrees: rotation,
        opacity,
    }
}

fn background() -> Color {
    Color::rgba(0.2, 0.35, 0.6, 1.0)
}

#[test]
fn a_background_alone_matches() {
    let parity = Parity::new();
    parity.check("background", size(), Tolerance::Exact, |graph, _| {
        graph.add_composite_over(Vec::new(), background())
    });
}

#[test]
fn transformed_layers_match() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let cases: [(&str, LayerTransform, Tolerance); 7] = [
        (
            "identity",
            visual([0.5, 0.5], [1.0, 1.0], 0.0, 1.0),
            Tolerance::Close,
        ),
        (
            "offset",
            visual([0.437, 0.52], [1.0, 1.0], 0.0, 1.0),
            Tolerance::Close,
        ),
        (
            "magnified",
            visual([0.5, 0.5], [2.3, 1.7], 0.0, 1.0),
            Tolerance::Close,
        ),
        (
            "minified",
            visual([0.4, 0.6], [0.31, 0.27], 0.0, 1.0),
            Tolerance::Close,
        ),
        (
            "flipped",
            visual([0.5, 0.5], [-0.8, 0.9], 0.0, 0.75),
            Tolerance::Close,
        ),
        (
            "rotated",
            visual([0.5, 0.5], [0.7, 0.6], 27.0, 1.0),
            Tolerance::Edges,
        ),
        (
            "small",
            visual([0.2, 0.3], [0.1, 0.12], 0.0, 0.5),
            Tolerance::Close,
        ),
    ];
    for (name, transform, tolerance) in cases {
        for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
            parity.check(
                &format!("layer-{name}-{space:?}"),
                size(),
                tolerance,
                |graph, side| {
                    graph.set_blend_space(space);
                    let source = graph.add_source(card.view(side));
                    graph.add_composite_over(
                        vec![GraphLayer {
                            source,
                            visual: transform,
                            blend_mode: BlendMode::Normal,
                        }],
                        background(),
                    )
                },
            );
        }
    }
}

#[test]
fn every_blend_mode_matches_in_both_spaces() {
    let parity = Parity::new();
    let bottom = parity.picture(size(), &test_card(size(), true));
    let top = parity.picture(size(), &test_card(size(), false));
    let modes = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
        BlendMode::HardLight,
        BlendMode::SoftLight,
        BlendMode::Difference,
        BlendMode::Exclusion,
        BlendMode::Hue,
        BlendMode::Saturation,
        BlendMode::Color,
        BlendMode::Luminosity,
    ];
    for mode in modes {
        for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
            // The non-separable modes branch on whether a color is gray (SetSat's
            // `high > low`), and dodge and burn divide by what is left of white, so a
            // pixel near those points can land apart on any two devices.
            let tolerance = if matches!(
                mode,
                BlendMode::Hue
                    | BlendMode::Saturation
                    | BlendMode::Color
                    | BlendMode::Luminosity
                    | BlendMode::ColorDodge
                    | BlendMode::ColorBurn
            ) {
                Tolerance::Unstable
            } else {
                Tolerance::Close
            };
            parity.check(
                &format!("blend-{mode:?}-{space:?}"),
                size(),
                tolerance,
                |graph, side| {
                    graph.set_blend_space(space);
                    let below = graph.add_source(bottom.view(side));
                    let above = graph.add_source(top.view(side));
                    graph.add_composite_over(
                        vec![
                            GraphLayer {
                                source: below,
                                visual: visual([0.5, 0.5], [1.0, 1.0], 0.0, 1.0),
                                blend_mode: BlendMode::Normal,
                            },
                            GraphLayer {
                                source: above,
                                visual: visual([0.55, 0.45], [-0.9, 0.8], 0.0, 0.7),
                                blend_mode: mode,
                            },
                        ],
                        background(),
                    )
                },
            );
        }
    }
}

#[test]
fn many_small_layers_match_a_batched_gpu_composite() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    parity.check("many-layers", size(), Tolerance::Close, |graph, side| {
        let source = graph.add_source(card.view(side));
        #[allow(clippy::cast_precision_loss)]
        let layers = (0..11)
            .map(|index| GraphLayer {
                source,
                visual: visual(
                    [0.1 + index as f32 * 0.08, 0.2 + (index % 3) as f32 * 0.25],
                    [0.2, 0.25],
                    0.0,
                    0.4 + index as f32 * 0.05,
                ),
                blend_mode: if index % 2 == 0 {
                    BlendMode::Normal
                } else {
                    BlendMode::Screen
                },
            })
            .collect();
        graph.add_composite(layers)
    });
}

#[test]
fn the_yuv420_pack_matches_the_gpu_readback() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), true));
    for chroma in [
        alpha2_media::ChromaLocation::Center,
        alpha2_media::ChromaLocation::Left,
        alpha2_media::ChromaLocation::TopLeft,
    ] {
        let encoding = alpha2_media::YuvEncoding::new(alpha2_media::ColorMetadata::EXPORT, chroma);
        let build = |side: Side| {
            let mut graph = crate::RenderGraph::new(size());
            let source = graph.add_source(card.view(side));
            let output = graph
                .add_composite_over(
                    vec![GraphLayer {
                        source,
                        visual: visual([0.5, 0.5], [0.9, 0.9], 0.0, 0.8),
                        blend_mode: BlendMode::Normal,
                    }],
                    background(),
                )
                .expect("composite");
            graph.set_output(output).expect("output");
            graph
        };
        let gpu_graph = build(Side::Gpu);
        let gpu_planes = parity
            .gpu
            .render_graph_yuv420p(&gpu_graph, &[], encoding)
            .expect("GPU pack")
            .wait()
            .expect("GPU readback");
        let (width, height) = (size().width as usize, size().height as usize);
        let mut y = vec![0_u8; width * height];
        let mut u = vec![0_u8; width * height / 4];
        let mut v = vec![0_u8; width * height / 4];
        let cpu_graph = build(Side::Cpu);
        parity
            .cpu
            .render_graph_into_yuv420p(
                &cpu_graph,
                encoding,
                &mut Yuv420pTargets {
                    y: crate::PlaneTarget::new(&mut y, width),
                    u: crate::PlaneTarget::new(&mut u, width / 2),
                    v: crate::PlaneTarget::new(&mut v, width / 2),
                },
            )
            .expect("CPU pack");
        let as_rgba = |plane: &[u8]| {
            plane
                .iter()
                .flat_map(|&value| [value, 0, 0, 0])
                .collect::<Vec<_>>()
        };
        let name = format!("pack-{chroma:?}");
        let luma_size = size();
        let chroma_size = RenderSize::new(size().width / 2, size().height / 2).expect("size");
        assert_close(
            &format!("{name}-y"),
            luma_size,
            Tolerance::Close,
            &as_rgba(&gpu_planes.y),
            &as_rgba(&y),
        );
        assert_close(
            &format!("{name}-u"),
            chroma_size,
            Tolerance::Close,
            &as_rgba(&gpu_planes.u),
            &as_rgba(&u),
        );
        assert_close(
            &format!("{name}-v"),
            chroma_size,
            Tolerance::Close,
            &as_rgba(&gpu_planes.v),
            &as_rgba(&v),
        );
    }
}

#[test]
fn the_cpu_picture_does_not_depend_on_its_thread_count() {
    let card = test_card(size(), false);
    let draw = |threads| {
        let renderer = crate::CpuRenderer::new(threads);
        let picture = crate::CpuSource::image(std::sync::Arc::new(
            crate::CpuImage::from_premultiplied_rgba8(size(), &card).expect("picture"),
        ));
        let mut graph = crate::RenderGraph::new(size());
        let source = graph.add_source(crate::SourceView::cpu(&picture));
        let output = graph
            .add_composite_over(
                vec![GraphLayer {
                    source,
                    visual: visual([0.47, 0.5], [0.7, 0.8], 13.0, 0.9),
                    blend_mode: BlendMode::SoftLight,
                }],
                background(),
            )
            .expect("composite");
        graph.set_output(output).expect("output");
        renderer.execute_graph(&graph).expect("draws").read_rgba8()
    };
    let one = draw(1);
    assert_eq!(one, draw(3));
    assert_eq!(one, draw(8));
}
