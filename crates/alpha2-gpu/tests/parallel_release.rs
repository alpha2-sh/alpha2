//! Devices made and dropped on parallel threads, as a test binary's tests
//! make and drop theirs, leave every thread free to exit.
//!
//! A thread that used a driver can keep per-thread data whose destructor
//! lives in that driver's libraries (the NVIDIA 580 Vulkan driver's EGL
//! library does). When the process's last instance goes, the loader unloads
//! the drivers, and a thread that exits after that dies with SIGSEGV. Test
//! builds keep one instance for the life of the process (see
//! `serialize_acquisition`), so the drivers stay loaded however the tests'
//! own instances come and go.
#![cfg(not(target_arch = "wasm32"))]

use alpha2_gpu::GpuContext;

/// The backend each platform's host renders on, as in `accepted_device.rs`.
const BACKENDS: wgpu::Backends = if cfg!(target_os = "windows") {
    wgpu::Backends::DX12
} else if cfg!(target_os = "macos") {
    wgpu::Backends::METAL
} else {
    wgpu::Backends::VULKAN
};

/// Rounds of two threads that each make a device, use it, drop it and exit.
/// Without the kept instance, the NVIDIA 580 driver killed the binary in the
/// first rounds on every run.
const ROUNDS: usize = 16;

/// A device on the adapter a default request picks, made as a test that
/// shares its queue makes one.
fn device() -> GpuContext {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = BACKENDS;
    let turn = alpha2_gpu::serialize_acquisition();
    let instance = wgpu::Instance::new(descriptor.with_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("an adapter");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("a device");
    drop(turn);
    GpuContext::from_parts(adapter, device, queue)
}

#[test]
fn threads_that_make_and_drop_devices_side_by_side_exit_cleanly() {
    for round in 0..ROUNDS {
        let threads: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(|| {
                    let gpu = device();
                    gpu.submit([]);
                    gpu.device()
                        .poll(wgpu::PollType::wait_indefinitely())
                        .expect("the device drains");
                })
            })
            .collect();
        for thread in threads {
            thread
                .join()
                .unwrap_or_else(|_| panic!("round {round}: a thread panicked"));
        }
    }
}
