//! A frame the GPU rejects fails with what the GPU said, and leaves nothing
//! behind: no kept result, no pooled texture, no verdict on the device. A lost
//! device fails that frame and every later one.

use std::sync::Arc;

use super::{centred, gpu, gpu_test_lock, nv12_test_plane, rgba_texture, visual};
use crate::pool::INVALID_TEXTURES;
use crate::readback::read_rgba8;
use crate::{
    BlendMode, CacheIntent, Color, GraphError, GraphLayer, RenderCacheKey, RenderError,
    RenderGraph, RenderSize, Renderer, SourceView,
};
use alpha2_gpu::{GpuContext, GpuDiagnostic, GpuDiagnosticLog, GpuErrorKind};
use alpha2_media::ColorMetadata;

const REJECTED: RenderError = RenderError::Gpu {
    kind: GpuErrorKind::Validation,
};

/// A keyed source composited at half opacity, its output kept.
fn cached_graph(source: &wgpu::TextureView) -> RenderGraph<'_> {
    let size = RenderSize::new(8, 4).expect("size");
    let mut graph = RenderGraph::new(size);
    let source =
        graph.add_source_with_key(SourceView::from_wgpu(source), RenderCacheKey::source(11));
    let output = graph
        .add_composite(vec![GraphLayer {
            source,
            visual: visual([1.0, 1.0], 0.5),
            blend_mode: BlendMode::Normal,
        }])
        .expect("composite");
    graph.set_output(output).expect("output");
    graph
        .set_cache_intent(output, CacheIntent::Auto)
        .expect("known node");
    graph
}

/// The same frame on a renderer that never failed.
fn reference_pixels(pixel: [u8; 4]) -> Vec<u8> {
    let renderer = Renderer::new(gpu());
    let texture = rgba_texture(&renderer, pixel);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let output = renderer
        .execute_graph(&cached_graph(&view))
        .expect("renders");
    read_rgba8(&renderer, &output)
}

#[test]
fn a_frame_the_gpu_rejects_fails_with_its_error_and_keeps_nothing() {
    let _lock = gpu_test_lock();
    let pixel = [200, 100, 50, 255];
    let expected = reference_pixels(pixel);
    let renderer = Renderer::new(gpu());
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, pixel);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let graph = cached_graph(&view);

    // The frame's target fails to allocate: its pass, and so its submission, is rejected.
    INVALID_TEXTURES.set(1);
    let failed = renderer.execute_graph(&graph);
    assert_eq!(
        INVALID_TEXTURES.get(),
        0,
        "the frame made the invalid texture"
    );
    assert_eq!(failed.err(), Some(GraphError::Render(REJECTED)));
    // The result it never drew is not kept.
    let cache = renderer.result_cache_stats();
    assert_eq!((cache.inserts, cache.entries), (0, 0));
    // The texture that failed is not pooled for a later frame.
    let pool = renderer.texture_pool_stats();
    assert_eq!((pool.active, pool.available), (0, 0), "{pool:?}");
    // A rejected frame is not taken for a lost device.
    assert_eq!(renderer.gpu_lost(), None);

    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer
        .execute_graph(&graph)
        .expect("the next frame renders");
    assert_eq!(read_rgba8(&renderer, &output), expected);
    assert!(pollster::block_on(scope.pop()).is_none());
    assert_eq!(renderer.result_cache_stats().entries, 1);
    assert!(renderer.texture_pool_stats().created > pool.created);
}

