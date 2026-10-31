//! Prints every adapter of every backend, one per line, for CI and machine check logs:
//! `backend<TAB>name<TAB>type<TAB>driver<TAB>driver_info<TAB>vendor:device`, then on Windows the
//! DX12 shader compiler the instance settings name.
//!
//! `cargo run -p alpha2-gpu --example adapters`

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
