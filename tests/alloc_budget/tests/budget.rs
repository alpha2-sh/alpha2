//! Allocation counts of the per-frame and per-block hot paths.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use alpha2_audio::{LoudnessMeter, TransportState};
use alpha2_engine::{AudioRenderer, EngineProject};
use alpha2_eval::{ActiveIndex, evaluate, frame_needs};
use alpha2_time::MediaTime;

#[path = "../../../crates/alpha2-eval/benches/support/timeline.rs"]
#[allow(dead_code)]
mod timeline;

#[path = "../../../crates/alpha2-engine/benches/support/audio.rs"]
#[allow(dead_code)]
mod audio;

/// Counts allocations made on a thread while it is measuring.
struct Counting;

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to the system allocator unchanged; the thread-local counters are
// const-initialized, so touching them never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A growth counts the size it grows to, not the size it leaves.
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
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
            BYTES.with(|bytes| bytes.set(bytes.get() + size as u64));
        }
    });
}

/// What `calls` calls of some work allocated on this thread, in all.
struct Measured {
    calls: u64,
    allocations: u64,
    bytes: u64,
}

impl Measured {
    /// Whether the calls made at most `budget` allocations each: the total
    /// is compared, so one allocation over all the calls already exceeds a
    /// budget of zero.
    const fn within(&self, budget: u64) -> bool {
        self.allocations <= budget.saturating_mul(self.calls)
    }
}

impl std::fmt::Display for Measured {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "allocations={} bytes={} over {} calls",
            self.allocations, self.bytes, self.calls
        )
    }
}

