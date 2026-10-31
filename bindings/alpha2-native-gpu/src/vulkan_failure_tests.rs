//! What the encoder handoff and the import path do when the GPU or a helper
//! fails.
//!
//! Each test needs the device wgpu and `FFmpeg` share, which only a GPU with
//! Vulkan Video creates; elsewhere they skip, recorded as the `vulkan-video`
//! capability. A failure a helper cannot be made to produce on a healthy
//! device comes from [`inject`].

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use alpha2_ffmpeg_bridge::{GpuFault, VulkanEncodeFrameAccess};
use rsmpeg::avutil::AVFrame;

use super::inject::{self, Hook};
use super::*;

/// The shared device, or `None` (a recorded skip) where this GPU cannot make one.
pub(super) fn shared_context() -> Option<SharedVulkanContext> {
    alpha2_gpu::validation_log::install_unless_off();
    alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        SharedVulkanContext::create().map_err(|error| format!("no Vulkan Video device ({error})"))
    )
}

/// How long a child test may run before it counts as hung. A healthy one
/// takes a few seconds even beside a whole parallel gate; one waiting on a
/// driver that never answers would otherwise hold the suite for as long as
/// nobody looks (one such hang sat 18 minutes in a futex wait).
pub(super) const CHILD_DEADLINE: Duration = Duration::from_secs(60);

/// Runs `command`, a child test, to its end and returns what it printed. A
/// child still running after [`CHILD_DEADLINE`] is killed, and the test
/// fails naming `what` with what the child had printed by then.
pub(super) fn child_output(command: &mut Command, what: &str) -> std::process::Output {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child test");
    // Read both pipes while waiting, so a child that prints much cannot block on a full one.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let deadline = Instant::now() + CHILD_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("child status") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = stderr.join().unwrap_or_default();
            panic!(
                "{what} hung: still running after {CHILD_DEADLINE:?}, killed:\n{}",
                String::from_utf8_lossy(&stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    };
    // A child run by an `--exact` path that matches nothing runs nothing and still succeeds.
    assert!(
        !output.status.success() || String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{what} did not run:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    output
}

/// Runs `test`, an ignored `*_child` test of this module, in a child process:
/// a short wait timeout stays there, and a child still running after
/// [`CHILD_DEADLINE`] has hung, which fails the test.
///
/// Every test here that makes a shared device runs this way, one device per
/// process. Run side by side in one process, the NVIDIA 580 driver could
/// deadlock between one device's `vkDestroyDevice`, which joins a driver
/// thread whose exit spins in a thread-local destructor, and another
/// device's `vkCreateImageView`, blocked on the driver lock that thread
/// holds: one test thread at full CPU, the suite never ending. A host makes
/// one shared device per process, so only the tests can meet it.
fn run_child(test: &str) {
    let output = child_output(
        Command::new(std::env::current_exe().expect("test executable")).args([
            "--exact",
            &format!("vulkan::failure_tests::{test}"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]),
        test,
    );
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{test} failed: {}", output.status);
}

/// NV12 frames of the shared device's own pool, 64x64.
fn frames(context: &SharedVulkanContext, count: usize, usage: Option<u32>) -> Vec<AVFrame> {
    let mut frames = context.ffmpeg_device().hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = rsmpeg::ffi::AV_PIX_FMT_VULKAN;
        data.sw_format = rsmpeg::ffi::AV_PIX_FMT_NV12;
        data.width = 64;
        data.height = 64;
        data.initial_pool_size = 0;
    }
    if let Some(usage) = usage {
        alpha2_ffmpeg_bridge::vulkan_frames_add_usage(&mut frames, usage).expect("usage");
    }
    frames.init().expect("frames context");
    (0..count)
        .map(|_| {
            let mut frame = AVFrame::new();
            frames.get_buffer(&mut frame).expect("frame");
            frame
        })
        .collect()
}

/// `hwcontext_vulkan` keeps a usage set before init as given, dropping no flag,
/// so a pool reports exactly what it asked for; only a pool left at zero gets a
/// usage `FFmpeg` picked, which has `TRANSFER_DST`.
#[test]
fn a_pool_keeps_the_usage_it_asks_for_and_ffmpeg_picks_only_for_zero() {
    run_child("pool_usage_child");
}

#[test]
#[ignore = "run by a_pool_keeps_the_usage_it_asks_for_and_ffmpeg_picks_only_for_zero"]
fn pool_usage_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let pool = |usage: Option<u32>| {
        let mut frames = context.ffmpeg_device().hwframe_ctx_alloc();
        {
            let data = frames.data();
            data.format = rsmpeg::ffi::AV_PIX_FMT_VULKAN;
            data.sw_format = rsmpeg::ffi::AV_PIX_FMT_NV12;
            data.width = 64;
            data.height = 64;
            data.initial_pool_size = 0;
        }
        if let Some(usage) = usage {
            alpha2_ffmpeg_bridge::vulkan_frames_add_usage(&mut frames, usage).expect("usage");
        }
        frames.init().expect("frames context");
        alpha2_ffmpeg_bridge::vulkan_frames_usage(&frames)
    };
    let asked = alpha2_ffmpeg_bridge::VULKAN_ENCODE_INPUT_USAGE;
    assert_eq!(pool(Some(asked)), asked);
    let picked = pool(None);
    assert_ne!(
        picked & 0x2,
        0,
        "FFmpeg's own pick has TRANSFER_DST: {picked:#x}"
    );
}

/// Frames an encoder takes its input in.
pub(super) fn encode_frames(context: &SharedVulkanContext, count: usize) -> Vec<AVFrame> {
    frames(
        context,
        count,
        Some(alpha2_ffmpeg_bridge::VULKAN_ENCODE_INPUT_USAGE),
    )
}

/// Luma and chroma plane textures the renderer has drawn into, as a render
/// that packs a frame leaves them.
pub(super) fn rendered_planes(
    gpu: &GpuContext,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::Texture) {
    let plane = |format, width, height| {
        gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("handoff test plane"),
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
    };
    let luma = plane(wgpu::TextureFormat::R8Unorm, width, height);
    let chroma = plane(wgpu::TextureFormat::Rg8Unorm, width / 2, height / 2);
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for texture in [&luma, &chroma] {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("handoff test pack"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
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
        }));
    }
    gpu.submit([encoder.finish()]);
    (luma, chroma)
}

