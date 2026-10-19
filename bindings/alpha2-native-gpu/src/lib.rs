//! Platform GPU interop for native decoded video surfaces.
//!
//! `FFmpeg` ownership remains in `alpha2-ffmpeg-bridge`; this crate owns raw GPU
//! handles, barriers, synchronization, and bounded surface accounting.

#[cfg(all(
    test,
    feature = "hwdevice",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
mod conformance;
mod context;
/// The fixed-width ABI shared with the C helpers, generated from their
/// headers by the build script, with bindgen's compile-time layout
/// assertions. Every target generates every platform's declarations. The
/// assertions come from the same header as the struct, so they agree with any
/// edit to it; the structs only Windows or macOS consumes are also pinned by
/// hand below.
#[allow(
    dead_code,
    non_camel_case_types,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::undocumented_unsafe_blocks
)]
mod interop {
    include!(concat!(env!("OUT_DIR"), "/interop.rs"));
}

// The layouts of the interop structs that only `c/d3d12.c`, `c/metal.m` and
// their Rust modules use, which no Linux build compiles. A header edit that
// moves, resizes or renames one of their fields fails here on every target,
// so it is made on purpose, together with those consumers.
const _: () = {
    use interop::{
        Alpha2D3D12CopyInfo as D3D12, Alpha2D3D12EncodeCaps as Caps,
        Alpha2D3D12EncodeCopy as Encode, Alpha2MetalFrameInfo as Metal,
    };
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<D3D12>() == 40 && align_of::<D3D12>() == 8);
    assert!(offset_of!(D3D12, resource) == 0);
    assert!(offset_of!(D3D12, fence) == 8);
    assert!(offset_of!(D3D12, fence_value) == 16);
    assert!(offset_of!(D3D12, width) == 24);
    assert!(offset_of!(D3D12, height) == 28);
    assert!(offset_of!(D3D12, dxgi_format) == 32);
    assert!(size_of::<Encode>() == 48 && align_of::<Encode>() == 8);
    assert!(offset_of!(Encode, luma) == 0);
    assert!(offset_of!(Encode, chroma) == 8);
    assert!(offset_of!(Encode, destination) == 16);
    assert!(offset_of!(Encode, frame_fence) == 24);
    assert!(offset_of!(Encode, frame_value) == 32);
    assert!(offset_of!(Encode, width) == 40);
    assert!(offset_of!(Encode, height) == 44);
    assert!(size_of::<Caps>() == 76 && align_of::<Caps>() == 4);
    assert!(offset_of!(Caps, supported) == 0);
    assert!(offset_of!(Caps, refused_check) == 4);
    assert!(offset_of!(Caps, refused_status) == 8);
    assert!(offset_of!(Caps, validation_flags) == 12);
    assert!(offset_of!(Caps, min_width) == 16);
    assert!(offset_of!(Caps, min_height) == 20);
    assert!(offset_of!(Caps, max_width) == 24);
    assert!(offset_of!(Caps, max_height) == 28);
    assert!(offset_of!(Caps, width_multiple) == 32);
    assert!(offset_of!(Caps, height_multiple) == 36);
    assert!(offset_of!(Caps, max_p_l0_references) == 40);
    assert!(offset_of!(Caps, max_b_l0_references) == 44);
    assert!(offset_of!(Caps, max_l1_references) == 48);
    assert!(offset_of!(Caps, rate_control_modes) == 52);
    assert!(offset_of!(Caps, refused_rate_control_modes) == 56);
    assert!(offset_of!(Caps, rate_control_status) == 60);
    assert!(size_of::<Metal>() == 56 && align_of::<Metal>() == 8);
    assert!(offset_of!(Metal, width) == 0);
    assert!(offset_of!(Metal, height) == 4);
    assert!(offset_of!(Metal, plane_width) == 8);
    assert!(offset_of!(Metal, plane_height) == 16);
    assert!(offset_of!(Metal, clean_x) == 24);
    assert!(offset_of!(Metal, clean_y) == 28);
    assert!(offset_of!(Metal, clean_width) == 32);
    assert!(offset_of!(Metal, clean_height) == 36);
    assert!(offset_of!(Metal, pixel_format) == 40);
    assert!(offset_of!(Metal, io_surface) == 48);
};
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
mod d3d12;
mod import;
mod lifetime;
#[cfg(all(target_os = "macos", feature = "hwdevice"))]
mod metal;
#[cfg(feature = "hwdevice")]
mod status;
mod surface_budget;
mod sync;
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
mod vulkan;

