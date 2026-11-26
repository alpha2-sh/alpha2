//! Pooled textures keep their default view, so warm frames create no texture views.

use super::{centred, gpu, gpu_test_lock, plane, runtime_tint_effect, visual};
use crate::pool::VIEWS_CREATED;
use crate::readback::read_rgba8;
use crate::{
    BlendMode, EffectParams, EffectRuntime, GraphLayer, ParamValue, RenderGraph, RenderSize,
    Renderer, SourcePresentation, SourceView,
};
use alpha2_media::ColorMetadata;

/// Presents two sources, tints and composites them, and reads back RGBA8 and YUV 4:2:0.
fn frame(renderer: &Renderer, tint: &crate::EffectId, size: RenderSize, frame: u8) -> Vec<u8> {
    let pixels = |seed: u8| {
        (0..size.width * size.height)
            .flat_map(|index| {
                let value = u8::try_from(index % 251).expect("small").wrapping_add(seed);
                [value, 255 - value, seed, 255]
            })
            .collect::<Vec<_>>()
    };
    let stride = usize::try_from(size.width * 4).expect("small stride");
    let present = |bytes: &[u8]| {
        let (texture, pending) = renderer
            .prepare_rgba8_presented(
                plane(bytes, stride),
                size,
                SourcePresentation::identity(size),
            )
            .expect("source presents")
            .into_parts();
        if let Some(pending) = pending {
            assert!(renderer.submit_pending_source(&pending));
        }
        texture
    };
    let (first, second) = (pixels(frame), pixels(frame.wrapping_add(90)));
    let (first, second) = (present(&first), present(&second));
    let mut graph = RenderGraph::new(size);
    let tinted = [first.view(), second.view()].map(|view| {
        let source = graph.add_source(SourceView::from_wgpu(view));
        graph
            .add_effect(
                tint.clone(),
                source,
                EffectParams::new().with("tint", ParamValue::Color([0.5, 1.0, 0.75, 1.0])),
                EffectRuntime::default(),
            )
            .expect("dependency exists")
    });
    let output = graph
        .add_composite(vec![
            GraphLayer {
                source: tinted[0],
                visual: visual([1.0, 1.0], 1.0),
                blend_mode: BlendMode::Normal,
            },
            GraphLayer {
                source: tinted[1],
                visual: visual([0.5, 0.5], 0.75),
                blend_mode: BlendMode::Normal,
            },
        ])
        .expect("dependencies exist");
    graph.set_output(output).expect("output exists");
    let result = renderer.execute_graph(&graph).expect("graph renders");
    let rgba = read_rgba8(renderer, &result);
    let planes = renderer
        .render_graph_yuv420p(&graph, &[], centred(ColorMetadata::default()))
        .expect("frame submits")
        .wait()
        .expect("frame reads back");
    assert_eq!(planes.y.len(), rgba.len() / 4);
    rgba
}

#[test]
fn a_warm_frame_creates_no_texture_view() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint effect compiles");
    let size = RenderSize::new(64, 32).expect("size");
    for index in 0..2 {
        frame(&renderer, &tint, size, index);
    }
    let created = renderer.texture_pool_stats().created;
    let uploads = renderer.upload_textures.stats().created;

    VIEWS_CREATED.set(0);
    for index in 2..8 {
        frame(&renderer, &tint, size, index);
    }
    assert_eq!(
        VIEWS_CREATED.get(),
        0,
        "six warm frames created texture views"
    );
    // The frames reused every texture, so no view was made with a new one either.
    assert_eq!(renderer.texture_pool_stats().created, created);
    assert_eq!(renderer.upload_textures.stats().created, uploads);
}

#[test]
fn a_warm_frame_creates_no_bind_group() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint effect compiles");
    let size = RenderSize::new(64, 32).expect("size");
    for index in 0..8 {
        frame(&renderer, &tint, size, index);
    }
    let created = renderer.texture_pool_stats().created;
    let uploads = renderer.upload_textures.stats().created;
    let warm = renderer.diagnostics().bind_groups;

    for index in 8..16 {
        frame(&renderer, &tint, size, index);
    }
    assert_eq!(renderer.texture_pool_stats().created, created);
    assert_eq!(renderer.upload_textures.stats().created, uploads);
    let after = renderer.diagnostics().bind_groups;
    assert_eq!(
        after.created - warm.created,
        0,
        "eight warm frames created bind groups"
    );
    assert!(after.reused > warm.reused);
}