/// The device's raw handle, for the timeline waits wgpu does not expose.
fn raw_device(gpu: &GpuContext) -> ash::Device {
    // SAFETY: the shared context is always Vulkan; the handle is cloned, and used only while `gpu`
    // keeps the device alive.
    unsafe { gpu.device().as_hal::<wgpu_hal::api::Vulkan>() }
        .expect("Vulkan")
        .raw_device()
        .clone()
}

/// Whether `semaphore` reaches `value` within `timeout`.
fn reaches(device: &ash::Device, semaphore: u64, value: u64, timeout: Duration) -> bool {
    let semaphores = [vk::Semaphore::from_raw(semaphore)];
    let values = [value];
    let wait = vk::SemaphoreWaitInfo::default()
        .semaphores(&semaphores)
        .values(&values);
    let nanoseconds = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
    // SAFETY: a wait on a live timeline semaphore of this device.
    unsafe { device.wait_semaphores(&wait, nanoseconds) }.is_ok()
}

/// A submission wgpu refuses: it copies from a buffer destroyed before it
/// was submitted, so none of it reaches the queue.
fn submit_a_rejected_render(gpu: &GpuContext) {
    let buffer = |usage| {
        gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("rejected render"),
            size: 16,
            usage,
            mapped_at_creation: false,
        })
    };
    let source = buffer(wgpu::BufferUsages::COPY_SRC);
    let target = buffer(wgpu::BufferUsages::COPY_DST);
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_buffer_to_buffer(&source, 0, &target, 0, 16);
    let commands = encoder.finish();
    source.destroy();
    gpu.submit([commands]);
}

#[test]
fn a_copy_after_a_render_the_gpu_rejected_runs_and_leaves_the_queue_moving() {
    // A copy that waited on the GPU for a timeline value the render's submission was to signal
    // would wait forever when wgpu rejects that submission, stalling the queue the renderer and
    // FFmpeg share.
    run_child("rejected_render_copy_child");
}

