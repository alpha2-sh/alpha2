//! What uploading a linear-light (`OpenEXR`) frame costs the CPU in memory.
//!
//! The browser hands every `OpenEXR` frame to `upload_linear_rgba16f` as half
//! floats. Converting them to bytes first would allocate and fill a second
//! frame-sized buffer, about 13 ms a 4K frame in wasm, only for
//! `write_texture` to copy the bytes again.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use alpha2_gpu::GpuContext;
use alpha2_render::{RenderSize, Renderer};

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

#[test]
fn a_linear_frame_reaches_the_gpu_without_a_cpu_copy_of_its_halves() {
    let gpu = pollster::block_on(GpuContext::request_product())
        .expect("these budgets need a GPU adapter");
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(1920, 1080).expect("size");
    let halves = vec![0x3c00_u16; 1920 * 1080 * 4];
    let held = (halves.len() * 2) as u64;
    // The first upload also creates the pooled texture.
    drop(
        renderer
            .upload_linear_rgba16f(size, &halves)
            .expect("uploads"),
    );
    for index in 0..3 {
        let (texture, bytes) = allocated(|| renderer.upload_linear_rgba16f(size, &halves));
        drop(texture.expect("uploads"));
        // Bookkeeping and the driver's own staging are small; a copy of the halves is the frame's
        // size.
        assert!(
            bytes < held / 8,
            "frame {index}: uploading a {held}-byte frame allocated {bytes} bytes"
        );
    }
}
