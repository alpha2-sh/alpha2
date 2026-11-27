//! Heap held by animated text: refused rasters and what the tile cache keeps across frames.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{
    ClusterState, DEFAULT_TEXT_CACHE_BYTES, TextFill, TextFont, TextRasterError, TextRasterRequest,
    TextRasterizer, TextStroke,
};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
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
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: `pointer` came from `System` with this layout, and the caller's contract for
        // `GlobalAlloc::realloc` holds for `System`.
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            grow(size.saturating_sub(layout.size()));
            LIVE.fetch_sub(layout.size().saturating_sub(size), Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The tile cache's byte budget.
const TILE_CACHE_BYTES: usize = 64 << 20;

/// Serializes the tests, since the counters are process-wide.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn rasterizer() -> TextRasterizer {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../fonts/Alpha2Fixture.ttf"
    ))
    .expect("fixture font");
    TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: bytes.into(),
        face_index: 0,
    }])
    .expect("valid font")
}

fn request(text: &str, font_size: f32, strokes: Vec<TextStroke>) -> TextRasterRequest {
    TextRasterRequest {
        text: text.into(),
        font_size,
        max_width: 0.0,
        line_height: 1.2,
        align: TextAlign::Start,
        fill: TextFill::Solid([255, 255, 255, 255]),
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        strokes,
        split_background: false,
        background: None,
        font_family: None,
        letter_spacing: 0.0,
        font_weight: 400,
        clamp: None,
        spans: Vec::new(),
    }
}

fn states(
    rasterizer: &TextRasterizer,
    request: &TextRasterRequest,
    state: ClusterState,
) -> Vec<ClusterState> {
    let clusters = rasterizer.layout(request).expect("lays out").clusters.len();
    vec![state; clusters]
}

#[test]
fn a_caption_scaled_tenfold_past_the_tile_budget_is_refused_before_its_tiles_are_drawn() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rasterizer = rasterizer();
    let caption = request(
        "Twenty char caption.",
        300.0,
        vec![TextStroke {
            color: [0, 0, 0, 255],
            width: 2.0,
        }],
    );
    let scaled = states(
        &rasterizer,
        &caption,
        ClusterState {
            scale: [10.0, 10.0],
            ..ClusterState::IDENTITY
        },
    );
    let start = LIVE.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let result = rasterizer.rasterize_animated(&caption, &scaled);
    let peak = PEAK.load(Ordering::Relaxed) - start;
    assert_eq!(result.map(drop), Err(TextRasterError::TooLarge));
    assert!(peak < 16 << 20, "peak {peak} bytes");
}

#[test]
fn a_title_scaling_down_leaves_at_most_the_caches_held() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let start = LIVE.load(Ordering::Relaxed);
    let mut rasterizer = rasterizer();
    let title = request("Title", 270.0, Vec::new());
    for frame in 0..30_u16 {
        let scale = 10.0 - 9.0 * f32::from(frame) / 29.0;
        let frame_states = states(
            &rasterizer,
            &title,
            ClusterState {
                scale: [scale, scale],
                ..ClusterState::IDENTITY
            },
        );
        let request = TextRasterRequest {
            content_version: u64::from(frame) + 1,
            ..title.clone()
        };
        rasterizer
            .rasterize_animated(&request, &frame_states)
            .expect("draws");
    }
    let held = LIVE.load(Ordering::Relaxed) - start;
    assert!(
        held <= TILE_CACHE_BYTES + DEFAULT_TEXT_CACHE_BYTES + (4 << 20),
        "held {held} bytes"
    );
}

#[test]
fn a_rotating_title_never_holds_more_than_the_caches() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let start = LIVE.load(Ordering::Relaxed);
    let mut rasterizer = rasterizer();
    let title = request("Spin", 324.0, Vec::new());
    for frame in 0..600_u16 {
        let frame_states = states(
            &rasterizer,
            &title,
            ClusterState {
                rotation: f32::from(frame) * std::f32::consts::TAU / 97.0,
                ..ClusterState::IDENTITY
            },
        );
        let request = TextRasterRequest {
            content_version: u64::from(frame) + 1,
            ..title.clone()
        };
        rasterizer
            .rasterize_animated(&request, &frame_states)
            .expect("draws");
        let held = LIVE.load(Ordering::Relaxed) - start;
        assert!(
            held <= TILE_CACHE_BYTES + DEFAULT_TEXT_CACHE_BYTES + (4 << 20),
            "frame {frame} held {held} bytes"
        );
    }
}

#[test]
fn a_font_strike_past_the_pixel_budget_is_drawn_without_decoding_it() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../fonts/Alpha2HostileStrike.ttf"
    ))
    .expect("fixture font");
    let mut rasterizer = TextRasterizer::new(vec![TextFont {
        family: "Strike".into(),
        bytes: bytes.into(),
        face_index: 0,
    }])
    .expect("the strike font registers");
    let start = LIVE.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    rasterizer
        .rasterize(&request("A", 40.0, Vec::new()))
        .expect("draws");
    let peak = PEAK.load(Ordering::Relaxed) - start;
    assert!(peak < 16 << 20, "peak {peak} bytes");
}
