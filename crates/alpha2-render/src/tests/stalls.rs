//! Stalled GPU work: readbacks that time out instead of hanging and renderers a lost submission stops.

use super::*;

/// Occupies the shared queue with short draws, standing in for a hung GPU
/// without hanging it. Each draw is one fragment, so its loop runs serially;
/// the count arrives in a uniform and the body is nonlinear, so the driver's
/// compiler can neither fold nor unroll it. (The suite's device has WebGL2
/// limits, which rule out compute.)
///
/// Every draw is its own command buffer and all of them render to one 1x1
/// target, so they run one after another and none of them runs long: Apple
/// GPUs judge a command buffer that runs for long without finishing a hang
/// and restart the GPU, and after two restarts the kernel ignores the
/// process's submissions for good (`Deny submissions/ignore app[...] with 2
/// GPURestarts`), which reads back as zeros.
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

    /// Submits `draws` command buffers of `iterations` loop turns each.
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

/// Why a readback submitted behind `stall` of queued work cannot be seen to time out, or
/// `None` when enough of the stall can still be pending when the readback is waited on.
///
/// The GPU starts on the stall while it is being submitted, so what can still be queued
/// when the readback is waited on is at most the stall less `stalling`, the time submitting
/// it took, and less `submitting`, the readback's submit. A driver that runs work inside
/// `submit` (lavapipe) spends all of the stall in `stalling`; one that makes a submit wait
/// for the work queued before it spends it in `submitting`. Less than a quarter of the stall
/// left is within the calibration's error of none: the test cannot tell a timeout from a
/// readback that was never behind anything. A queue already empty has nothing left at all.
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
    // lavapipe runs the stalled draws inside their submits (Mesa 25.2: 110-310 ms) and the
    // readback's submit waits for the last one (1-4 ms); the queue is not empty yet.
    assert!(stall_drained_before_the_wait(stall, ms(110), ms(4), false).is_some());
    assert!(stall_drained_before_the_wait(stall, ms(90), ms(1), false).is_some());
    assert!(stall_drained_before_the_wait(stall, ms(1), ms(80), false).is_some());
    assert!(stall_drained_before_the_wait(stall, ms(1), ms(1), true).is_some());
    // A GPU that takes the stall and returns at once (the 4090) still has it pending, and
    // so does one slow to submit it (RADV: about 55 ms for 67 draws): the readback behind
    // it has to time out.
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
    // Its own device spares the shared one, but not the process: macOS stops taking a
    // process's submissions after its second GPU restart, whichever device caused them.
    let context = pollster::block_on(GpuContext::request_high_performance())
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

    // About 100 ms of short draws: far past the 1 ms timeout, but no single draw long enough for
    // a driver to call it a hang (after two GPU restarts macOS ignores the process's submits).
    // Two timed runs cancel the fixed cost of submitting.
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
    // The fastest of three runs: a loaded machine only ever adds time, and one late run must not
    // pass for loop cost.
    let run = |iterations: u32| {
        (0..3)
            .map(|_| run_once(timed_draws, iterations))
            .fold(f64::INFINITY, f64::min)
    };
    let (short, long) = (run(25_000), run(125_000));
    let timed_iterations = f64::from(timed_draws) * 100_000.0;
    // On lavapipe the extra iterations cost nothing measurable (the longer run can be the
    // faster one): such a device cannot be stalled by this loop.
    if long - short < 0.005 {
        eprintln!(
            "skipped: the busy loop takes no measurable time on this device ({:.2} ms for {timed_iterations} iterations)",
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
    // Timed from before the stall is submitted: lavapipe runs the draws inside `submit`.
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
        eprintln!(
            "skipped: {reason} (stall submitted in {stalling:?}, readback in {submitting:?}, \
             queue empty: {queue_empty})"
        );
        renderer.set_gpu_wait_timeout(crate::DEFAULT_GPU_WAIT_TIMEOUT);
        return;
    }
    let error = match pending.wait() {
        Err(error) => error,
        Ok(planes) => {
            // Timed alone afterwards, the same draws tell a broken wait from a calibration a loaded
            // machine misled.
            renderer.set_gpu_wait_timeout(crate::DEFAULT_GPU_WAIT_TIMEOUT);
            let stall = std::time::Duration::from_secs_f64(run_once(draws, iterations));
            if stall < std::time::Duration::from_secs_f64(stall_seconds / 4.0) {
                eprintln!(
                    "skipped: the calibrated stall takes {stall:?} alone, not {stall_seconds} s: \
                     the busy loop's cost was the machine's load"
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
    // Compare values, not just size: a device that dies behind the stall reads back zeros.
    assert_eq!(
        planes, before,
        "the device reads back differently after the stall"
    );
}

#[test]
fn a_readback_the_gpu_did_not_run_stops_the_renderer() {
    // A macOS process that hung the GPU twice has submissions ignored yet completed, so a
    // readback maps on time over bytes the GPU never wrote.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    for _ in 0..3 {
        // The pooled staging buffer is reused; each readback's canary is its own.
        renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("readback submits")
            .wait()
            .expect("a readback the GPU ran carries its canary");
    }
    assert_eq!(renderer.gpu_lost(), None);

    // What a dropped submission leaves: a staging buffer ending in whatever it held before.
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

    // What a dropped submission leaves: a staging buffer that maps on time, ending in some
    // other readback's canary.
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
