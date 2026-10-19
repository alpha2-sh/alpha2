//! What converting a frame the renderer cannot sample costs in memory.
//!
//! A frame that reaches the renderer as RGBA, or as 4:4:4 planes, is
//! converted into one buffer of its own; allocating a second and copying the
//! first into it would double the memory traffic of every such frame.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;

use alpha2_media::{CancelToken, DecodeEvent, DecodedVideoFrame, Decoder};
use alpha2_native::NativeDecoder;

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

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
}

/// The next video frame, and the bytes this thread allocated decoding and
/// converting it.
fn next_frame(decoder: &mut NativeDecoder) -> (DecodedVideoFrame, u64) {
    let cancel = CancelToken::default();
    loop {
        BYTES.with(|bytes| bytes.set(0));
        MEASURING.with(|measuring| measuring.set(true));
        let event = decoder.decode_next(&cancel);
        MEASURING.with(|measuring| measuring.set(false));
        match event.expect("decode") {
            DecodeEvent::Video(frame) => return (frame, BYTES.with(Cell::get)),
            DecodeEvent::EndOfStream => panic!("no video frame"),
            _ => {}
        }
    }
}

#[test]
fn a_converted_frame_is_written_once_into_the_buffer_it_is_returned_in() {
    // RGBA (PNG-coded RGB, SDR), then 4:4:4 planes with alpha (ProRes 4444, PQ).
    for name in ["parity-rgb.mov", "prores-pq-alpha.mov"] {
        let mut decoder = NativeDecoder::open(corpus(name)).expect(name);
        for index in 0..3 {
            let (frame, allocated) = next_frame(&mut decoder);
            let held = frame
                .planes
                .iter()
                .map(|plane| plane.bytes.len() as u64)
                .sum::<u64>();
            assert!(held > 0, "{name}");
            // The returned planes, plus room for bookkeeping and the demuxed packet; a second
            // frame-sized buffer is far over.
            assert!(
                allocated < held + held / 2,
                "{name} frame {index}: allocated {allocated} bytes for a {held}-byte frame"
            );
        }
    }
}