#[test]
fn a_rejected_readback_frame_fails_alone_and_the_next_one_reads_back() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let texture = rgba_texture(&renderer, [40, 160, 220, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let graph = cached_graph(&view);

    INVALID_TEXTURES.set(1);
    let failed = renderer.render_graph_yuv420p(&graph, &[], centred(ColorMetadata::default()));
    assert_eq!(INVALID_TEXTURES.get(), 0);
    // The frame's own error, not a lost device and not a missing canary.
    assert_eq!(failed.err(), Some(REJECTED));
    assert_eq!(renderer.gpu_lost(), None);

    let planes = renderer
        .render_graph_yuv420p(&graph, &[], centred(ColorMetadata::default()))
        .expect("the next frame submits")
        .wait()
        .expect("the next frame carries its canary");
    assert_eq!(planes.y.len(), 8 * 4);
    assert_eq!(renderer.gpu_lost(), None);
}

#[test]
fn a_rejected_frame_for_an_encoder_fails_alone_and_tracks_no_canary() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let texture = rgba_texture(&renderer, [40, 160, 220, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let graph = cached_graph(&view);
    let luma = nv12_test_plane(
        &renderer.gpu,
        "luma",
        RenderSize::new(8, 4).expect("size"),
        wgpu::TextureFormat::R8Unorm,
    );
    let chroma = nv12_test_plane(
        &renderer.gpu,
        "chroma",
        RenderSize::new(4, 2).expect("size"),
        wgpu::TextureFormat::Rg8Unorm,
    );
    let pack = |renderer: &Renderer| {
        renderer.render_graph_nv12_timed(
            &graph,
            &[],
            crate::Nv12PlaneTargets {
                luma: &luma,
                chroma: &chroma,
            },
            centred(ColorMetadata::default()),
            &mut crate::FrameSubmitTimings::default(),
        )
    };

    INVALID_TEXTURES.set(1);
    assert_eq!(pack(&renderer).err(), Some(REJECTED));
    assert_eq!(renderer.submission_canaries.outstanding(), 0);
    pack(&renderer).expect("the next frame packs");
    renderer
        .confirm_gpu_work()
        .expect("the GPU ran the next frame, and the rejected one is not held against it");
    assert_eq!(renderer.gpu_lost(), None);
}

#[test]
fn an_error_the_browser_reports_later_drops_what_the_renderer_kept() {
    // A browser's error scopes resolve after the frame returned, so its embedding reports
    // the uncaptured error instead.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let graph = cached_graph(&view);
    let first = renderer.execute_graph(&graph).expect("renders");
    let expected = read_rgba8(&renderer, &first);
    drop(first);
    // An idle texture of the cached result's size, which a readback leaves only where it
    // packs half floats.
    drop(
        renderer
            .render_solid(
                RenderSize::new(8, 4).expect("size"),
                Color::rgba(0.0, 0.0, 0.0, 1.0),
            )
            .expect("solid target"),
    );
    assert_eq!(renderer.result_cache_stats().entries, 1);
    let pooled = renderer.texture_pool_stats();
    assert!(pooled.available > 0, "{pooled:?}");

    renderer
        .gpu_error_reports()
        .report(GpuErrorKind::OutOfMemory);
    let again = renderer.execute_graph(&graph).expect("renders");
    assert_eq!(read_rgba8(&renderer, &again), expected);
    let cache = renderer.result_cache_stats();
    assert_eq!(cache.invalidations[4], 1, "{cache:?}");
    assert_eq!(cache.hits, 0, "drawn again, not found");
    // Nothing pooled before the report was handed out again.
    assert!(renderer.texture_pool_stats().created > pooled.created);
}

#[test]
fn a_lost_device_fails_the_frame_without_a_panic_and_the_log_says_so() {
    // Its own device, since it is destroyed; it holds the lock because a second instance loads
    // and unloads the Vulkan layers and drivers.
    let _lock = gpu_test_lock();
    let gpu = pollster::block_on(GpuContext::request_product()).expect("a GPU adapter");
    let log = Arc::new(GpuDiagnosticLog::default());
    let recorded = Arc::clone(&log);
    gpu.set_diagnostic_handler(move |diagnostic: GpuDiagnostic| recorded.push(diagnostic));
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(8, 4).expect("size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits")
        .wait()
        .expect("the device runs work");

    renderer.gpu.device().destroy();
    let refused = renderer.begin_yuv420p_readback(&target, centred(ColorMetadata::default()));
    assert_eq!(refused.err(), Some(RenderError::GpuDeviceLost));
    assert_eq!(renderer.gpu_lost(), Some(RenderError::GpuDeviceLost));
    assert_eq!(
        renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .err(),
        Some(RenderError::GpuDeviceLost)
    );

    // The loss reaches the diagnostic handler once the device is polled.
    let _ = renderer
        .gpu
        .device()
        .poll(wgpu::PollType::wait_indefinitely());
    let snapshot = log.snapshot();
    assert!(snapshot.device_lost, "{snapshot:?}");
    assert!(
        snapshot.entries.iter().any(|entry| matches!(
            entry,
            GpuDiagnostic::DeviceLost { reason, .. } if reason == "Destroyed"
        )),
        "{snapshot:?}"
    );
    assert_eq!(snapshot.failure_code(), Some("DEVICE_LOST"));
}
