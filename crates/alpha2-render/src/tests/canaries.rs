//! Canaries: the submissions that tell finished GPU work from a stalled one.

use super::*;

/// Submits one canary of its own, as an NV12 pack does, and tracks it.
fn submit_tracked_canary(renderer: &Renderer) {
    let mut encoder = renderer.command_encoder("canary test encoder");
    let (buffer, canary) = renderer
        .canaries
        .encode_own(&renderer.gpu, &mut encoder)
        .expect("a canary");
    renderer.submit([encoder.finish()]).expect("submitted");
    renderer
        .submission_canaries
        .track(&renderer.gpu, buffer, canary);
}

#[test]
fn a_canary_that_reports_on_another_thread_is_waited_for_not_read_as_a_timeout() {
    // A map callback runs on the polling thread: on macOS the VideoToolbox worker polls the
    // renderer's device, so the last canary report may still be in transit when the wait returns.
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let set_delay = |renderer: &Renderer, delay: Option<std::time::Duration>| {
        *renderer
            .submission_canaries
            .report_elsewhere_after
            .lock()
            .expect("hook") = delay;
    };
    set_delay(&renderer, Some(std::time::Duration::from_millis(200)));
    for _ in 0..2 {
        submit_tracked_canary(&renderer);
    }
    assert_eq!(renderer.confirm_gpu_work(), Ok(()));
    assert_eq!(renderer.submission_canaries.outstanding(), 0);
    assert_eq!(renderer.gpu_lost(), None);

    // A report that does not come within the wait timeout is still the GPU timing out, and
    // the wait ends on time.
    renderer.set_gpu_wait_timeout(std::time::Duration::from_millis(100));
    set_delay(&renderer, Some(std::time::Duration::from_secs(2)));
    submit_tracked_canary(&renderer);
    let started = std::time::Instant::now();
    assert_eq!(renderer.confirm_gpu_work(), Err(RenderError::GpuTimeout));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    renderer.set_gpu_wait_timeout(crate::DEFAULT_GPU_WAIT_TIMEOUT);
    assert_eq!(renderer.confirm_gpu_work(), Ok(()));
    set_delay(&renderer, None);

    // Another thread polling the device the whole time, as the encode worker does, collects
    // some of the callbacks itself.
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let device = renderer.gpu.device().clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let _ = device.poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(std::time::Duration::from_millis(10)),
                });
            }
        })
    };
    for _ in 0..50 {
        submit_tracked_canary(&renderer);
        assert_eq!(renderer.confirm_gpu_work(), Ok(()));
    }
    stop.store(true, Ordering::Release);
    poller.join().expect("the poller ends");
    assert_eq!(renderer.submission_canaries.outstanding(), 0);
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_lease_parked_on_another_thread_is_reclaimed_by_the_reclaim_that_waited_for_it() {
    // A lease is parked by a queue callback on whichever thread polled; a reclaim that drained
    // right after its own wait would miss one parked late.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    *renderer
        .retired_leases
        .park_elsewhere_after
        .lock()
        .expect("hook") = Some(std::time::Duration::from_millis(200));
    let (y, uv) = nv12_test_planes(&renderer, 2, 2, &[16; 4], &[128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(2, 2).expect("size");
    let submitted = Arc::new(AtomicBool::new(false));
    let lease = Arc::new(TestExternalLease::new(&submitted));
    let weak = Arc::downgrade(&lease);
    let (prepared, _) = renderer
        .prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::NV12,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
            lease,
        )
        .expect("NV12 source prepares");
    let (_texture, pending) = prepared.into_parts();
    assert!(renderer.submit_pending_source(&pending.expect("conversion pending")));
    assert!(submitted.load(Ordering::Acquire));
    assert_eq!(renderer.reclaim_external_leases(), Ok(1));
    assert!(weak.upgrade().is_none(), "the lease outlived its reclaim");
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn work_that_reads_nothing_back_is_checked_by_its_own_canary() {
    // An NV12 pack goes to a hardware encoder, not a readback, so only its own canary catches a
    // device that completes it without running it.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    for _ in 0..3 {
        let mut encoder = renderer.command_encoder("canary test encoder");
        let (buffer, canary) = renderer
            .canaries
            .encode_own(&renderer.gpu, &mut encoder)
            .expect("a canary");
        renderer.submit([encoder.finish()]).expect("submitted");
        renderer
            .submission_canaries
            .track(&renderer.gpu, buffer, canary);
    }
    renderer
        .confirm_gpu_work()
        .expect("the GPU ran what was submitted");
    assert_eq!(renderer.submission_canaries.outstanding(), 0);

    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    let luma = nv12_test_plane(&renderer.gpu, "luma", size, wgpu::TextureFormat::R8Unorm);
    let chroma = nv12_test_plane(
        &renderer.gpu,
        "chroma",
        RenderSize::new(4, 2).expect("chroma size"),
        wgpu::TextureFormat::Rg8Unorm,
    );
    let planes = || crate::Nv12PlaneTargets {
        luma: &luma,
        chroma: &chroma,
    };
    let source = target
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut graph = RenderGraph::new(size);
    let layer = GraphLayer {
        source: graph.add_source(SourceView::from_wgpu(&source)),
        visual: visual([1.0, 1.0], 1.0),
        blend_mode: BlendMode::Normal,
    };
    let output = graph.add_composite(vec![layer]).expect("composite");
    graph.set_output(output).expect("output");
    let issued = renderer.canaries.issued();
    renderer
        .render_graph_nv12_timed(
            &graph,
            &[],
            planes(),
            centred(ColorMetadata::default()),
            &mut crate::FrameSubmitTimings::default(),
        )
        .expect("the frame packs");
    assert_eq!(renderer.canaries.issued() - issued, 1);
    renderer.confirm_gpu_work().expect("the GPU ran the frame");

    // Encoded and never submitted: its buffer maps holding what it held, as one does
    // behind an ignored submission.
    let mut encoder = renderer
        .gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let (buffer, canary) = renderer
        .canaries
        .encode_own(&renderer.gpu, &mut encoder)
        .expect("a canary");
    drop(encoder);
    renderer
        .submission_canaries
        .track(&renderer.gpu, buffer, canary);
    assert_eq!(
        renderer.confirm_gpu_work(),
        Err(RenderError::GpuWorkDropped)
    );
    assert_eq!(renderer.gpu_lost(), Some(RenderError::GpuWorkDropped));
    // Every later frame that feeds an encoder is refused, not only readbacks.
    assert_eq!(
        renderer
            .render_graph_nv12_timed(
                &graph,
                &[],
                planes(),
                centred(ColorMetadata::default()),
                &mut crate::FrameSubmitTimings::default(),
            )
            .err(),
        Some(RenderError::GpuWorkDropped)
    );
}

#[test]
fn canary_buffers_are_pooled_and_never_shared_by_canaries_in_flight() {
    // Browser buffers that are only dropped live until GC, so canaries are pooled; two canaries
    // awaiting submission must not share a source, as `write_buffer` lands at the next submit.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    let expected = read_rgba8(&renderer, &target);
    let sources = || renderer.canaries.sources().created();
    assert_eq!(sources(), 1);

    // One readback at a time: the first's source serves every later one.
    for _ in 0..8 {
        assert_eq!(read_rgba8(&renderer, &target), expected);
        renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("readback submits")
            .wait()
            .expect("the GPU ran it");
    }
    assert_eq!(sources(), 1);

    // Readbacks in flight together each hold a source of their own, and all check out.
    let in_flight = (0..4)
        .map(|_| renderer.begin_rgba8_readback(&target).expect("submits"))
        .collect::<Vec<_>>();
    assert_eq!(sources(), 4);
    for readback in in_flight {
        assert_eq!(readback.wait().expect("the GPU ran it"), expected);
    }
    assert_eq!(renderer.canaries.sources().idle(), 4);

    // Canaries encoded into encoders that are all submitted only afterwards, in reverse:
    // every write lands before any copy reads, and each copy reads its own canary.
    let mut encoders = Vec::new();
    for _ in 0..6 {
        let mut encoder = renderer.command_encoder("canary test encoder");
        let (buffer, canary) = renderer
            .canaries
            .encode_own(&renderer.gpu, &mut encoder)
            .expect("a canary");
        encoders.push((encoder, buffer, canary));
    }
    assert_eq!(sources(), 6);
    for (encoder, buffer, canary) in encoders.into_iter().rev() {
        renderer.submit([encoder.finish()]).expect("submitted");
        renderer
            .submission_canaries
            .track(&renderer.gpu, buffer, canary);
    }
    renderer
        .confirm_gpu_work()
        .expect("the GPU ran what was submitted");
    let submission_buffers = renderer.canaries.submission_buffers().created();
    assert_eq!(submission_buffers, 6);

    for _ in 0..8 {
        submit_tracked_canary(&renderer);
        renderer.confirm_gpu_work().expect("the GPU ran it");
    }
    assert_eq!(
        renderer.canaries.submission_buffers().created(),
        submission_buffers
    );
    assert_eq!(sources(), 6);
    assert_eq!(renderer.gpu_lost(), None);

    // Idle canary buffers are given back with the rest of the idle memory.
    assert!(renderer.release_idle_memory() >= 2 * crate::canary::CANARY_BYTES);
    assert_eq!(renderer.canaries.sources().idle(), 0);
    assert_eq!(renderer.canaries.submission_buffers().idle(), 0);
    assert_eq!(read_rgba8(&renderer, &target), expected);
    assert_eq!(sources(), 7);
}

#[test]
fn canary_buffers_made_before_a_reported_error_are_not_used_again() {
    // The browser reports a rejected submission only later; anything pooled before may be a
    // failed allocation.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    let expected = read_rgba8(&renderer, &target);
    assert_eq!(renderer.canaries.sources().created(), 1);
    renderer
        .gpu_error_reports()
        .report(alpha2_gpu::GpuErrorKind::OutOfMemory);
    // The next frame forgets what was pooled, and its canary has a new source.
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    assert_eq!(read_rgba8(&renderer, &target), expected);
    assert_eq!(renderer.canaries.sources().created(), 2);
    assert_eq!(read_rgba8(&renderer, &target), expected);
    assert_eq!(renderer.canaries.sources().created(), 2);
    assert_eq!(renderer.gpu_lost(), None);
}