/// Allocations and bytes `work` makes on this thread over `calls` calls.
fn measure(calls: u64, mut work: impl FnMut()) -> Measured {
    ALLOCATIONS.with(|count| count.set(0));
    BYTES.with(|bytes| bytes.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    for _ in 0..calls {
        work();
    }
    MEASURING.with(|measuring| measuring.set(false));
    Measured {
        calls,
        allocations: ALLOCATIONS.with(Cell::get),
        bytes: BYTES.with(Cell::get),
    }
}

// Evaluation allocates exact-sized scratch and output lists; audio blocks must not allocate.
// Budgets are per call against the total, so averaging over calls cannot hide allocations.
const EVALUATE_PLAIN: u64 = 2;
const EVALUATE_EFFECTS: u64 = 2;
const EVALUATE_KEYFRAMES: u64 = 2;
const EVALUATE_TRANSITIONS: u64 = 3;
// Listing a frame's needs adds its list and, for each need it returns, the two IDs it owns. A clip
// reading a source frame another clip already asked for costs nothing.
const FRAME_NEEDS_LIST: u64 = 1;
const FRAME_NEEDS_PER_NEED: u64 = 2;
const AUDIO_BLOCK_16_CLIPS: u64 = 0;
const AUDIO_BLOCK_16_CLIPS_EFFECTS: u64 = 0;
const AUDIO_BLOCK_METERED: u64 = 0;
const LOUDNESS_METER_BLOCK: u64 = 0;

#[test]
fn one_timeline_evaluation_stays_within_its_allocation_budget() {
    for (load, budget) in [
        (timeline::Load::Plain, EVALUATE_PLAIN),
        (timeline::Load::Effects, EVALUATE_EFFECTS),
        // Parameters are sampled when a renderer reads them, so keyframing them costs evaluation
        // nothing.
        (timeline::Load::AnimatedEffects, EVALUATE_EFFECTS),
        (timeline::Load::Keyframes, EVALUATE_KEYFRAMES),
        (timeline::Load::Transitions, EVALUATE_TRANSITIONS),
    ] {
        let project = timeline::project(32, load);
        let index = ActiveIndex::build(&project).expect("valid project");
        // Mid-transition, where every load has all 32 tracks active.
        let time = MediaTime(timeline::SPAN_TICKS / 2);
        evaluate(&project, &index, time).expect("warm-up");
        let measured = measure(100, || {
            std::hint::black_box(evaluate(&project, &index, time).expect("evaluation"));
        });
        println!("evaluate load={} tracks=32 {measured}", load.name());
        assert!(
            measured.within(budget),
            "evaluate ({}) made {measured}, budget {budget} each",
            load.name()
        );
    }
}

#[test]
fn listing_a_frames_needs_allocates_only_what_it_returns() {
    for (load, evaluation) in [
        (timeline::Load::Plain, EVALUATE_PLAIN),
        (timeline::Load::Effects, EVALUATE_EFFECTS),
        (timeline::Load::Transitions, EVALUATE_TRANSITIONS),
    ] {
        // Every track reads one source: 32 readers of one frame, or of two mid-transition.
        let project = timeline::project(32, load);
        let index = ActiveIndex::build(&project).expect("valid project");
        let time = MediaTime(timeline::SPAN_TICKS / 2);
        let needs = frame_needs(&project, &index, time).expect("needs");
        let budget = evaluation
            + FRAME_NEEDS_LIST
            + FRAME_NEEDS_PER_NEED * u64::try_from(needs.len()).expect("small");
        let measured = measure(100, || {
            std::hint::black_box(frame_needs(&project, &index, time).expect("needs"));
        });
        println!(
            "frame_needs load={} tracks=32 needs={} {measured}",
            load.name(),
            needs.len()
        );
        assert!(
            measured.within(budget),
            "frame_needs ({}, {} needs) made {measured}, budget {budget} each",
            load.name(),
            needs.len()
        );
    }
}

#[test]
fn one_audio_block_stays_within_its_allocation_budget() {
    for (effects, budget) in [
        (false, AUDIO_BLOCK_16_CLIPS),
        (true, AUDIO_BLOCK_16_CLIPS_EFFECTS),
    ] {
        let snapshot = EngineProject::new(audio::project(16, 48_000, effects)).expect("project");
        let provider = audio::Sine::new(48_000);
        let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
        let mut start = 0_i64;
        let mut block = || {
            renderer
                .render(&snapshot, start, 4_096, &provider, TransportState::Playing)
                .expect("render");
            start += 4_096;
        };
        block();
        let measured = measure(16, &mut block);
        println!("audio_render clips=16 effects={effects} block=4096 {measured}");
        assert!(
            measured.within(budget),
            "16-clip blocks (effects {effects}) made {measured}, budget {budget} each"
        );

        // A seek replays the graph's settling history block by block, and a host may change its
        // block size: neither needs new memory.
        let mut seek = 0_i64;
        let measured = measure(4, || {
            seek += 48_000;
            for (start, frames) in [(seek, 4_096), (seek + 4_096, 128), (seek + 4_224, 1_000)] {
                renderer
                    .render(&snapshot, start, frames, &provider, TransportState::Playing)
                    .expect("render");
            }
        });
        println!("audio_render clips=16 effects={effects} seek_and_resize {measured}");
        assert!(
            measured.within(budget),
            "seeks (effects {effects}) made {measured}, budget {budget} each"
        );
    }
}

#[test]
fn a_metered_audio_block_stays_within_its_allocation_budget() {
    // An export meters the mix before the limiter and what it encodes, each block, in memory taken
    // when metering starts, so a long mix does not grow them.
    let snapshot = EngineProject::new(audio::project(16, 48_000, true)).expect("project");
    let provider = audio::Sine::new(48_000);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    renderer.set_loudness_metering(true).expect("meter");
    renderer
        .set_output_loudness_metering(true)
        .expect("output meter");
    let mut start = 0_i64;
    let mut block = || {
        renderer
            .render(&snapshot, start, 4_096, &provider, TransportState::Playing)
            .expect("render");
        start += 4_096;
    };
    block();
    // 64 blocks are 5.5 s: 54 gating blocks and 25 short-term windows a meter keeps.
    let measured = measure(64, &mut block);
    println!("audio_render clips=16 effects=true metered block=4096 {measured}");
    assert!(
        measured.within(AUDIO_BLOCK_METERED),
        "metered blocks made {measured}, budget {AUDIO_BLOCK_METERED} each"
    );
    assert!(
        renderer
            .loudness_report()
            .is_some_and(|report| report.integrated_lufs.is_some())
    );
}

#[test]
fn a_loudness_meter_measures_ten_minutes_without_allocating() {
    // Ten minutes at 8 kHz, mono, in 1,024-sample blocks: 6,000 gating blocks and 5,971 short-term
    // windows, which a meter keeping each one grows a list for, again and again.
    let mut meter = LoudnessMeter::new(8_000, 1).expect("meter");
    let tone = (0..1_024_u16)
        .map(|index| 0.1 * libm::sinf(0.785 * f32::from(index)))
        .collect::<Vec<_>>();
    let measured = measure(4_688, || meter.push(&[&tone]).expect("block"));
    println!("loudness_meter rate=8000 channels=1 block=1024 600s {measured}");
    assert!(
        measured.within(LOUDNESS_METER_BLOCK),
        "a meter made {measured} over ten minutes, budget {LOUDNESS_METER_BLOCK} each"
    );
    let report = meter.report();
    assert!(report.integrated_lufs.is_some() && report.loudness_range_lu.is_some());
}

#[test]
fn a_reallocation_counts_the_bytes_it_grows_to() {
    let mut grown: Vec<u8> = Vec::with_capacity(16);
    let measured = measure(1, || grown.reserve_exact(4_096));
    assert_eq!(measured.allocations, 1, "{measured}");
    assert!(measured.bytes >= 4_096, "{measured}");
}