#[test]
#[ignore = "run by a_copy_after_a_render_the_gpu_rejected_runs_and_leaves_the_queue_moving"]
fn rejected_render_copy_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let log = Arc::new(alpha2_gpu::GpuDiagnosticLog::default());
    let recorded = Arc::clone(&log);
    gpu.set_diagnostic_handler(move |diagnostic| recorded.push(diagnostic));
    // A wait that does not finish says so in seconds rather than the default half minute.
    set_wait_timeout(Duration::from_secs(2));
    let device = raw_device(&gpu);
    let mut copier = VulkanEncodeCopier::new(&context).expect("copier");
    let (luma, chroma) = rendered_planes(&gpu, 64, 64);
    let mut frames = encode_frames(&context, 1);

    // The frame's render, which wgpu refuses.
    submit_a_rejected_render(&gpu);
    assert_eq!(
        log.snapshot().failure_code(),
        Some("VALIDATION_FAILED"),
        "the render was refused: {:?}",
        log.snapshot()
    );
    let mut access = VulkanEncodeFrameAccess::begin(&mut frames[0]).expect("frame access");
    copier
        .submit(&mut access, &luma, &chroma, (64, 64))
        .expect("the copy submits");
    let semaphore = access.access().frame().semaphores[0];
    let signalled = access.access().frame().semaphore_values[0];
    drop(access);

    // The copy runs, so the encode that waits for it can.
    assert!(
        reaches(&device, semaphore, signalled, Duration::from_secs(5)),
        "the copy is waiting for a render that never ran"
    );
    // And the queue moves on: a later submission completes.
    let done = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&done);
    gpu.submit([]);
    gpu.on_submitted_work_done(move || signal.store(true, Ordering::Release));
    gpu.device()
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(5)),
        })
        .expect("the queue drains");
    assert!(done.load(Ordering::Acquire));
    copier.finish().expect("no copy is left waiting");
}

/// One imported frame, and the frame lease the import holds.
struct Import {
    imported: ImportedVulkanFrame,
    frame: Arc<VulkanFrameLease>,
}

fn import(context: &SharedVulkanContext, frame: AVFrame) -> Import {
    let frame = Arc::new(VulkanFrameLease::new(frame));
    let imported = context
        .import_surface(Arc::clone(&frame))
        .expect("the frame imports");
    Import { imported, frame }
}

/// The frame's timeline semaphore and the value its state records.
fn timeline(frame: &VulkanFrameLease) -> (u64, u64) {
    let access = frame.lock_access().expect("frame access");
    let descriptor = access.access().frame();
    (descriptor.semaphores[0], descriptor.semaphore_values[0])
}

fn staged(lease: &VulkanImportLease) -> Vec<StagedSignal> {
    lease
        .staged_signals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Sampled frames, as a decoder's pool hands them out, imported.
fn imports(context: &SharedVulkanContext, count: usize) -> Vec<Import> {
    frames(context, count, None)
        .into_iter()
        .map(|frame| import(context, frame))
        .collect()
}

#[test]
fn a_frame_dropped_after_its_second_import_failed_to_acquire_gives_the_first_back() {
    // The first source of a frame is acquired and its signal staged on the renderer's queue, then
    // the second source's acquire fails and the frame is dropped. Dropping the first lease must
    // take its signal off the renderer's queue, raise it with a submission of its own so the value
    // the frame records is reached, and return its ticket. A release that waits for the signal
    // taken off the queue waits forever, so this runs in a child.
    run_child("dropped_frame_rollback_child");
}

#[test]
#[ignore = "run by a_frame_dropped_after_its_second_import_failed_to_acquire_gives_the_first_back"]
fn dropped_frame_rollback_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let mut sources = imports(&context, 2);
    let second = sources.pop().expect("second");
    let first = sources.pop().expect("first");
    inject::fail_nth(Hook::AcquireSubmit, 2);
    first.imported.lease.prepare_submission();
    second.imported.lease.prepare_submission();
    let error = first
        .imported
        .lease
        .flush_submission()
        .expect_err("the second acquire fails");
    assert_eq!(error.gpu_fault(), Some(GpuFault::DeviceLost), "{error}");
    assert!(first.imported.lease.acquired.load(Ordering::Acquire));
    assert!(!second.imported.lease.acquired.load(Ordering::Acquire));
    let signals = staged(&first.imported.lease);
    assert_eq!(signals.len(), 1, "{signals:?}");
    let signal = signals[0];

    let (frame, other) = (Arc::clone(&first.frame), Arc::clone(&second.frame));
    drop((first, second));
    // SAFETY: the shared context is always Vulkan.
    let queue = unsafe { gpu.queue().as_hal::<wgpu_hal::api::Vulkan>() }.expect("Vulkan");
    assert!(
        !queue.remove_signal_semaphore(vk::Semaphore::from_raw(signal.semaphore)),
        "the dropped frame's signal would ride on the next submission"
    );
    drop(queue);
    let (semaphore, value) = timeline(&frame);
    assert_eq!(semaphore, signal.semaphore);
    assert_eq!(
        value,
        signal.acquired_value + 1,
        "the frame records the signal the dropped lease raises itself"
    );
    assert!(
        reaches(&device, semaphore, value, Duration::from_secs(5)),
        "the signal taken off the queue must be raised all the same"
    );
    let (unacquired, untouched) = timeline(&other);
    assert!(reaches(&device, unacquired, untouched, Duration::ZERO));

    let stats = context.import_ticket_stats();
    assert_eq!((stats.created, stats.pending, stats.abandoned), (2, 2, 0));
    // Both tickets are finished: the next import reuses one and keeps the other.
    let _next = imports(&context, 1);
    let stats = context.import_ticket_stats();
    assert_eq!(
        (stats.created, stats.reused, stats.idle, stats.pending),
        (2, 1, 1, 0),
        "{stats:?}"
    );
}

