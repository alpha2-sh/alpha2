//! CPU pictures pinned by hash: identical bytes on every target and thread count. Needs no GPU.

use std::sync::Arc;

use super::cpu_parity::test_card;
use crate::{
    BlendMode, BlendSpace, Color, CpuImage, CpuRenderer, CpuSource, EffectId, EffectParams,
    EffectRuntime, GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, ParamValue, PlaneTarget,
    RenderGraph, RenderSize, SourceView, Yuv420pTargets,
};

/// FNV-1a; a pin, not a security property.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn frame(threads: usize) -> Vec<u8> {
    let size = RenderSize::new(160, 90).expect("size");
    draw(
        &CpuRenderer::new(threads),
        size,
        Color::rgba(0.1, 0.2, 0.3, 1.0),
    )
}

/// One frame of every kind of work (blur, transition, transformed layers in several modes over a
/// background), packed into I420.
fn draw(renderer: &CpuRenderer, size: RenderSize, background: Color) -> Vec<u8> {
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
                #[allow(clippy::cast_precision_loss)]
                ratio: size.width as f32 / size.height as f32,
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
            background,
        )
        .expect("composite");
    graph.set_output(output).expect("output");
    let (width, height) = (size.width as usize, size.height as usize);
    let mut planes = vec![0_u8; width * height * 3 / 2];
    let (y, chroma) = planes.split_at_mut(width * height);
    let (u, v) = chroma.split_at_mut(width * height / 4);
    renderer
        .render_graph_into_yuv420p(
            &graph,
            alpha2_media::YuvEncoding::new(
                alpha2_media::ColorMetadata::EXPORT,
                alpha2_media::ChromaLocation::Left,
            ),
            &mut Yuv420pTargets {
                y: PlaneTarget::new(y, width),
                u: PlaneTarget::new(u, width / 2),
                v: PlaneTarget::new(v, width / 2),
            },
        )
        .expect("render");
    planes
}

#[test]
fn cpu_frames_are_the_same_on_every_target_and_thread_count() {
    let one = frame(1);
    assert_eq!(one, frame(3), "three threads draw another frame");
    assert_eq!(one, frame(8), "eight threads draw another frame");
    assert_eq!(fnv(&one), PINNED, "this target draws another frame");
}

const PINNED: u64 = 4_158_224_015_832_628_688;

#[test]
fn reused_frame_buffers_do_not_change_the_next_frame() {
    // Large enough that every picture's buffer is pooled.
    let size = RenderSize::new(640, 360).expect("size");
    let background = Color::rgba(0.1, 0.2, 0.3, 1.0);
    let fresh = draw(&CpuRenderer::new(3), size, background);
    let renderer = CpuRenderer::new(3);
    let other = draw(&renderer, size, Color::rgba(0.4, 0.3, 0.0, 0.5));
    assert_ne!(other, fresh);
    assert_eq!(draw(&renderer, size, background), fresh);
    assert!(renderer.stats().reused_frame_buffers > 0);
    assert!(renderer.release_frame_buffers() > 0);
    let cached = renderer.result_cache_stats().bytes;
    assert_eq!(renderer.release_idle_memory(), cached);
}
