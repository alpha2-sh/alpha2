//! What preparing a frame of more than eight bits per sample costs the CPU in
//! memory on a device without 16-bit normalized textures.
//!
//! Browser WebGPU never has them (wgpu's WebGPU backend reports no such
//! feature), so every 10- or 12-bit HDR frame a browser decodes takes this
//! path. Converting its samples to half floats on the CPU would allocate a
//! frame-sized buffer per plane per frame and cost about 30 ms at 4K, so the
//! containers go to the GPU as they are.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use alpha2_gpu::GpuContext;
use alpha2_media::{ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, YuvFormat};
use alpha2_render::{ColorPolicy, RenderSize, Renderer, SourcePlane, SourcePresentation, YuvFrame};

struct Counting;

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every call is forwarded to the system allocator unchanged; the
// thread-local counters are only read and written on their own thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn record(size: usize) {
    // `try_with` because allocations also happen while a thread's locals are torn down.
    let _ = MEASURING.try_with(|measuring| {
        if measuring.get() {
            BYTES.with(|bytes| bytes.set(bytes.get() + size as u64));
        }
    });
}

/// The bytes `work` allocates on this thread.
fn allocated<T>(work: impl FnOnce() -> T) -> (T, u64) {
    BYTES.with(|bytes| bytes.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    let result = work();
    MEASURING.with(|measuring| measuring.set(false));
    (result, BYTES.with(Cell::get))
}

/// A device opened without 16-bit normalized textures, as browser WebGPU is,
/// on the adapter alpha2 itself would pick.
fn device_without_16_bit_norm() -> GpuContext {
    let gpu = pollster::block_on(GpuContext::request_high_performance_without(
        wgpu::Features::TEXTURE_FORMAT_16BIT_NORM,
    ))
    .expect("these budgets need a GPU device");
    assert!(
        !gpu.device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM),
        "the device withholds 16-bit normalized textures"
    );
    gpu
}

#[test]
fn a_ten_bit_frame_reaches_the_gpu_without_a_cpu_copy_of_its_samples() {
    let renderer = Renderer::new(device_without_16_bit_norm());
    let size = RenderSize::new(1920, 1080).expect("size");
    let luma = vec![0_u8; 1920 * 1080 * 2];
    let chroma = vec![0_u8; 960 * 540 * 2];
    let held = (luma.len() + 2 * chroma.len()) as u64;
    let frame = YuvFrame {
        format: YuvFormat::planar(1, 1, 10),
        planes: [
            SourcePlane {
                bytes: &luma,
                stride: 1920 * 2,
            },
            SourcePlane {
                bytes: &chroma,
                stride: 960 * 2,
            },
            SourcePlane {
                bytes: &chroma,
                stride: 960 * 2,
            },
        ],
        alpha_plane: None,
        size,
        alpha: 1.0,
        color: Some(ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            ..ColorMetadata::default()
        }),
    };
    let present = || {
        renderer
            .prepare_yuv_presented(
                frame,
                SourcePresentation::identity(size),
                ColorPolicy::RejectHdr,
            )
            .expect("10-bit 4:2:0 presents")
            .0
    };
    // The first frame also compiles the conversion pipeline.
    let (_texture, pending) = present().into_parts();
    assert!(renderer.submit_pending_source(&pending.expect("pending")));
    for index in 0..3 {
        let (prepared, bytes) = allocated(present);
        let (_texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending")));
        // Bookkeeping and the driver's own staging are small; a copy of the samples is the frame's
        // size.
        assert!(
            bytes < held / 8,
            "frame {index}: preparing a {held}-byte frame allocated {bytes} bytes"
        );
    }
}
