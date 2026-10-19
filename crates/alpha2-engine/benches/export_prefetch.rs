//! Standalone benchmark for export decode lookahead.
//!
//! `Exporter::prefetch_needs` evaluates every frame of its lookahead window on
//! each call, so a host that asks once per exported frame pays the window size
//! in evaluations per frame. Prints the time per call and per frame of window.
//! Times are recorded, never gated.

use std::{hint::black_box, time::Instant};

use alpha2_engine::{EngineProject, Exporter, QualityProfile};
use alpha2_time::{FrameRate, MediaTime, TimeRange};

#[path = "../../alpha2-eval/benches/support/timeline.rs"]
#[allow(dead_code)]
mod timeline;

const TRACKS: usize = 32;
const CALLS: usize = 200;

fn main() {
    for load in [timeline::Load::Plain, timeline::Load::Effects] {
        let snapshot = EngineProject::new(timeline::project(TRACKS, load)).expect("project");
        let range =
            TimeRange::new(MediaTime::ZERO, MediaTime(timeline::SPAN_TICKS)).expect("range");
        let rate = FrameRate::new(30_000, 1_001).expect("rate");
        let exporter = Exporter::new(
            &snapshot,
            range,
            rate,
            QualityProfile::export(),
            alpha2_media::VideoCodec::H264,
        )
        .expect("exporter");
        for window in [1_u32, 8, 120] {
            let frames = exporter
                .prefetch_needs(&snapshot, window)
                .expect("prefetch")
                .len();
            let started = Instant::now();
            for _ in 0..CALLS {
                black_box(
                    exporter
                        .prefetch_needs(&snapshot, window)
                        .expect("prefetch"),
                );
            }
            let per_call = started.elapsed().as_nanos() / u128::try_from(CALLS).expect("small");
            println!(
                "export_prefetch load={} tracks={TRACKS} window={window} frames={frames} \
                 ns_per_call={per_call} ns_per_frame={}",
                load.name(),
                per_call / u128::try_from(frames).expect("small"),
            );
        }
    }
}
