//! Peak heap held while a project document over a count limit, or a hostile subtitle file, is
//! read.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use alpha2_model::{
    MAX_CAPTION_FILE_BYTES, MAX_PROJECT_JSON_BYTES, Project, ProjectLoadError, parse_srt,
    parse_webvtt,
};

struct Counting;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn grow(by: usize) {
    let _ = LIVE.try_with(|live| {
        live.set(live.get() + by);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
    });
}

fn shrink(by: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(by)));
}

// SAFETY: every call forwards to `System` with the caller's layout; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `GlobalAlloc::alloc` holds for `System`.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grow(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `System` with this layout.
        unsafe { System.dealloc(pointer, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: `pointer` came from `System` with this layout, and the caller's contract for
        // `GlobalAlloc::realloc` holds for `System`.
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            grow(size.saturating_sub(layout.size()));
            shrink(layout.size().saturating_sub(size));
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The result of `call` and the most heap it held at once beyond what was live before it.
fn peak<T>(call: impl FnOnce() -> T) -> (T, usize) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let result = call();
    (result, PEAK.with(Cell::get) - start)
}

const HEAD: &str = r#"{"schema_version":1,"id":"p","name":"p","settings":{"width":1920,"height":1080,"frame_rate":{"num":30,"den":1},"sample_rate":48000},"sources":[{"id":"v","locator":"v.mp4","kind":"video"}]"#;

/// A document of about `bytes` whose one sequence between `open` and `close` repeats `item`.
fn filled(open: &str, item: &str, close: &str, bytes: usize) -> String {
    let mut json = String::with_capacity(bytes);
    json.push_str(HEAD);
    json.push_str(open);
    let mut first = true;
    while json.len() + item.len() + 1 + close.len() <= bytes {
        if !first {
            json.push(',');
        }
        json.push_str(item);
        first = false;
    }
    json.push_str(close);
    json
}

fn refused_by_the_parser(json: &str) -> usize {
    let (loaded, held) = peak(|| Project::from_json(json));
    println!("held {held} bytes beyond the input");
    assert!(
        matches!(loaded, Err(ProjectLoadError::Json(_))),
        "{:?}",
        loaded.err()
    );
    held
}

#[test]
fn a_full_document_of_empty_channel_names_is_refused_within_a_fixed_budget() {
    let json = filled(
        r#","data":[{"id":"d","anchor":{"type":"timeline"},"kind":"scalars","interpolation":"hold","channels":["#,
        r#""""#,
        r#"],"times":[],"values":[]}],"timeline":{"tracks":[]}}"#,
        MAX_PROJECT_JSON_BYTES,
    );
    let held = refused_by_the_parser(&json);
    assert!(held < 16 << 20, "held {held} bytes beyond the input");
}

#[test]
fn a_full_document_of_merge_layers_is_refused_within_a_fixed_budget() {
    let json = filled(
        r#","compositions":[{"id":"comp","nodes":[{"id":"m","op":{"type":"merge","layers":["#,
        "{}",
        r#"]}}],"output":"m"}],"timeline":{"tracks":[]}}"#,
        MAX_PROJECT_JSON_BYTES,
    );
    // An internally tagged node is buffered whole before its variant reads the layers: about
    // 48 bytes per layer remain, a third of reading them as layers.
    let held = refused_by_the_parser(&json);
    assert!(held < 3 << 29, "held {held} bytes beyond the input");
}

#[test]
fn a_subtitle_file_of_line_feeds_is_read_within_a_fixed_budget() {
    let feeds = "\n".repeat(MAX_CAPTION_FILE_BYTES);
    let vtt = format!("WEBVTT{}", &feeds[6..]);
    let (srt, held_srt) = peak(|| parse_srt(&feeds).expect("an empty SRT file"));
    let (vtt, held_vtt) = peak(|| parse_webvtt(&vtt).expect("an empty WebVTT file"));
    assert!(srt.cues.is_empty() && vtt.cues.is_empty());
    assert!(held_srt < 1 << 20, "held {held_srt} bytes for SRT");
    assert!(held_vtt < 1 << 20, "held {held_vtt} bytes for WebVTT");
}
