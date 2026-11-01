//! Generates the native GPU helpers' Rust ABI bindings, builds the C helpers, and adds the
//! linked `FFmpeg`'s rpath to tests on macOS.

use std::env;

use alpha2_build_support::{
    FFMPEG_SDK_HINT, compile_c_shim, docs_rs, emit_ffmpeg_dev_rpath, ffmpeg_include_dirs,
    generate_interop_bindings, shared_include_dir,
};

fn main() {
    let shared = shared_include_dir();
    println!(
        "cargo:rerun-if-changed={}",
        shared.join("alpha2_vulkan_loader.h").display()
    );
    // Every target generates every platform's declarations. bindgen's layout assertions only cover
    // this target, so `src/lib.rs` pins the other platforms' layouts by hand.
    generate_interop_bindings(
        &[
            shared.join("alpha2_vulkan_interop.h"),
            "include/alpha2_native_gpu_d3d12.h".into(),
            "include/alpha2_native_gpu_metal.h".into(),
            "include/alpha2_native_gpu_status.h".into(),
        ],
        &[
            "Alpha2VulkanEncodeCopy",
            "Alpha2D3D12CopyInfo",
            "Alpha2D3D12EncodeCopy",
            "Alpha2D3D12EncodeCaps",
            "Alpha2MetalFrameInfo",
        ],
        "interop.rs",
    );
    // docs.rs documents without linking and has no FFmpeg headers to compile against.
    if env::var_os("CARGO_FEATURE_HWDEVICE").is_none() || docs_rs() {
        return;
    }
    emit_ffmpeg_dev_rpath();
    match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
        "linux" => {
            compile_c_shim(
                "alpha2_native_gpu_vulkan",
                &["c/vulkan.c"],
                &[shared, "include".into()],
                &ffmpeg_include_dirs(&["libavutil"]),
                &[],
                &format!("failed to compile native Vulkan helper. {FFMPEG_SDK_HINT}"),
            );
            // The Vulkan loader is opened at run time, not linked, so hosts without Vulkan still
            // start.
            println!("cargo:rustc-link-lib=dl");
        }
        "macos" => {
            compile_c_shim(
                "alpha2_native_gpu_metal",
                &["c/metal.m"],
                &["include".into()],
                &[],
                &["-fobjc-arc"],
                "failed to compile native Metal helper",
            );
            for framework in [
                "CoreFoundation",
                "CoreMedia",
                "CoreVideo",
                "IOSurface",
                "Metal",
                "VideoToolbox",
            ] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
        }
        "windows" => {
            compile_c_shim(
                "alpha2_native_gpu_d3d12",
                &["c/d3d12.c", "c/d3d12_encode.c", "c/d3d12_encode_caps.c"],
                &["include".into()],
                &[],
                &[],
                "failed to compile native D3D12 helper",
            );
            println!("cargo:rustc-link-lib=d3d12");
            println!("cargo:rustc-link-lib=dxgi");
        }
        _ => {}
    }
}
