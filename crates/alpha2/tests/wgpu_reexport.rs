//! Handles made through `alpha2::gpu::wgpu` are accepted by `GpuContext`.
#![cfg(not(target_arch = "wasm32"))]

use alpha2::GpuContext;
use alpha2::gpu::wgpu;

// Use the native backend; an all-backends instance may pick a GL adapter that cannot create a
// device.
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
        let instance = alpha2::gpu::new_instance(BACKENDS);
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
