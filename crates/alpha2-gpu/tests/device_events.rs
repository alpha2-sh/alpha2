//! A real device error and a real device loss reach the diagnostic handler
//! typed, on a context made either way a host makes one: the
//! evidence a failure report classifies by comes only through this chain.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alpha2_gpu::{ExternalQueueLock, GpuContext, GpuDiagnostic, GpuDiagnosticLog};

/// Counts how often a submission took it; nothing else submits here, so it
/// needs to exclude nothing.
#[derive(Debug, Default)]
struct CountingLock {
    taken: AtomicUsize,
}

impl ExternalQueueLock for CountingLock {
    fn lock(&self) {
        self.taken.fetch_add(1, Ordering::AcqRel);
    }

    fn unlock(&self) {}
}

/// A context made from handles a host created itself, sharing its queue.
fn shared_queue_context(lock: Arc<CountingLock>) -> GpuContext {
    let _turn = alpha2_gpu::serialize_acquisition();
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("a GPU adapter");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("a device");
    GpuContext::from_parts_with_queue_lock(adapter, device, queue, Some(lock))
}

fn logged(gpu: &GpuContext) -> Arc<GpuDiagnosticLog> {
    let log = Arc::new(GpuDiagnosticLog::default());
    let recorded = Arc::clone(&log);
    gpu.set_diagnostic_handler(move |diagnostic: GpuDiagnostic| recorded.push(diagnostic));
    log
}

/// A buffer both mapped for reading and for writing, which WebGPU refuses.
fn make_an_invalid_buffer(gpu: &GpuContext) {
    drop(gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("invalid usage"),
        size: 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::MAP_WRITE,
        mapped_at_creation: false,
    }));
}

fn check_errors_and_loss_reach_the_log(gpu: &GpuContext) {
    let log = logged(gpu);
    make_an_invalid_buffer(gpu);
    gpu.submit([gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default())
        .finish()]);
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the device drains");
    let snapshot = log.snapshot();
    assert_eq!(snapshot.entries.len(), 1, "{snapshot:?}");
    assert_eq!(snapshot.entries[0].failure_code(), "VALIDATION_FAILED");
    assert_eq!(snapshot.failure_code(), Some("VALIDATION_FAILED"));
    assert!(!snapshot.device_lost);

    gpu.device().destroy();
    let _ = gpu.device().poll(wgpu::PollType::wait_indefinitely());
    let snapshot = log.snapshot();
    assert!(snapshot.device_lost, "{snapshot:?}");
    assert!(
        matches!(
            snapshot.entries.last(),
            Some(GpuDiagnostic::DeviceLost { reason, .. }) if reason == "Destroyed"
        ),
        "{snapshot:?}"
    );
    assert_eq!(snapshot.failure_code(), Some("DEVICE_LOST"));
}

#[test]
fn a_requested_device_reports_its_errors_and_its_loss() {
    let gpu = pollster::block_on(GpuContext::request()).expect("a GPU adapter");
    check_errors_and_loss_reach_the_log(&gpu);
}

#[test]
fn a_device_adopted_with_a_shared_queue_reports_its_errors_and_its_loss() {
    let lock = Arc::new(CountingLock::default());
    let gpu = shared_queue_context(Arc::clone(&lock));
    check_errors_and_loss_reach_the_log(&gpu);
    assert!(lock.taken.load(Ordering::Acquire) >= 1);
}
