//! Readbacks: deferred and blocking reads, buffer reuse, and the step a failed readback names.

use super::*;

#[test]
fn a_deferred_readback_matches_the_blocking_one_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    for gpu in gpus_by_intermediate() {
        deferred_readback_on(gpu);
    }
}

fn deferred_readback_on(gpu: GpuContext) {
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(8, 4).expect("non-zero size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("valid clear pass");
    let expected = read_rgba8(&renderer, &target);

    // A caller that cannot block (a browser) submits, maps without blocking, then packs; it must
    // match the blocking helper.
    let readback = renderer
        .begin_rgba8_readback(&target)
        .expect("readback submits");
    assert_eq!(readback.size(), size);
    map_without_blocking(|ready| readback.map_async(ready), || readback.poll());
    assert_eq!(readback.finish().expect("mapped pixels"), expected);

    // The browser export's sequence: a YUV readback mapped without blocking, planes read in
    // place at `plane_layout`; it must equal the blocking readback's planes.
    let blocking = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits")
        .wait()
        .expect("blocking planes");
    let exported = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits");
    map_without_blocking(|ready| exported.map_async(ready), || exported.poll());
    assert_eq!(
        i420_by_plane_layout(exported),
        [blocking.y, blocking.u, blocking.v].concat()
    );
}

/// Requests a mapping through `map` and polls until it reports, as a caller
/// that may not block does, and requires it to succeed.
fn map_without_blocking(
    map: impl FnOnce(Box<dyn FnOnce(Result<(), RenderError>) + Send>),
    poll: impl Fn(),
) {
    let mapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mapped_flag = std::sync::Arc::clone(&mapped);
    let done_flag = std::sync::Arc::clone(&done);
    map(Box::new(move |mapped| {
        mapped_flag.store(mapped.is_ok(), std::sync::atomic::Ordering::Release);
        done_flag.store(true, std::sync::atomic::Ordering::Release);
    }));
    // Bounded by time, not a poll count: a thousand non-blocking polls were over before
    // lavapipe had finished the copy.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done.load(std::sync::atomic::Ordering::Acquire) && std::time::Instant::now() < deadline {
        poll();
        std::thread::yield_now();
    }
    assert!(
        done.load(std::sync::atomic::Ordering::Acquire),
        "mapping reported readiness"
    );
    assert!(mapped.load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn readback_buffers_reuse_only_after_successful_finish() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("non-zero size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("valid clear pass");

    let abandoned = renderer
        .begin_rgba8_readback(&target)
        .expect("abandoned readback submits");
    assert_eq!(renderer.readback_buffers.stats().created, 1);
    drop(abandoned);
    let abandoned_stats = renderer.readback_buffers.stats();
    assert_eq!(abandoned_stats.available, 0);
    assert_eq!(abandoned_stats.discarded, 1);

    let completed = renderer
        .begin_rgba8_readback(&target)
        .expect("completed readback submits");
    assert_eq!(renderer.readback_buffers.stats().created, 2);
    let expected = completed.wait().expect("completed readback maps");
    assert_eq!(renderer.readback_buffers.stats().available, 1);

    let reused = renderer
        .begin_rgba8_readback(&target)
        .expect("reused readback submits");
    let in_flight = renderer.readback_buffers.stats();
    assert_eq!(in_flight.created, 2);
    assert_eq!(in_flight.reused, 1);
    assert_eq!(in_flight.available, 0);
    assert_eq!(reused.wait().expect("reused readback maps"), expected);
    assert_eq!(renderer.readback_buffers.stats().available, 1);
}

#[test]
fn readback_buffer_pool_retains_at_most_three_idle_slots() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    for size in [256, 512, 768, 1024] {
        renderer
            .readback_buffers
            .acquire(renderer.gpu.device(), size, "alpha2 test readback buffer")
            .recycle();
    }
    let stats = renderer.readback_buffers.stats();
    assert_eq!(stats.created, 4);
    assert_eq!(stats.reused, 0);
    assert_eq!(stats.discarded, 1);
    assert_eq!(stats.available, 3);
    assert_eq!(stats.available_bytes, 512 + 768 + 1024);
}

#[test]
fn yuv_pack_targets_reuse_before_earlier_buffers_finish_mapping() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target renders");
    let target_bytes = renderer.texture_pool_stats().active_bytes;

    let first = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("first YUV readback submits");
    let (mapping_sender, mapping_receiver) = std::sync::mpsc::channel();
    first.map_async(move |ready| {
        let _ = mapping_sender.send(ready);
    });
    // Four 256-byte luma rows, then two rows each for U and V in their own placement-aligned
    // regions, then the 16-byte canary.
    assert_eq!(first.staging_bytes(), 256 * 4 + 2 * (256 * 2) + 16);
    let after_first = renderer.texture_pool_stats();
    assert_eq!(after_first.active, 1);
    assert_eq!(after_first.active_bytes, target_bytes);
    assert_eq!(after_first.available, 3);

    let second = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("second YUV readback submits");
    let after_second = renderer.texture_pool_stats();
    assert_eq!(after_second.created, after_first.created);
    assert_eq!(after_second.reused, after_first.reused + 3);
    assert_eq!(after_second.active, 1);
    assert_eq!(after_second.active_bytes, target_bytes);
    assert_eq!(after_second.available, 3);

    first
        .wait_for_submission()
        .expect("first YUV submission completes");
    assert_eq!(
        mapping_receiver.recv().expect("mapping callback runs"),
        Ok(())
    );
    let first_planes = first.finish().expect("first YUV readback packs");
    let second_planes = second.wait().expect("second YUV readback maps");
    assert_eq!(second_planes, first_planes);
}

