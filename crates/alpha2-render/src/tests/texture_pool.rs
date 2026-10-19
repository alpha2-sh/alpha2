//! The texture pool: eviction to its budget, reuse across sizes, and releasing idle memory.

use super::*;

#[test]
fn texture_pool_evicts_idle_targets_to_its_budget() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let input = rgba_texture(&renderer, [255; 4]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let layer = CompositeLayer {
        texture: &view,
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    };
    let _output = renderer
        .render_layers(
            RenderSize::new(2, 2).expect("non-zero size"),
            &[layer, layer],
        )
        .expect("layer renders");
    assert!(renderer.texture_pool_stats().available_bytes > 0);

    renderer.set_texture_budget_bytes(0);
    let stats = renderer.texture_pool_stats();
    assert_eq!(stats.budget_bytes, 0);
    assert_eq!(stats.available, 0);
    assert_eq!(stats.available_bytes, 0);
    assert_eq!(stats.evicted, 1);
}

/// Renders `count` solid targets of `size` at once, then releases them all:
/// one frame whose working set is `count` textures.
fn solid_frame(renderer: &Renderer, size: RenderSize, count: usize) {
    let held = (0..count)
        .map(|_| {
            renderer
                .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
                .expect("solid target renders")
        })
        .collect::<Vec<_>>();
    drop(held);
}

#[test]
fn a_stale_descriptor_leaves_the_idle_pool_for_the_current_size() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    solid_frame(&renderer, RenderSize::new(64, 64).expect("size"), 4);
    renderer.set_texture_budget_bytes(renderer.texture_pool_stats().available_bytes);
    let before = renderer.texture_pool_stats();
    for _ in 0..100 {
        solid_frame(&renderer, RenderSize::new(32, 32).expect("size"), 1);
    }
    let after = renderer.texture_pool_stats();
    // One frame of hysteresis: the stale textures are protected once.
    assert!(
        after.created - before.created <= 2,
        "created {}",
        after.created - before.created
    );
    assert!(after.reused - before.reused >= 98);
}

#[test]
fn a_size_change_settles_into_reuse_within_the_budget() {
    // A preview size, then an export at twice the size whose working set fits the budget
    // only once the preview textures are gone.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let preview = RenderSize::new(32, 18).expect("size");
    let export = RenderSize::new(64, 36).expect("size");
    for _ in 0..3 {
        solid_frame(&renderer, preview, 3);
    }
    let preview_bytes = renderer.texture_pool_stats().available_bytes;
    renderer.set_texture_budget_bytes(preview_bytes * 4);
    for _ in 0..3 {
        solid_frame(&renderer, export, 3);
    }
    let settled = renderer.texture_pool_stats();
    for _ in 0..20 {
        solid_frame(&renderer, export, 3);
    }
    let after = renderer.texture_pool_stats();
    assert_eq!(
        after.created, settled.created,
        "export frames after warm-up"
    );
    assert!(after.available_bytes <= after.budget_bytes);
}

#[test]
fn alternating_sizes_over_budget_keep_one_of_them_instead_of_thrashing() {
    // Two sizes alternating every frame with room for one: evicting the older on every
    // release would create a texture on every frame.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let first = RenderSize::new(16, 16).expect("size");
    let second = RenderSize::new(16, 8).expect("size");
    solid_frame(&renderer, first, 1);
    renderer.set_texture_budget_bytes(renderer.texture_pool_stats().available_bytes);
    let before = renderer.texture_pool_stats();
    for _ in 0..10 {
        solid_frame(&renderer, first, 1);
        solid_frame(&renderer, second, 1);
    }
    let after = renderer.texture_pool_stats();
    assert!(
        after.created - before.created <= 11,
        "created {}",
        after.created - before.created
    );
    assert!(after.reused - before.reused >= 9);
}

#[test]
fn texture_pool_tracks_shared_render_results_once() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 2).expect("non-zero size");
    let texture = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target renders");
    let clone = texture.clone();
    let active = renderer.texture_pool_stats();
    assert_eq!(active.active, 1);
    assert_eq!(active.active_bytes, active.high_water_bytes);
    assert!(active.active_bytes > 0);

    drop(texture);
    assert_eq!(renderer.texture_pool_stats().active, 1);
    drop(clone);
    let idle = renderer.texture_pool_stats();
    assert_eq!(idle.active, 0);
    assert_eq!(idle.active_bytes, 0);
    assert_eq!(idle.available, 1);
}

