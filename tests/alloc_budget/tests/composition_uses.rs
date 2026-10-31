//! Loading and export planning cost the clips plus the distinct compositions, never clips times
//! composition steps.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use alpha2_eval::ActiveIndex;
use alpha2_model::Project;
use alpha2_time::{MediaTime, TimeRange};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's layout; the counters only observe.
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

const SPAN: i64 = 4_000;

/// `clips` clips of a composition of about 52,000 steps, then one plain clip on its own track.
fn project(clips: usize) -> Project {
    let merge = |id: &str, inputs: &mut dyn Iterator<Item = String>| {
        let layers = inputs
            .map(|input| format!(r#"{{"input":"{input}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"id":"{id}","op":{{"type":"merge","layers":[{layers}]}}}}"#)
    };
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

fn peak_bytes<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let value = work();
    (value, PEAK.load(Ordering::Relaxed) - before)
}

#[test]
fn listing_effects_and_reads_costs_the_clips_not_the_clips_times_their_compositions() {
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
    let counts = (224 * 7, 1, 224 * 7 * 31);
    let (planned, one) = plan(1);
    assert_eq!(planned, counts);
    let (planned, many) = plan(2_000);
    assert_eq!(planned, counts);
    // A clip entry is a few hundred bytes; per-step entries would be tens of bytes times 50,000.
    let per_clip = many.saturating_sub(one) / 1_999;
    assert!(
        per_clip < 4_096,
        "each clip costs {per_clip} bytes ({one} bytes for one clip, {many} for 2000)"
    );
}