pub use context::NativeGpuIdentity;
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
pub use d3d12::{
    D3D12AdapterIdentity, D3D12CopyLease, D3D12EncodeCodec, D3D12EncodeCopier, D3D12EncodeExtent,
    ImportedD3D12Frame, SharedD3D12Context, SharedD3D12Error, d3d12_adapter_identity,
    gpu_wait_timeout as d3d12_wait_timeout,
};
pub use import::{ImportedPlaneDescriptor, NativePlaneLayout};
pub use lifetime::{NativeSurfaceLease, SurfaceIdentity};
#[cfg(all(target_os = "macos", feature = "hwdevice"))]
pub use metal::{
    ImportedMetalFrame, MetalEncoderPlanes, MetalImportLease, SharedMetalContext, SharedMetalError,
    gpu_wait_timeout as metal_wait_timeout,
};
pub use surface_budget::{SurfaceBudget, SurfaceBudgetError, SurfaceReservation, SurfaceUsage};
pub use sync::{ExternalSyncPoint, NativeImageState};
#[cfg(all(target_os = "linux", feature = "hwdevice"))]
pub use vulkan::{
    ImportTicketStats, ImportedVulkanFrame, SharedVulkanContext, SharedVulkanError,
    VideoEncodeExtent, VulkanEncodeCopier, VulkanImportLease, VulkanLayerVersion,
    probe_vulkan_submission, shared_device_index,
};

/// Sets how long the platform interop helpers wait on GPU work before they
/// give up with a timeout, instead of blocking forever on a hung device.
///
/// The default is 30 seconds. Destroying a helper whose work did not finish in
/// time leaks its GPU objects rather than freeing memory the device may still
/// use.
pub fn set_gpu_wait_timeout(timeout: std::time::Duration) {
    #[cfg(all(target_os = "linux", feature = "hwdevice"))]
    vulkan::set_wait_timeout(timeout);
    #[cfg(all(target_os = "windows", feature = "hwdevice"))]
    d3d12::set_wait_timeout(timeout);
    #[cfg(all(target_os = "macos", feature = "hwdevice"))]
    metal::set_wait_timeout(timeout);
    let _ = timeout;
}

/// `timeout` in whole milliseconds for a Win32 wait, which reads
/// `0xFFFFFFFF` (`INFINITE`) as no timeout at all: anything that long or
/// longer waits one millisecond less instead, so no setting makes a wait on a
/// hung device unbounded.
#[cfg_attr(
    not(all(target_os = "windows", feature = "hwdevice")),
    allow(dead_code)
)]
fn win32_wait_milliseconds(timeout: std::time::Duration) -> u32 {
    const INFINITE: u32 = u32::MAX;
    u32::try_from(timeout.as_millis())
        .unwrap_or(INFINITE)
        .min(INFINITE - 1)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn no_wait_timeout_reaches_win32_as_infinite() {
        let milliseconds =
            |value: u64| super::win32_wait_milliseconds(Duration::from_millis(value));
        assert_eq!(milliseconds(30_000), 30_000);
        assert_eq!(milliseconds(u64::from(u32::MAX) - 1), u32::MAX - 1);
        assert_eq!(milliseconds(u64::from(u32::MAX)), u32::MAX - 1);
        assert_eq!(milliseconds(u64::from(u32::MAX) + 1), u32::MAX - 1);
        assert_eq!(milliseconds(u64::MAX), u32::MAX - 1);
        assert_eq!(super::win32_wait_milliseconds(Duration::MAX), u32::MAX - 1);
        // Below a millisecond is no wait, as `Duration::as_millis` rounds it.
        assert_eq!(
            super::win32_wait_milliseconds(Duration::from_micros(999)),
            0
        );
    }
}
