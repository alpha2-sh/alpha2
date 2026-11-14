//! A pipeline whose first creation fails is not kept: that frame fails with the creation's error
//! and the next frame creates it again.

use std::sync::atomic::Ordering;

use super::{gpu, gpu_test_lock, rgba_texture, runtime_tint_effect, visual};
use crate::readback::read_rgba8;
use crate::{
    BlendMode, CompositeLayer, EffectParams, EffectRuntime, ParamValue, RenderError, RenderGraph,
    RenderSize, Renderer, SourceView,
};
use alpha2_gpu::GpuErrorKind;

fn composite(renderer: &Renderer, view: &wgpu::TextureView) -> Result<Vec<u8>, RenderError> {
    renderer
        .render_layers(
            RenderSize::new(16, 16).expect("size"),
            &[CompositeLayer {
                texture: view,
                visual: visual([1.0, 1.0], 1.0),
                blend_mode: BlendMode::Normal,
            }],
        )
        .map(|output| read_rgba8(renderer, &output))
}

#[test]
fn a_composite_pipeline_that_fails_to_create_fails_one_frame() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    renderer.fail_next_pipeline.store(true, Ordering::Relaxed);
    assert_eq!(
        composite(&renderer, &view).err(),
        Some(RenderError::Gpu {
            kind: GpuErrorKind::Validation
        })
    );
    let drawn = composite(&renderer, &view).expect("the next frame creates the pipeline again");
    assert_eq!(drawn[..4], [200, 100, 50, 255]);
}

#[test]
fn a_built_in_effect_pipeline_that_fails_to_create_fails_one_frame() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tint = renderer
        .register_lazily(runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        ))
        .expect("registers");
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let draw = || {
        let mut graph = RenderGraph::new(RenderSize::new(4, 4).expect("size"));
        let source = graph.add_source(SourceView::from_wgpu(&view));
        let tinted = graph
            .add_effect(
                tint.clone(),
                source,
                EffectParams::new().with("tint", ParamValue::Color([1.0, 0.5, 1.0, 1.0])),
                EffectRuntime::default(),
            )
            .expect("effect");
        graph.set_output(tinted).expect("output");
        renderer
            .execute_graph(&graph)
            .map(|output| read_rgba8(&renderer, &output))
            .map_err(crate::GraphError::into_render_error)
    };
    renderer.fail_next_pipeline.store(true, Ordering::Relaxed);
    assert_eq!(
        draw().err(),
        Some(RenderError::Gpu {
            kind: GpuErrorKind::Validation
        })
    );
    let drawn = draw().expect("the next frame creates the pipeline again");
    assert_eq!(drawn[..4], [200, 50, 50, 255]);
}

#[test]
fn warmup_reports_a_pipeline_that_fails_to_create() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tint = renderer
        .register_lazily(runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        ))
        .expect("registers");
    renderer.fail_next_pipeline.store(true, Ordering::Relaxed);
    assert!(renderer.warmup(std::slice::from_ref(&tint)).is_err());
    renderer
        .warmup(std::slice::from_ref(&tint))
        .expect("the next warmup creates it");
}