#[test]
fn a_warm_frame_packed_into_the_same_planes_creates_no_texture_view() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 32).expect("size");
    let plane = |format, width, height| {
        renderer
            .gpu
            .device()
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("test plane target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    };
    let luma = plane(wgpu::TextureFormat::R8Unorm, size.width, size.height);
    let chroma = plane(
        wgpu::TextureFormat::Rg8Unorm,
        size.width / 2,
        size.height / 2,
    );
    let source = renderer
        .render_solid(size, crate::Color::rgba(0.8, 0.3, 0.1, 1.0))
        .expect("source");
    let mut graph = RenderGraph::new(size);
    let node = graph.add_source(SourceView::from_wgpu(source.view()));
    let output = graph
        .add_composite(vec![GraphLayer {
            source: node,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        }])
        .expect("dependency exists");
    graph.set_output(output).expect("output exists");
    let pack = || {
        renderer
            .render_graph_nv12_timed(
                &graph,
                &[],
                crate::Nv12PlaneTargets {
                    luma: &luma,
                    chroma: &chroma,
                },
                centred(ColorMetadata::default()),
                &mut crate::FrameSubmitTimings::default(),
            )
            .expect("frame submits");
    };
    for _ in 0..2 {
        pack();
    }
    VIEWS_CREATED.set(0);
    for _ in 0..6 {
        pack();
    }
    assert_eq!(
        VIEWS_CREATED.get(),
        0,
        "six warm frames packed into the same two planes created texture views"
    );
}

#[test]
fn a_kept_bind_group_leaves_with_the_texture_it_binds() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint effect compiles");
    let size = RenderSize::new(64, 32).expect("size");
    let first = frame(&renderer, &tint, size, 0);
    assert!(renderer.diagnostics().bind_groups.kept > 0);
    renderer.clear_texture_pool();
    renderer.upload_textures.clear();
    assert_eq!(renderer.diagnostics().bind_groups.kept, 0);
    assert_eq!(frame(&renderer, &tint, size, 0), first);
}

#[test]
fn a_bind_group_over_a_caller_texture_is_made_for_each_pass() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint effect compiles");
    let texture = super::rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut graph = RenderGraph::new(RenderSize::new(8, 4).expect("size"));
    let source = graph.add_source(SourceView::from_wgpu(&view));
    let tinted = graph
        .add_effect(
            tint,
            source,
            EffectParams::new().with("tint", ParamValue::Color([0.5, 1.0, 0.75, 1.0])),
            EffectRuntime::default(),
        )
        .expect("dependency exists");
    graph.set_output(tinted).expect("output exists");
    for _ in 0..3 {
        drop(renderer.execute_graph(&graph).expect("graph renders"));
    }
    let before = renderer.diagnostics().bind_groups;
    drop(renderer.execute_graph(&graph).expect("graph renders"));
    let after = renderer.diagnostics().bind_groups;
    assert_eq!(after.created - before.created, 1);
    assert_eq!(after.kept, before.kept);
}

#[test]
fn a_texture_back_from_the_pool_draws_through_its_kept_view() {
    // Different pixels each frame into the same pooled textures; the kept views must read back each
    // frame's own picture.
    let _lock = gpu_test_lock();
    let warm = Renderer::new(gpu());
    let tint = pollster::block_on(warm.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint effect compiles");
    let size = RenderSize::new(64, 32).expect("size");
    for index in 0..4 {
        let fresh = Renderer::new(gpu());
        let fresh_tint = pollster::block_on(fresh.register_effect(runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        )))
        .expect("tint effect compiles");
        let seed = index * 37;
        assert_eq!(
            frame(&warm, &tint, size, seed),
            frame(&fresh, &fresh_tint, size, seed),
            "frame {index}"
        );
    }
    assert!(warm.texture_pool_stats().reused > 0);
}
