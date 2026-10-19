//! Standalone benchmark for evaluating timelines that carry effects,
//! keyframes, and transitions, not only bare clips.
//!
//! Prints `ns_per_evaluation` per case. Times are recorded, never gated; the
//! allocation budget tests gate the deterministic side of the same work.

use std::{hint::black_box, time::Instant};

use alpha2_eval::{ActiveIndex, evaluate};
use alpha2_time::MediaTime;

#[path = "support/timeline.rs"]
#[allow(dead_code)]
mod timeline;

const TRACKS: usize = 32;
const ITERATIONS: usize = 20_000;

fn main() {
    for load in timeline::Load::ALL {
        let project = timeline::project(TRACKS, load);
        let index = ActiveIndex::build(&project).expect("benchmark project is valid");
        let started = Instant::now();
        for iteration in 0..ITERATIONS {
            let ticks = i64::try_from(iteration).expect("small") * 7 % timeline::SPAN_TICKS;
            let frame = evaluate(black_box(&project), black_box(&index), MediaTime(ticks));
            black_box(frame.expect("evaluation"));
        }
        let nanos = started.elapsed().as_nanos() / u128::try_from(ITERATIONS).expect("non-zero");
        println!(
            "timeline_features load={} tracks={TRACKS} iterations={ITERATIONS} \
             ns_per_evaluation={nanos}",
            load.name()
        );
    }
}
