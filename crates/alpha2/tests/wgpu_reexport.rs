//! A user of the facade's `wgpu-interop` feature who does not depend on `wgpu`
//! themselves reaches the `wgpu` the interop surface speaks through
//! `alpha2::gpu::wgpu`, and handles made with it are the ones `GpuContext`
//! adopts.
#![cfg(not(target_arch = "wasm32"))]

use alpha2::GpuContext;
use alpha2::gpu::wgpu;

/// The backend each platform's host opens its device on. An instance on every
/// backend may prefer an adapter that makes no device (a GL one where only a
/// Vulkan driver is installed).
const BACKENDS: wgpu::Backends = if cfg!(target_os = "windows") {
    wgpu::Backends::DX12
} else if cfg!(target_os = "macos") {
    wgpu::Backends::METAL
} else {
    wgpu::Backends::VULKAN
};

#[test]
fn handles_made_through_the_reexported_wgpu_are_adopted_by_a_gpu_context() {
    let (adapter, device, queue) = {
        let _turn = alpha2::gpu::serialize_acquisition();
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = BACKENDS;
        let instance = wgpu::Instance::new(descriptor.with_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .expect("a GPU adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("a GPU device");
        (adapter, device, queue)
    };
    let name = adapter.get_info().name;

    let context = GpuContext::from_parts(adapter, device, queue);

    assert_eq!(context.capabilities().adapter_name, name);
}