#[test]
fn texture_pool_evicts_the_least_recently_released_descriptor() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let small = RenderSize::new(1, 1).expect("small size");
    let current = RenderSize::new(2, 1).expect("current size");
    drop(renderer.textures.acquire(
        renderer.gpu.device(),
        small,
        wgpu::TextureFormat::Rgba8Unorm,
        "old descriptor",
    ));
    drop(renderer.textures.acquire(
        renderer.gpu.device(),
        current,
        wgpu::TextureFormat::Rgba8Unorm,
        "current descriptor",
    ));

    renderer.set_texture_budget_bytes(8);
    let after_eviction = renderer.texture_pool_stats();
    assert_eq!(after_eviction.available, 1);
    assert_eq!(after_eviction.available_bytes, 8);
    assert_eq!(after_eviction.evicted, 1);

    let current_lease = renderer.textures.acquire(
        renderer.gpu.device(),
        current,
        wgpu::TextureFormat::Rgba8Unorm,
        "current descriptor",
    );
    let old_lease = renderer.textures.acquire(
        renderer.gpu.device(),
        small,
        wgpu::TextureFormat::Rgba8Unorm,
        "old descriptor",
    );
    let active = renderer.texture_pool_stats();
    assert_eq!(active.created, 3);
    assert_eq!(active.reused, 1);
    assert_eq!(active.active, 2);
    assert_eq!(active.active_bytes, 12);
    assert_eq!(active.high_water_bytes, 12);
    drop((current_lease, old_lease));
}

#[test]
fn releasing_idle_memory_keeps_what_is_held_and_drops_what_is_pooled() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let held = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target renders");
    let pooled = renderer
        .render_solid(size, Color::rgba(0.5, 0.5, 0.5, 1.0))
        .expect("second target renders");
    let planes = renderer
        .begin_yuv420p_readback(&pooled, centred(ColorMetadata::default()))
        .expect("readback submits")
        .wait()
        .expect("readback maps");
    drop(pooled);
    let before = renderer.texture_pool_stats();
    assert!(before.available_bytes > 0, "a released target is pooled");
    let readback = renderer.diagnostics().readback_buffers;
    assert!(
        readback.available_bytes > 0,
        "a finished readback buffer is pooled"
    );

    let released = renderer.release_idle_memory();
    assert!(released >= before.available_bytes + readback.available_bytes);
    let after = renderer.texture_pool_stats();
    assert_eq!(after.available_bytes, 0);
    assert_eq!(after.active_bytes, before.active_bytes, "held targets stay");
    assert_eq!(renderer.diagnostics().readback_buffers.available_bytes, 0);

    // What is still held renders on, and the pools refill as work returns.
    let again = renderer
        .begin_yuv420p_readback(&held, centred(ColorMetadata::default()))
        .expect("readback after release submits")
        .wait()
        .expect("readback after release maps");
    assert_eq!(again.y.len(), planes.y.len());
    assert!(renderer.release_idle_memory() > 0);
}

#[test]
fn an_upload_texture_larger_than_the_idle_budget_is_dropped_without_evicting_the_others() {
    // Releasing an upload the budget can never hold must not first evict every idle upload: a
    // 6K EXR frame each frame would make every other source re-create its plane textures.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let pool = &renderer.upload_textures;
    // RGBA8 rows of 8 pixels: 32 bytes a row.
    let rows = |height| RenderSize::new(8, height).expect("size");
    pool.set_budget_bytes(1024);
    for height in [2, 3] {
        drop(
            renderer
                .acquire_rgba8_upload_texture(rows(height))
                .expect("upload"),
        );
    }
    let idle = pool.stats();
    assert_eq!((idle.available, idle.available_bytes), (2, 64 + 96));
    // 2 KiB, over the 1 KiB budget whatever else is idle.
    drop(
        renderer
            .acquire_rgba8_upload_texture(rows(64))
            .expect("oversized upload"),
    );
    let after = pool.stats();
    assert_eq!(
        (after.available, after.available_bytes),
        (2, 64 + 96),
        "the idle uploads survive an upload the budget cannot hold"
    );
    assert_eq!(
        after.discarded,
        idle.discarded + 1,
        "the oversized one is dropped"
    );
    let reused = pool.stats().reused;
    drop(
        renderer
            .acquire_rgba8_upload_texture(rows(2))
            .expect("reuse"),
    );
    assert_eq!(pool.stats().reused, reused + 1);

    // Within the budget, the oldest idle uploads make room, first in first out.
    pool.set_budget_bytes(64 + 96 + 128);
    drop(
        renderer
            .acquire_rgba8_upload_texture(rows(4))
            .expect("upload"),
    );
    assert_eq!(pool.stats().available, 3, "128 more bytes fit exactly");
    drop(
        renderer
            .acquire_rgba8_upload_texture(rows(5))
            .expect("upload"),
    );
    let evicted = pool.stats();
    // 160 more bytes: the two oldest (96, then 64 after the reuse above put it back last)
    // leave.
    assert!(evicted.available_bytes <= 64 + 96 + 128, "{evicted:?}");
    assert_eq!(evicted.available, 2, "{evicted:?}");
    let reused = evicted.reused;
    drop(
        renderer
            .acquire_rgba8_upload_texture(rows(4))
            .expect("kept"),
    );
    drop(
        renderer
            .acquire_rgba8_upload_texture(rows(5))
            .expect("kept"),
    );
    assert_eq!(pool.stats().reused, reused + 2, "the newest two were kept");
}
