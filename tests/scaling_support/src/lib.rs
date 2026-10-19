//! Complexity checks for tests: a step that is meant to take time in
//! proportion to its input (or to its input times its logarithm) is run at
//! one size and at eight times it, and must take less than 22 times the work:
//! its work must grow slower than the input to the power 1.5.
//!
//! A bound on elapsed wall-clock time fails on a busy machine without any
//! regression, and passes a quadratic step on a fast one. A ratio of two
//! sizes does not depend on the machine's speed or the build's optimisation,
//! and the work is the CPU work of the testing thread
//! ([`alpha2_os::thread_cpu_work`]), not the time it waited for a CPU, so
//! other tests and processes running at once change it little. Linear work
//! grows 8 times, `n log n` work with what caches and allocators add about 11
//! to 15 times, and quadratic work 64 times. A quadratic part is caught when
//! it is most of the work at the large size (about three quarters or more):
//! the regressions these tests guard against, a search per item where a
//! lookup belongs, are that at the sizes a document can reach.
//!
//! The inputs are made and run in batches that hold as many items at both
//! sizes, not all at once: a cheap step runs over gigabytes of inputs in a
//! reading, and holding them together pushes a machine with less memory into
//! compressing or swapping pages, whose undoing the testing thread pays for
//! at the large size only.

use std::time::{Duration, Instant};

/// How much larger the large input is than the small one.
pub const GROWTH: usize = 8;

/// The large input's work must be less than this multiple of the small
/// one's: [`GROWTH`] to the power 1.5, 22.6, rounded down.
pub const BOUND: u64 = 22;

/// How long the small input's runs take together at least, so a reading is
/// many times the resolution of the clock and of the scheduler.
const LEAST_RUN: Duration = Duration::from_millis(20);

/// Readings of each size; the least is kept, as the one least disturbed.
const READINGS: usize = 3;

/// About how many items a batch of large inputs holds together, or one
/// large input when it holds more; a batch of small inputs holds as many.
const BATCH_ITEMS: usize = 1 << 20;

/// The calling thread's work so far: its CPU work where the operating system
/// reports it, else the time since `origin`.
fn work(origin: Instant) -> u64 {
    alpha2_os::thread_cpu_work()
        .unwrap_or_else(|| u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

/// The work of `run` over `repeats` inputs of `size`, made by `setup` and
/// dropped `batch` at a time, the making and dropping outside the reading.
fn measure<T>(
    size: usize,
    repeats: usize,
    batch: usize,
    origin: Instant,
    setup: &mut impl FnMut(usize) -> T,
    run: &mut impl FnMut(&mut T),
) -> (u64, Duration) {
    let (mut work_done, mut elapsed) = (0, Duration::ZERO);
    let mut left = repeats;
    while left > 0 {
        let count = left.min(batch);
        let mut inputs: Vec<T> = (0..count).map(|_| setup(size)).collect();
        let (started, wall) = (work(origin), Instant::now());
        for input in &mut inputs {
            run(input);
        }
        work_done += work(origin).saturating_sub(started);
        elapsed += wall.elapsed();
        drop(inputs);
        left -= count;
    }
    (work_done, elapsed)
}

/// Asserts that `run` over an input `setup` makes of [`GROWTH`] times `small`
/// takes less than [`BOUND`] times the work it takes over one of `small`:
/// that its work grows slower than the size to the power 1.5, as linear and
/// `n log n` work does and quadratic work does not.
///
/// `setup` makes an input of a size and is not measured; `run` is. Each run
/// gets an input of its own, so a step that caches what it computed is
/// measured computing it; the inputs are made a batch at a time, so no more
/// than about a million items' worth are held at once. The small input is
/// run as many times as takes [`LEAST_RUN`] together, and the large one as
/// many times.
///
/// # Panics
/// When the large input takes [`BOUND`] times the small one's work or more,
/// naming `what`.
pub fn assert_linear<T>(
    what: &str,
    small: usize,
    setup: impl FnMut(usize) -> T,
    run: impl FnMut(&mut T),
) {
    assert_growth_below(what, small, BOUND, setup, run);
}

/// [`assert_linear`] with another bound on the ratio of the large input's
/// work to the small one's: for a step whose work must not grow with the
/// size at all, say, a bound of a few times.
///
/// # Panics
/// When the large input takes `bound` times the small one's work or more,
/// naming `what`.
pub fn assert_growth_below<T>(
    what: &str,
    small: usize,
    bound: u64,
    mut setup: impl FnMut(usize) -> T,
    mut run: impl FnMut(&mut T),
) {
    let origin = Instant::now();
    let large = small * GROWTH;
    let large_batch = (BATCH_ITEMS / large).max(1);
    let small_batch = large_batch * GROWTH;
    let mut repeats = 1;
    // The run that reaches the least time is the first reading of the small size.
    let mut least_small = loop {
        let (work, wall) = measure(small, repeats, small_batch, origin, &mut setup, &mut run);
        if wall >= LEAST_RUN || repeats >= 1 << 16 {
            break work;
        }
        repeats *= 2;
    };
    let mut least_large = u64::MAX;
    // Alternated, so a slow stretch of the machine falls on both sizes.
    for reading in 0..READINGS {
        least_large =
            least_large.min(measure(large, repeats, large_batch, origin, &mut setup, &mut run).0);
        if reading + 1 < READINGS {
            least_small = least_small
                .min(measure(small, repeats, small_batch, origin, &mut setup, &mut run).0);
        }
    }
    // Precision does not matter in a ratio for a message.
    #[allow(clippy::cast_precision_loss)]
    let ratio = least_large as f64 / least_small.max(1) as f64;
    // Shown with `--nocapture`, and with a failure.
    eprintln!("{what}: {GROWTH}x the input took {ratio:.2}x the work ({repeats} runs each)");
    assert!(
        least_large < least_small.saturating_mul(bound),
        "{what}: {GROWTH}x the input ({small} -> {large}, {repeats} runs each) took {ratio:.1}x the \
         work ({least_small} -> {least_large}); the bound is {bound}x (linear work takes about \
         {GROWTH}x, quadratic {}x)",
        GROWTH * GROWTH
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum_pairs(values: &[u64], quadratic: bool) {
        let mut total = 0_u64;
        if quadratic {
            for a in values {
                for b in values {
                    total = total.wrapping_add(a ^ b);
                }
            }
        } else {
            for a in values {
                total = total.wrapping_add(*a);
            }
        }
        std::hint::black_box(total);
    }

    #[test]
    fn linear_work_passes() {
        assert_linear(
            "a sum",
            200_000,
            |size| (0..size as u64).collect::<Vec<_>>(),
            |values| sum_pairs(values, false),
        );
    }

    #[test]
    #[should_panic(expected = "a sum over every pair")]
    fn quadratic_work_fails() {
        assert_linear(
            "a sum over every pair",
            500,
            |size| (0..size as u64).collect::<Vec<_>>(),
            |values| sum_pairs(values, true),
        );
    }
}
