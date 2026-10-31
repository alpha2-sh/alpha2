//! Stalled GPU work: readbacks time out instead of hanging, and dropped submissions stop the
//! renderer.

use super::*;

/// Occupies the shared queue with short draws, simulating a hung GPU without hanging it.
///
/// Each draw is one fragment with a uniform-driven nonlinear loop, so it cannot be folded or
/// unrolled (WebGL2 limits rule out compute). Each draw is its own command buffer so none runs
/// long: macOS treats a long command buffer as a hang, and after two GPU restarts ignores the
/// process's submissions, which then read back as zeros.
struct BusyLoop {
    gpu: GpuContext,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    count: wgpu::Buffer,
    view: wgpu::TextureView,
}

impl BusyLoop {
    #[allow(clippy::too_many_lines)]
    fn new(gpu: &GpuContext) -> Self {
        let module = gpu
            .device()
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("busy loop"),
                source: wgpu::ShaderSource::Wgsl(
                    "@group(0) @binding(0) var<uniform> count: vec4u;
                     @vertex fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
                         let x = f32(i32(index & 1u) * 4 - 1);
                         let y = f32(i32(index >> 1u) * 4 - 1);
                         return vec4f(x, y, 0.0, 1.0);
                     }
                     @fragment fn fs() -> @location(0) vec4f {
                         var value = count.y;
                         for (var i = 0u; i < count.x; i++) {
                             value = (value ^ (value >> 13u)) * 0x5bd1e995u + i;
                         }
                         return vec4f(f32(value & 255u) / 255.0, 0.0, 0.0, 1.0);
                     }"
                    .into(),
                ),
            });
        let pipeline = gpu
            .device()
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("busy loop"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let count = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("busy loop count"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = gpu.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("busy loop"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: count.as_entire_binding(),
            }],
        });
        let target = gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("busy loop target"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            gpu: gpu.clone(),
            pipeline,
            bind_group,
            count,
            view,
        }
    }

    fn submit(&self, draws: u32, iterations: u32) {
        self.gpu.queue().write_buffer(
            &self.count,
            0,
            &[iterations, 1, 0, 0].map(u32::to_ne_bytes).concat(),
        );
        for _ in 0..draws {
            let mut encoder = self
                .gpu
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("busy loop"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            self.gpu.submit([encoder.finish()]);
        }
    }
}

/// Why a readback behind `stall` cannot be seen to time out, or `None` if enough stall remains
/// pending at the wait.
///
/// At most the stall minus `stalling` (time to submit it) and `submitting` (the readback's
/// submit) can still be queued; drivers that run work inside `submit` spend it there. Under a
/// quarter of the stall left is within calibration error, so the test cannot distinguish a
/// timeout.
fn stall_drained_before_the_wait(
    stall: std::time::Duration,
    stalling: std::time::Duration,
    submitting: std::time::Duration,
    queue_empty: bool,
) -> Option<&'static str> {
    let left = stall.saturating_sub(stalling).saturating_sub(submitting);
    if queue_empty {
        Some("the stalled work drained before the readback was waited on")
    } else if left < stall / 4 {
        Some("the stalled work ran while it and the readback were submitted")
    } else {
        None
    }
}