#[test]
fn a_dropped_frame_raises_the_signal_a_decode_already_waits_for() {
    // An import stages the signal its sampling submission was to raise and records it as the
    // frame's value; a decode then uses the frame as a reference, so it queues a wait on that value
    // and records the next. The frame is dropped before the sampling submission: only taking the
    // signal back off the queue would leave the decode, and the release behind it, waiting forever,
    // so this runs in a child.
    run_child("raised_signal_child");
}

#[test]
#[ignore = "run by a_dropped_frame_raises_the_signal_a_decode_already_waits_for"]
fn raised_signal_child() {
    let Some(context) = shared_context() else {
        return;
    };
    set_wait_timeout(Duration::from_secs(2));
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let source = imports(&context, 1).remove(0);
    source.imported.lease.prepare_submission();
    source
        .imported
        .lease
        .flush_submission()
        .expect("the frame acquires");
    let signals = staged(&source.imported.lease);
    assert_eq!(signals.len(), 1, "{signals:?}");
    let signal = signals[0];
    // The decode, as FFmpeg's: under the frame's lock it reads the value the frame records, waits
    // for it, and records the one it raises after. It runs on the host here, so it waits on nothing
    // else of the GPU's.
    let decoded = signal.signal_value + 1;
    {
        let mut access = source.frame.lock_access().expect("frame access");
        // SAFETY: the test plays FFmpeg's decode, whose host signal of `decoded` below it waits
        // for.
        let descriptor = unsafe { access.access_mut() }.frame_mut();
        for index in 0..descriptor.images.len() {
            if descriptor.semaphores[index] == signal.semaphore {
                assert_eq!(descriptor.semaphore_values[index], signal.signal_value);
                descriptor.semaphore_values[index] = decoded;
            }
        }
        access.commit();
    }
    let decode = std::thread::spawn({
        let device = device.clone();
        move || {
            let waited = reaches(
                &device,
                signal.semaphore,
                signal.signal_value,
                Duration::from_secs(10),
            );
            if waited {
                // SAFETY: the frame's live timeline, past the value just reached and with nothing
                // else pending on it.
                unsafe {
                    device
                        .signal_semaphore(
                            &vk::SemaphoreSignalInfo::default()
                                .semaphore(vk::Semaphore::from_raw(signal.semaphore))
                                .value(decoded),
                        )
                        .expect("signal");
                }
            }
            waited
        }
    });
    // The sampling submission never comes. The frame itself stays, as a decoder's reference does:
    // freeing it would free its timeline.
    let frame = Arc::clone(&source.frame);
    drop(source);
    assert!(
        decode.join().expect("decode thread"),
        "the decode waited for a signal that was taken off the queue and never raised"
    );
    assert!(reaches(
        &device,
        signal.semaphore,
        decoded,
        Duration::from_secs(5)
    ));
    gpu.submit([]);
    gpu.device()
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(5)),
        })
        .expect("the queue drains");
    let stats = context.import_ticket_stats();
    assert_eq!((stats.created, stats.abandoned), (1, 0), "{stats:?}");
    drop(frame);
}

