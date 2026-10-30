//! A device a host made itself and hands over reports what that device was opened with, not what
//! its adapter offers.

use alpha2_gpu::GpuContext;

#[test]
fn an_adopted_device_reports_its_own_features_and_limits() {
    let (adapter, device, queue) = {
        let _turn = alpha2_gpu::serialize_acquisition();
        let instance = alpha2_gpu::new_instance(wgpu::Backends::all());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("a GPU adapter");
        assert!(adapter.limits().max_texture_dimension_2d >= 2048);
        let required_limits = wgpu::Limits {
            max_texture_dimension_2d: 2048,
            ..wgpu::Limits::downlevel_webgl2_defaults()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: wgpu::Features::empty(),
            required_limits,
            ..wgpu::DeviceDescriptor::default()
        }))
        .expect("a device without timestamp queries");
        (adapter, device, queue)
    };
    let context = GpuContext::from_parts(adapter, device, queue);
    let capabilities = context.capabilities();
    assert!(
        !capabilities.timestamp_queries,
        "{}",
        capabilities.summary()
    );
    assert_eq!(capabilities.max_texture_dimension_2d, 2048);
}
