//! The product device: the backend and adapter a host renders on, as tests now take it too.
#![cfg(not(target_arch = "wasm32"))]

use alpha2_gpu::GpuContext;

#[test]
fn the_product_device_is_on_this_platforms_backend() {
    let gpu = pollster::block_on(GpuContext::request_product()).expect("a product device");
    let backend = gpu.capabilities().backend.clone();
    eprintln!("product device: {}", gpu.capabilities().summary());
    if std::env::var_os("WGPU_BACKEND").is_some() {
        return;
    }
    let expected: &[&str] = if cfg!(target_os = "windows") {
        &["Dx12"]
    } else if cfg!(target_os = "macos") {
        &["Metal"]
    } else {
        // Vulkan, or GL where no Vulkan adapter opens a device.
        &["Vulkan", "Gl"]
    };
    assert!(expected.contains(&backend.as_str()), "{backend}");
}

/// Run by `a_pin_that_names_no_adapter_is_refused_with_the_list` with the pin set.
#[test]
#[ignore = "run by a_pin_that_names_no_adapter_is_refused_with_the_list"]
fn pinned_request_child() {
    let error = pollster::block_on(GpuContext::request_product()).expect_err("no such adapter");
    assert_eq!(error.step(), alpha2_gpu::GpuAcquisitionStep::Pin);
    let message = error.to_string();
    assert!(
        message.contains("ALPHA2_GPU_ADAPTER=no-such-gpu names no adapter"),
        "{message}"
    );
    assert!(
        message.contains("[0] "),
        "the adapters are listed: {message}"
    );
}

#[test]
fn a_pin_that_names_no_adapter_is_refused_with_the_list() {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "pinned_request_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ALPHA2_GPU_ADAPTER", "no-such-gpu")
        .output()
        .expect("child");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run by `a_dxc_path_to_no_file_fails_dx12_with_the_compiler_named` with the path set.
#[cfg(windows)]
#[test]
#[ignore = "run by a_dxc_path_to_no_file_fails_dx12_with_the_compiler_named"]
fn missing_dxc_child() {
    let error = pollster::block_on(GpuContext::request_product()).expect_err("no DX12 backend");
    assert_eq!(error.step(), alpha2_gpu::GpuAcquisitionStep::Instance);
    let message = error.to_string();
    assert!(message.contains("DX12 backend did not start"), "{message}");
    assert!(message.contains("alpha2-missing-dxc"), "{message}");
}

#[cfg(windows)]
#[test]
fn a_dxc_path_to_no_file_fails_dx12_with_the_compiler_named() {
    let missing = std::env::temp_dir()
        .join("alpha2-missing-dxc")
        .join("dxcompiler.dll");
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "missing_dxc_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ALPHA2_DXC_PATH", &missing)
        .env_remove("WGPU_BACKEND")
        .env_remove("WGPU_DX12_COMPILER")
        .output()
        .expect("child");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