#[test]
fn a_ticket_whose_acquire_may_still_run_waits_in_the_pool_until_it_has() {
    // An acquire is submitted, its signal is never staged, and the release that would have
    // followed it fails. Dropping the lease must not free the ticket's command pool while the
    // acquire is still waiting on the decoder's timeline. Freeing commands the GPU has yet to run
    // is undefined, so this runs in a child.
    run_child("unfenced_acquire_child");
}

#[test]
#[ignore = "run by a_ticket_whose_acquire_may_still_run_waits_in_the_pool_until_it_has"]
fn unfenced_acquire_child() {
    let Some(context) = shared_context() else {
        return;
    };
    set_wait_timeout(Duration::from_secs(2));
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let source = imports(&context, 1).remove(0);
    // What `FFmpeg` submitted for the frame finishes first, so the signal below is the timeline's
    // only one from the host.
    let (semaphore, value) = timeline(&source.frame);
    assert!(reaches(&device, semaphore, value, Duration::from_secs(5)));
    // The decoder has not raised the value the frame records, so the acquire waits.
    let (semaphore, blocked) = {
        let mut access = source.frame.lock_access().expect("frame access");
        // SAFETY: breaks the contract on purpose: the acquire is to wait on a value nothing
        // signals yet, which the test signals from the host before the frame is used again.
        let descriptor = unsafe { access.access_mut() }.frame_mut();
        let blocked = descriptor.semaphore_values[0] + 1000;
        for index in 0..descriptor.images.len() {
            if descriptor.images[index] != 0 {
                descriptor.semaphore_values[index] = blocked;
            }
        }
        let semaphore = descriptor.semaphores[0];
        access.commit();
        (semaphore, blocked)
    };
    {
        let lease = &source.imported.lease;
        let mut access = lease.lock().expect("frame access");
        let mut status = 0;
        // SAFETY: the guard holds the frame and queue locks, and the ticket was made for this frame
        // at import.
        let step = unsafe { acquire_submit(&mut access, &mut status) };
        assert_eq!(step, 0, "acquire: {status}");
        lease.acquired.store(true, Ordering::Release);
        access.frame.commit();
    }
    // SAFETY: a counter read.
    let retained = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    inject::fail_nth(Hook::Release, 1);
    // The frame stays, as a decoder's reference does: `FFmpeg` would wait for the acquire to free
    // it.
    let frame = Arc::clone(&source.frame);
    drop(source);
    let stats = context.import_ticket_stats();
    assert_eq!(
        (stats.created, stats.pending, stats.abandoned),
        (1, 1, 0),
        "a ticket whose acquire may still run waits in the pool: {stats:?}"
    );
    // SAFETY: a counter read.
    let retained_after = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    assert_eq!(retained_after, retained, "nothing was leaked either");
    // Still running, so the next import does not take it.
    let next = imports(&context, 1);
    let stats = context.import_ticket_stats();
    assert_eq!(
        (stats.created, stats.reused, stats.pending),
        (2, 0, 1),
        "{stats:?}"
    );
    // The decoder catches up: the acquire runs, and the ticket is reused.
    // SAFETY: the frame's live timeline, signalled to the value the acquire waits for.
    unsafe {
        device
            .signal_semaphore(
                &vk::SemaphoreSignalInfo::default()
                    .semaphore(vk::Semaphore::from_raw(semaphore))
                    .value(blocked),
            )
            .expect("signal");
    }
    assert!(reaches(
        &device,
        semaphore,
        blocked + 1,
        Duration::from_secs(5)
    ));
    // SAFETY: the shared device; waiting for it to be idle lets the acquire's fence signal before
    // the poll.
    unsafe { device.device_wait_idle() }.expect("idle");
    drop(next);
    let _again = imports(&context, 1);
    let stats = context.import_ticket_stats();
    assert_eq!((stats.created, stats.reused), (2, 1), "{stats:?}");
    drop(frame);
}

#[test]
fn a_frame_retried_after_an_import_failed_to_acquire_acquires_what_it_missed() {
    // The batch re-enqueue: a flush drains the batch, so a lease left unacquired by a failure must
    // be queueable again, or the frame's retry never acquires it and samples it in its decode
    // layout.
    run_child("retried_acquire_child");
}

