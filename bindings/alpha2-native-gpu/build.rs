//! Generates the Rust side of the native GPU helpers' ABI, builds the helpers
//! without exposing their types to the renderer, and adds the linked
//! `FFmpeg`'s rpath to this package's tests on macOS.

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
    // Every target generates every platform's declarations. bindgen's layout assertions come from
    // the same headers, so they only catch rustc and clang disagreeing on this target; `src/lib.rs`
    // pins the other platforms' layouts by hand so that a header edit fails on Linux too.
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
    // Only `hwdevice` links FFmpeg, through rsmpeg and the bridge.
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
            // The helper opens the Vulkan loader at run time (`alpha2_vulkan_loader.h`): a link
            // here would keep the host from starting where there is none.
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
