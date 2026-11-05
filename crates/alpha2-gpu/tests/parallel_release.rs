//! Devices made and dropped on parallel threads leave every thread free to exit.
//!
//! Some drivers leave per-thread data whose destructor lives in the driver library, so a thread
//! exiting after the last instance unloads the driver crashes. Test builds keep one instance for
//! the life of the process (see `serialize_acquisition`) to prevent that.
#![cfg(not(target_arch = "wasm32"))]

use alpha2_gpu::GpuContext;

const BACKENDS: wgpu::Backends = if cfg!(target_os = "windows") {
    wgpu::Backends::DX12
} else if cfg!(target_vendor = "apple") {
    wgpu::Backends::METAL
} else {
    wgpu::Backends::VULKAN
};

/// Rounds of two threads that each make, use and drop a device, then exit.
const ROUNDS: usize = 16;

fn device() -> GpuContext {
    let turn = alpha2_gpu::serialize_acquisition();
    let instance = alpha2_gpu::new_instance(BACKENDS);
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("an adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        // Within every adapter's limits, including reduced ones such as the iOS simulator's.
        required_limits: wgpu::Limits::downlevel_defaults(),
        ..wgpu::DeviceDescriptor::default()
    }))
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