#[test]
#[ignore = "run by a_frame_retried_after_an_import_failed_to_acquire_acquires_what_it_missed"]
fn retried_acquire_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let sources = imports(&context, 2);
    inject::fail_nth(Hook::AcquireSubmit, 2);
    for source in &sources {
        source.imported.lease.prepare_submission();
    }
    sources[0]
        .imported
        .lease
        .flush_submission()
        .expect_err("the second acquire fails");
    for source in &sources {
        source.imported.lease.prepare_submission();
    }
    sources[0]
        .imported
        .lease
        .flush_submission()
        .expect("the retry acquires the second frame");
    for source in &sources {
        assert!(source.imported.lease.acquired.load(Ordering::Acquire));
        assert_eq!(source.imported.lease.staged_signals(), 1);
    }
    // The sampling submission raises both frames' signals.
    gpu.submit([]);
    for source in &sources {
        source.imported.lease.submitted().expect("released");
    }
    let timelines = sources
        .iter()
        .map(|source| timeline(&source.frame))
        .collect::<Vec<_>>();
    for (semaphore, value) in timelines {
        assert!(reaches(&device, semaphore, value, Duration::from_secs(5)));
    }
}

#[test]
fn a_ticket_the_device_cannot_vouch_for_is_never_reused() {
    // The reclaim branches: a ticket whose fence poll fails, or whose reset fails, is abandoned
    // rather than reused; finished tickets beyond the idle cap are destroyed.
    run_child("unvouched_ticket_child");
}

#[test]
#[ignore = "run by a_ticket_the_device_cannot_vouch_for_is_never_reused"]
fn unvouched_ticket_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let stats = || context.import_ticket_stats();
    drop(imports(&context, 1));
    assert_eq!((stats().created, stats().pending), (1, 1));

    inject::fail_nth(Hook::TicketPoll, 1);
    drop(imports(&context, 1));
    let after_poll = stats();
    assert_eq!(
        (after_poll.created, after_poll.reused, after_poll.abandoned),
        (2, 0, 1),
        "a ticket its device cannot report on is not reused: {after_poll:?}"
    );

    inject::fail_nth(Hook::TicketReset, 1);
    drop(imports(&context, 1));
    let after_reset = stats();
    assert_eq!(
        (
            after_reset.created,
            after_reset.reused,
            after_reset.abandoned
        ),
        (3, 0, 2),
        "a ticket that cannot be reset is not reused: {after_reset:?}"
    );

    // More finished tickets than the idle cap: the extra ones are destroyed.
    let burst = MAX_IDLE_IMPORT_TICKETS + 2;
    drop(imports(&context, burst));
    assert_eq!(stats().pending, burst);
    let _next = imports(&context, 1);
    let after_burst = stats();
    assert_eq!(after_burst.pending, 0, "{after_burst:?}");
    assert_eq!(
        after_burst.idle,
        MAX_IDLE_IMPORT_TICKETS - 1,
        "{after_burst:?}"
    );
    assert_eq!(after_burst.abandoned, 2, "{after_burst:?}");
}

#[test]
fn too_many_unfinished_tickets_wait_a_bounded_time_then_leak_the_oldest() {
    // The give-back overflow: past MAX_PENDING_IMPORT_TICKETS tickets whose releases are still
    // running, the oldest is destroyed after a bounded wait, which, when the wait runs out, leaks
    // it instead of freeing what the GPU still uses.
    run_child("pending_overflow_child");
}

/// Makes every later acquire and release of `frame` wait, through its timeline, for a value nothing
/// has signalled yet; returns the timeline and that value, for the test to signal from the host
/// before the frame is used again.
fn block_timeline(frame: &VulkanFrameLease) -> (u64, u64) {
    let mut access = frame.lock_access().expect("frame access");
    // SAFETY: breaks the contract on purpose: what is submitted next waits on a value nothing
    // signals yet, which the test signals from the host before the frame is used again.
    let descriptor = unsafe { access.access_mut() }.frame_mut();
    let blocked = descriptor.semaphore_values[0] + 1000;
    for index in 0..descriptor.images.len() {
        if descriptor.images[index] != 0 {
            descriptor.semaphore_values[index] = blocked;
        }
    }
    let semaphore = descriptor.semaphores[0];
    access.commit();
    (semaphore, blocked)
}