#[test]
fn a_stall_the_driver_drains_while_submitting_is_not_waited_on() {
    let ms = std::time::Duration::from_millis;
    let stall = ms(100);
    // lavapipe runs the stalled draws inside their submits (110-310 ms) and the readback's submit
    // waits for the last (1-4 ms); the queue is not yet empty.
    assert!(stall_drained_before_the_wait(stall, ms(110), ms(4), false).is_some());
    assert!(stall_drained_before_the_wait(stall, ms(90), ms(1), false).is_some());
    assert!(stall_drained_before_the_wait(stall, ms(1), ms(80), false).is_some());
    assert!(stall_drained_before_the_wait(stall, ms(1), ms(1), true).is_some());
    // A GPU that returns from submit at once, or one slow to submit, still has the stall pending,
    // so the readback must time out.
    assert_eq!(
        stall_drained_before_the_wait(stall, ms(2), ms(1), false),
        None
    );
    assert_eq!(
        stall_drained_before_the_wait(stall, ms(55), ms(2), false),
        None
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_readback_behind_stalled_gpu_work_times_out_instead_of_hanging() {
    let _lock = gpu_test_lock();
    // A separate device does not help: macOS stops the whole process after its second GPU restart.
    let context = pollster::block_on(GpuContext::request_product())
        .expect("render tests require a working GPU adapter");
    let mut renderer = Renderer::new(context.clone());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    // Warm readback pipelines so setup cannot outlast the stall.
    let before = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits")
        .wait()
        .expect("readback completes");

    // About 100 ms of short draws: far past the 1 ms timeout, but no draw long enough to count as a
    // hang. Two timed runs cancel the fixed submit cost.
    let (draw_seconds, stall_seconds, timed_draws) = (0.0015, 0.1, 8);
    let busy = BusyLoop::new(&context);
    let run_once = |draws: u32, iterations: u32| {
        let timed = std::time::Instant::now();
        busy.submit(draws, iterations);
        context
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the calibration run finishes");
        timed.elapsed().as_secs_f64()
    };
    // Fastest of three runs, since load only adds time.
    let run = |iterations: u32| {
        (0..3)
            .map(|_| run_once(timed_draws, iterations))
            .fold(f64::INFINITY, f64::min)
    };
    let (short, long) = (run(25_000), run(125_000));
    let timed_iterations = f64::from(timed_draws) * 100_000.0;
    // On some software rasterizers extra iterations cost nothing measurable; such a device cannot
    // be stalled this way.
    if long - short < 0.005 {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::MeasurableGpuStall,
            "the busy loop takes no measurable time on this device ({:.2} ms for {timed_iterations} iterations)",
            (long - short) * 1e3
        );
        return;
    }
    let per_iteration = (long - short) / timed_iterations;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to u32.
    let iterations = (draw_seconds / per_iteration).clamp(1e3, 1e7) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // At most 67.
    let draws = (stall_seconds / draw_seconds).ceil() as u32;

    renderer.set_gpu_wait_timeout(std::time::Duration::from_millis(1));
    // Timed from before submission, since lavapipe runs draws inside `submit`.
    let started = std::time::Instant::now();
    busy.submit(draws, iterations);
    let stalling = started.elapsed();
    let submitted = std::time::Instant::now();
    let pending = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits");
    let submitting = submitted.elapsed();
    let queue_empty = context
        .device()
        .poll(wgpu::PollType::Poll)
        .expect("the device polls")
        .is_queue_empty();
    if let Some(reason) = stall_drained_before_the_wait(
        std::time::Duration::from_secs_f64(stall_seconds),
        stalling,
        submitting,
        queue_empty,
    ) {
        renderer.set_gpu_wait_timeout(crate::DEFAULT_GPU_WAIT_TIMEOUT);
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::MeasurableGpuStall,
            "{reason} (stall submitted in {stalling:?}, readback in {submitting:?}, queue empty: \
             {queue_empty})"
        );
        return;
    }
    let error = match pending.wait() {
        Err(error) => error,
        Ok(planes) => {
            // Timed alone afterwards, the same draws distinguish a broken wait from a misled
            // calibration.
            renderer.set_gpu_wait_timeout(crate::DEFAULT_GPU_WAIT_TIMEOUT);
            let stall = std::time::Duration::from_secs_f64(run_once(draws, iterations));
            if stall < std::time::Duration::from_secs_f64(stall_seconds / 4.0) {
                alpha2_test_support::skip!(
                    alpha2_test_support::Capability::MeasurableGpuStall,
                    "the calibrated stall takes {stall:?} alone, not {stall_seconds} s: the busy \
                     loop's cost was the machine's load"
                );
                return;
            }
            panic!(
                "the readback finished behind the stalled work within 1 ms \
                 ({draws} draws of {iterations} iterations, {per_iteration:e} s per iteration, \
                 {stall:?} alone, submit took {submitting:?}): {planes:?}"
            )
        }
    };
    assert_eq!(error, RenderError::GpuTimeout);
    assert_eq!(error.to_string(), crate::GPU_TIMEOUT_MESSAGE);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        renderer.reclaim_external_leases(),
        Err(RenderError::GpuTimeout)
    );

    context
        .device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the stalled work finishes");
    renderer.set_gpu_wait_timeout(crate::DEFAULT_GPU_WAIT_TIMEOUT);
    let planes = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits")
        .wait()
        .expect("readback completes");
    // Compare values: a device that dies behind the stall reads back zeros.
    assert_eq!(
        planes, before,
        "the device reads back differently after the stall"
    );
}

#[test]
fn a_readback_the_gpu_did_not_run_stops_the_renderer() {
    // A process that hung the GPU twice on macOS has submissions ignored yet completed, so a
    // readback maps on time over unwritten bytes.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    for _ in 0..3 {
        renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("readback submits")
            .wait()
            .expect("a readback the GPU ran carries its canary");
    }
    assert_eq!(renderer.gpu_lost(), None);

    let dropped = renderer.canaries.issue();
    assert_eq!(dropped.check(&[0; 16]), Err(RenderError::GpuWorkDropped));
    assert_eq!(renderer.gpu_lost(), Some(RenderError::GpuWorkDropped));
    assert_eq!(
        renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .err(),
        Some(RenderError::GpuWorkDropped)
    );
    assert!(
        RenderError::GpuWorkDropped
            .to_string()
            .contains("GPU device lost"),
        "its message says what happened"
    );
}

#[test]
fn an_rgba8_readback_the_gpu_did_not_run_fails_and_stops_the_renderer() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.6, 0.75, 1.0))
        .expect("solid target");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    for _ in 0..3 {
        let issued = renderer.canaries.issued();
        let readback = renderer
            .begin_rgba8_readback(&target)
            .expect("readback submits");
        assert_eq!(renderer.canaries.issued() - issued, 1, "one canary each");
        assert_eq!(readback.staging_bytes(), 256 * 4 + 16);
        assert_eq!(
            readback
                .wait()
                .expect("a readback the GPU ran carries its canary"),
            [64, 153, 191, 255].repeat(32)
        );
    }
    assert_eq!(renderer.gpu_lost(), None);
    let pooled = renderer.readback_buffers.stats();

    // A dropped submission leaves a staging buffer that maps on time, ending in another readback's
    // canary.
    let readback = renderer
        .begin_rgba8_readback(&target)
        .expect("readback submits")
        .expecting(renderer.canaries.issue());
    assert_eq!(readback.wait(), Err(RenderError::GpuWorkDropped));
    assert_eq!(renderer.gpu_lost(), Some(RenderError::GpuWorkDropped));
    assert_eq!(
        renderer.readback_buffers.stats().available,
        pooled.available - 1,
        "a buffer holding no frame of its readback is not pooled again"
    );
    assert_eq!(
        renderer.begin_rgba8_readback(&target).err(),
        Some(RenderError::GpuWorkDropped),
        "the renderer refuses readbacks once the device drops work"
    );
    assert_eq!(
        renderer.read_rgba8(&target),
        Err(RenderError::GpuWorkDropped)
    );
    assert!(pollster::block_on(scope.pop()).is_none());
}