#[test]
fn a_readback_failure_names_the_step_that_failed() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target renders");

    let unmapped = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits");
    unmapped
        .wait_for_submission()
        .expect("submission completes");
    // A readback finished before it was mapped is refused without unmapping: unmapping an
    // unmapped buffer is a validation error a host would blame a later failure on.
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    assert_eq!(
        unmapped.finish(),
        Err(crate::RenderError::TextureReadback(
            crate::ReadbackFailure::NotMapped
        ))
    );
    let into = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits");
    into.wait_for_submission().expect("submission completes");
    let (mut y, mut u, mut v) = (vec![0; 32], vec![0; 8], vec![0; 8]);
    assert_eq!(
        into.finish_into(&mut Yuv420pTargets {
            y: PlaneTarget::new(&mut y, 8),
            u: PlaneTarget::new(&mut u, 4),
            v: PlaneTarget::new(&mut v, 4),
        }),
        Err(crate::RenderError::TextureReadback(
            crate::ReadbackFailure::NotMapped
        ))
    );
    let mapped = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits");
    mapped.wait_for_submission().expect("submission completes");
    assert_eq!(
        mapped.finish_mapped(|_| ()),
        Err(crate::RenderError::TextureReadback(
            crate::ReadbackFailure::NotMapped
        ))
    );
    let rgba = renderer
        .begin_rgba8_readback(&target)
        .expect("readback submits");
    assert_eq!(
        rgba.finish(),
        Err(crate::RenderError::TextureReadback(
            crate::ReadbackFailure::NotMapped
        ))
    );
    let error = pollster::block_on(scope.pop()).map(|error| error.to_string());
    assert_eq!(
        error, None,
        "refusing an unmapped readback raised a GPU error"
    );

    let twice = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits");
    let (sender, receiver) = std::sync::mpsc::channel();
    twice.map_async(|_| {});
    twice.map_async(move |mapped| {
        let _ = sender.send(mapped);
    });
    assert_eq!(
        receiver.recv().expect("the second request reports at once"),
        Err(crate::RenderError::TextureReadback(
            crate::ReadbackFailure::DuplicateMapRequest
        ))
    );
    twice.wait_for_submission().expect("submission completes");
    assert_eq!(
        crate::RenderError::TextureReadback(crate::ReadbackFailure::MapFailed).to_string(),
        "GPU texture readback failed: map-failed"
    );
}