/// Gives `pool` back `count` tickets whose acquire and release, on the blocked `frame`, are still
/// waiting.
fn give_back_unfinished(pool: &ImportTicketPool, frame: &VulkanFrameLease, count: usize) {
    for _ in 0..count {
        let access = frame.lock_access().expect("frame access");
        let (device, allocator, queue_family) = {
            let access = access.access();
            (
                access.frame().device,
                access.allocator(),
                access.queue_family(),
            )
        };
        let ticket = pool
            .take(device, allocator, queue_family)
            .expect("a ticket");
        let mut lease = LeaseAccess {
            frame: access,
            ticket: ticket.ticket,
        };
        let mut status = 0;
        // SAFETY: the guard holds the frame and queue locks, and the ticket was made for this
        // frame's device and queue family.
        unsafe {
            assert_eq!(acquire_submit(&mut lease, &mut status), 0);
            assert_eq!(release(&mut lease, &mut status), 0);
        }
        lease.frame.commit();
        drop(lease);
        pool.give_back(ticket);
    }
}

/// Lets everything waiting on the blocked timeline run, and waits for the device to be idle.
fn unblock(device: &ash::Device, (semaphore, blocked): (u64, u64)) {
    // SAFETY: the frame's live timeline, signalled to a value above its current one.
    unsafe {
        device
            .signal_semaphore(
                &vk::SemaphoreSignalInfo::default()
                    .semaphore(vk::Semaphore::from_raw(semaphore))
                    .value(blocked),
            )
            .expect("signal");
        device.device_wait_idle().expect("idle");
    }
}

#[test]
#[ignore = "run by too_many_unfinished_tickets_wait_a_bounded_time_then_leak_the_oldest"]
fn pending_overflow_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let frame = VulkanFrameLease::new(frames(&context, 1, None).remove(0));
    let pool = ImportTicketPool::default();
    let blocked = block_timeline(&frame);
    set_wait_timeout(Duration::from_millis(1));
    // SAFETY: a counter read.
    let retained_before = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    give_back_unfinished(&pool, &frame, MAX_PENDING_IMPORT_TICKETS + 1);
    let stats = pool.stats();
    assert_eq!(
        (stats.pending, stats.abandoned),
        (MAX_PENDING_IMPORT_TICKETS, 1),
        "{stats:?}"
    );
    // SAFETY: a counter read.
    let retained_after = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    assert_eq!(
        retained_after,
        retained_before + 1,
        "the oldest ticket's release was still running: it must be leaked, not freed"
    );
    // Let every release run, so the pool frees the rest and the device ends idle.
    unblock(&device, blocked);
    drop(pool);
}

#[test]
fn a_ticket_given_back_past_the_cap_waits_without_holding_the_pool() {
    // Destroying the overflow ticket waits for its release, up to the wait timeout. Waiting under
    // the pool's lock would stall every import of the device for that long.
    run_child("overflow_outside_lock_child");
}

#[test]
#[ignore = "run by a_ticket_given_back_past_the_cap_waits_without_holding_the_pool"]
fn overflow_outside_lock_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let frame = VulkanFrameLease::new(frames(&context, 1, None).remove(0));
    let pool = ImportTicketPool::default();
    let blocked = block_timeline(&frame);
    set_wait_timeout(Duration::from_millis(1));
    give_back_unfinished(&pool, &frame, MAX_PENDING_IMPORT_TICKETS);
    let wait = Duration::from_secs(3);
    set_wait_timeout(wait);
    let started = Instant::now();
    let retires = inject::retires_begun();
    let looked = std::thread::scope(|scope| {
        let overflow = scope.spawn(|| give_back_unfinished(&pool, &frame, 1));
        // The pool is asked once the give-back is in its wait for the oldest ticket.
        while inject::retires_begun() == retires {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the give-back never began waiting for the oldest ticket"
            );
            std::thread::yield_now();
        }
        let asked = Instant::now();
        let stats = pool.stats();
        let looked = asked.elapsed();
        overflow.join().expect("give back");
        assert_eq!(stats.pending, MAX_PENDING_IMPORT_TICKETS, "{stats:?}");
        looked
    });
    assert!(
        started.elapsed() >= wait,
        "the overflow ticket's release was still running, so its destroy waited it out"
    );
    assert!(
        looked < Duration::from_secs(1),
        "the pool answered only after {looked:?}: the overflow wait held its lock"
    );
    unblock(&device, blocked);
    drop(pool);
}

