//! What a project's composition uses cost to load and to plan an export.
//!
//! A composition compiles once however many clips show it, so listing the
//! effects a project draws and counting the decoder cursors an export reads
//! cost the clips plus the distinct compositions, never the clips times the
//! steps of what they show. This binary counts every allocation to see it.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use alpha2_eval::ActiveIndex;
use alpha2_model::Project;
use alpha2_time::{MediaTime, TimeRange};

/// Counts the bytes allocated and not yet freed, and the most there were.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's layout; the counters only observe. The
// default `alloc_zeroed` and `realloc` go through these two.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: the caller's contract for `GlobalAlloc::alloc` holds for `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `System` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Ticks each clip lasts.
const SPAN: i64 = 4_000;

/// `clips` clips one after another, each showing composition `a`: seven
/// merges of 32 groups of `b`, merged, where `b` merges seven merges of 31
/// reads of the video and a blur of one, so each clip draws about 52,000
/// steps, 48,608 of them reads. One more plain clip on a track of its own
/// follows them.
fn project(clips: usize) -> Project {
    let merge = |id: &str, inputs: &mut dyn Iterator<Item = String>| {
        let layers = inputs
            .map(|input| format!(r#"{{"input":"{input}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"id":"{id}","op":{{"type":"merge","layers":[{layers}]}}}}"#)
    };
    // Seven merges of 32 inputs, then the output merging the seven.
    let shape = |input: &dyn Fn(usize, usize) -> (String, String)| {
        let mut nodes = Vec::new();
        for group in 0..7 {
            let mut ids = Vec::new();
            for index in 0..32 {
                let (id, node) = input(group, index);
                nodes.push(node);
                ids.push(id);
            }
            nodes.push(merge(&format!("m{group}"), &mut ids.into_iter()));
        }
        nodes.push(merge("out", &mut (0..7).map(|group| format!("m{group}"))));
        nodes.join(",")
    };
    let b = shape(&|group, index| {
        let id = format!("r{group}_{index}");
        let node = if index == 0 {
            format!(
                r#"{{"id":"{id}","op":{{"type":"effect","input":"r{group}_1","effect":{{"id":"alpha2.effect.gaussian-blur","version":1}}}}}}"#
            )
        } else {
            format!(r#"{{"id":"{id}","op":{{"type":"media","source_id":"v"}}}}"#)
        };
        (id, node)
    });
    let a = shape(&|group, index| {
        let id = format!("g{group}_{index}");
        let node = format!(r#"{{"id":"{id}","op":{{"type":"group","composition":"b"}}}}"#);
        (id, node)
    });
    let mut shown = String::new();
    for clip in 0..clips {
        let start = SPAN * i64::try_from(clip).expect("index");
        let _ = write!(
            shown,
            r#"{}{{"id":"c{clip}","source_id":"v","composition":{{"id":"a"}},"timeline_range":{{"start":{start},"end":{end}}},"source_range":{{"start":0,"end":{SPAN}}}}}"#,
            if clip == 0 { "" } else { "," },
            end = start + SPAN,
        );
    }
    let last = SPAN * i64::try_from(clips).expect("count");
    Project::from_json(&format!(
        r#"{{"schema_version":1,"id":"q","name":"q",
          "settings":{{"width":64,"height":36,"frame_rate":{{"num":30,"den":1}},"sample_rate":48000}},
          "sources":[{{"id":"v","locator":"asset://v","kind":"video"}}],
          "compositions":[{{"id":"a","nodes":[{a}],"output":"out"}},{{"id":"b","nodes":[{b}],"output":"out"}}],
          "timeline":{{"tracks":[
            {{"id":"t","name":"T","kind":"video","clips":[{shown}]}},
            {{"id":"p","name":"P","kind":"video","clips":[{{"id":"plain","source_id":"v",
              "timeline_range":{{"start":{last},"end":{end}}},"source_range":{{"start":0,"end":{SPAN}}}}}]}}
          ]}}}}"#,
        end = last + SPAN,
    ))
    .expect("a valid project")
}

/// The most bytes `work` holds at once beyond what was live before it.
fn peak_bytes<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let value = work();
    (value, PEAK.load(Ordering::Relaxed) - before)
}

#[test]
fn listing_effects_and_reads_costs_the_clips_not_the_clips_times_their_compositions() {
    // What a host does with a project it loads, and before it exports a range.
    let plan = |clips: usize| {
        let project = project(clips);
        let last = SPAN * i64::try_from(clips).expect("count");
        let range = TimeRange::new(
            MediaTime::from_ticks(last),
            MediaTime::from_ticks(last + SPAN),
        )
        .expect("range");
        peak_bytes(|| {
            let index = ActiveIndex::build(&project).expect("index");
            let effects = index.effects().count();
            let reads = index.peak_source_reads(range, |_| true);
            let all = index.peak_source_reads(
                TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(last + SPAN)).expect("range"),
                |_| true,
            );
            (effects, reads, all)
        })
    };
    // Each clip draws 224 groups of `b`, each with seven blurs and 217 reads; only the plain clip
    // reads in the last range.
    let counts = (224 * 7, 1, 224 * 7 * 31);
    let (planned, one) = plan(1);
    assert_eq!(planned, counts);
    let (planned, many) = plan(2_000);
    assert_eq!(planned, counts);
    // A clip's own entry in the index is a few hundred bytes; each of its compositions' steps
    // would be tens more, 50,000 of them.
    let per_clip = many.saturating_sub(one) / 1_999;
    assert!(
        per_clip < 4_096,
        "each clip costs {per_clip} bytes ({one} bytes for one clip, {many} for 2000)"
    );
}
