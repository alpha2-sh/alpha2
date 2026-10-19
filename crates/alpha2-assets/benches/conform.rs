//! Standalone benchmark for conform over many sources. It prints the time a
//! conform takes; it never fails on it.

use std::{hint::black_box, time::Instant};

use alpha2_assets::{MediaPurpose, conform};

#[path = "support/sources.rs"]
mod sources;

fn main() {
    for count in [1_000, 4_000, 10_000] {
        let (project, catalog) = sources::many_sources(count);
        let iterations = 20;
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(conform(
                black_box(&project),
                black_box(&catalog),
                MediaPurpose::Preview,
            ));
        }
        let micros = started.elapsed().as_micros() / iterations;
        println!("conform sources={count} us_per_conform={micros}");
    }
}