#[test]
fn a_pool_dropped_with_unfinished_tickets_waits_once_for_all_of_them() {
    // A device that is hung but not lost: every pending ticket's fence stays unsignalled. The
    // pool's drop must give them all one wait timeout together, and leak what is still unfinished,
    // rather than wait the whole timeout for each in turn.
    run_child("pool_drop_deadline_child");
}

#[test]
#[ignore = "run by a_pool_dropped_with_unfinished_tickets_waits_once_for_all_of_them"]
fn pool_drop_deadline_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    let device = raw_device(&gpu);
    let frame = VulkanFrameLease::new(frames(&context, 1, None).remove(0));
    let pool = ImportTicketPool::default();
    let blocked = block_timeline(&frame);
    set_wait_timeout(Duration::from_millis(1));
    give_back_unfinished(&pool, &frame, MAX_PENDING_IMPORT_TICKETS);
    assert_eq!(pool.stats().pending, MAX_PENDING_IMPORT_TICKETS);
    let wait = Duration::from_millis(400);
    set_wait_timeout(wait);
    // SAFETY: a counter read.
    let retained_before = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    let started = Instant::now();
    drop(pool);
    let took = started.elapsed();
    // SAFETY: a counter read.
    let retained_after = unsafe { alpha2_native_gpu_vulkan_retained_helpers() };
    assert_eq!(
        retained_after - retained_before,
        MAX_PENDING_IMPORT_TICKETS as u64,
        "every ticket was still running: each must be leaked, not freed"
    );
    assert!(
        took >= wait && took < wait * 3,
        "the pool's drop took {took:?} for {MAX_PENDING_IMPORT_TICKETS} unfinished tickets with \
         a {wait:?} wait timeout: they must share one deadline"
    );
    unblock(&device, blocked);
}

/// The shared device's imports, as the cross-backend suite drives them.
struct Conformance;

impl crate::conformance::Backend for Conformance {
    type Context = SharedVulkanContext;
    type Surface = VulkanFrameLease;
    const BACKEND: alpha2_ffmpeg_bridge::NativeGpuBackend =
        alpha2_ffmpeg_bridge::NativeGpuBackend::Vulkan;
    // What `vkQueueSubmit`, the acquire's first GPU call, can return; VK_ERROR_UNKNOWN names no
    // fault.
    const STATUSES: &'static [(i32, Option<GpuFault>)] = &[
        (
            vk::Result::ERROR_DEVICE_LOST.as_raw(),
            Some(GpuFault::DeviceLost),
        ),
        (
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY.as_raw(),
            Some(GpuFault::DeviceMemory),
        ),
        (
            vk::Result::ERROR_OUT_OF_HOST_MEMORY.as_raw(),
            Some(GpuFault::HostMemory),
        ),
        (vk::Result::ERROR_UNKNOWN.as_raw(), None),
    ];

    fn context() -> Option<SharedVulkanContext> {
        shared_context()
    }

    fn surface(context: &SharedVulkanContext) -> Arc<VulkanFrameLease> {
        let frame = frames(context, 1, None).remove(0);
        Arc::new(VulkanFrameLease::new(frame))
    }

    fn fail_next_import(status: i32) {
        inject::fail_nth_with(Hook::AcquireSubmit, 1, status);
    }

    fn import(
        context: &SharedVulkanContext,
        surface: Arc<VulkanFrameLease>,
    ) -> Result<(), BridgeError> {
        let imported = context.import_surface(surface)?;
        imported.lease.prepare_submission();
        imported.lease.flush_submission()?;
        let gpu = context.gpu();
        let submitted = gpu.submit([]);
        imported.lease.submitted()?;
        gpu.device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submitted),
                timeout: Some(Duration::from_secs(5)),
            })
            .expect("the sampling submission finishes");
        Ok(())
    }

    fn leaked() -> u64 {
        // SAFETY: a counter read.
        unsafe { alpha2_native_gpu_vulkan_retained_helpers() }
    }
}

#[test]
fn the_vulkan_import_keeps_the_interop_contract() {
    run_child("conformance_child");
}

#[test]
#[ignore = "run by the_vulkan_import_keeps_the_interop_contract"]
fn conformance_child() {
    crate::conformance::check::<Conformance>();
}
