//! Prints every adapter of every backend as
//! `backend<TAB>name<TAB>type<TAB>driver<TAB>driver_info<TAB>vendor:device`, plus the DX12 shader
//! compiler on Windows.

fn main() {
    let instance = alpha2_gpu::new_instance(wgpu::Backends::all());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    if adapters.is_empty() {
        println!("no adapters");
    }
    for adapter in adapters {
        let info = adapter.get_info();
        println!(
            "{:?}\t{}\t{:?}\t{}\t{}\t{:04x}:{:04x}",
            info.backend,
            info.name,
            info.device_type,
            info.driver,
            info.driver_info,
            info.vendor,
            info.device,
        );
    }
    #[cfg(windows)]
    println!(
        "dx12 shader compiler: {:?}",
        alpha2_gpu::dx12_shader_compiler()
    );
}
