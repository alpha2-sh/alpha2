//! Generates the Rust side of the shims' interop ABI, builds the
//! target-specific `FFmpeg` platform shim, and adds the linked `FFmpeg`'s
//! rpath to this package's tests on macOS. It is the one package every
//! `FFmpeg` user builds through, so it is the one that checks the build
//! links the pinned SDK it was pointed at.

use std::env;

use alpha2_build_support::{
    FFMPEG_SDK_HINT, check_pinned_sdk, compile_ffmpeg_shim, docs_rs, emit_ffmpeg_dev_rpath,
    ffmpeg_include_dirs, generate_interop_bindings, shared_include_dir,
};

fn main() {
    check_pinned_sdk();
    emit_ffmpeg_dev_rpath();
    let shared = shared_include_dir();
    println!("cargo:rerun-if-changed=include/alpha2_ffmpeg_version.h");
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
            "include/alpha2_videotoolbox_interop.h".into(),
            "include/alpha2_d3d12_interop.h".into(),
        ],
        &[
            "Alpha2VulkanFrameInfo",
            "Alpha2VulkanFrameAccess",
            "Alpha2VulkanDeviceInfo",
            "Alpha2VulkanQueueFamily",
            "Alpha2VideoToolboxFrameInfo",
            "Alpha2D3D12FrameInfo",
        ],
        "interop.rs",
    );
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // docs.rs documents without linking and has no FFmpeg headers to compile against.
    if docs_rs() {
        return;
    }
    // Every build captures FFmpeg's log lines for typed errors (`src/log.rs`).
    compile_ffmpeg_shim(
        "alpha2_ffmpeg_log_shim",
        &["src/log_shim.c"],
        &["include".into()],
        &ffmpeg_include_dirs(&["libavutil"]),
        &format!("failed to compile the FFmpeg log shim. {FFMPEG_SDK_HINT}"),
    );
    if env::var_os("CARGO_FEATURE_HWDEVICE").is_none() {
        return;
    }
    // The VideoToolbox shim reads only FFmpeg's structures, so Linux compiles and tests it too.
    let (sources, library): (&[&str], _) = match target_os.as_str() {
        "linux" => (
            &["src/vulkan_shim.c", "src/videotoolbox_shim.c"],
            "alpha2_ffmpeg_vulkan_shim",
        ),
        "macos" => (
            &["src/videotoolbox_shim.c"],
            "alpha2_ffmpeg_videotoolbox_shim",
        ),
        "windows" => (&["src/d3d12_shim.c"], "alpha2_ffmpeg_d3d12_shim"),
        _ => return,
    };
    compile_ffmpeg_shim(
        library,
        sources,
        &["include".into(), shared],
        &ffmpeg_include_dirs(&["libavcodec", "libavutil"]),
        &format!("failed to compile target FFmpeg hardware-frame shim. {FFMPEG_SDK_HINT}"),
    );
    // The Vulkan shim opens the loader at run time (`alpha2_vulkan_loader.h`).
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=dl");
    }
}
