//! Complexity checks: a step is run at one size and at eight times it, and its work must grow
//! less than 22 times (below the input to the power 1.5).
//!
//! A ratio of two sizes does not depend on machine speed, unlike a wall-clock bound. Work is the
//! testing thread's CPU time ([`alpha2_os::thread_cpu_work`]), so concurrent load changes it
//! little. Linear work grows 8x, `n log n` about 11-15x, quadratic 64x.
//!
//! Inputs are made in batches rather than all at once, so a cheap step over gigabytes of input
//! does not push the machine into swapping that only the large size pays for.

use std::time::{Duration, Instant};

/// Ratio of the large input size to the small one.
pub const GROWTH: usize = 8;

/// Maximum work ratio: [`GROWTH`] to the power 1.5, rounded down.
pub const BOUND: u64 = 22;

/// Minimum total run time of the small size, well above clock and scheduler resolution.
const LEAST_RUN: Duration = Duration::from_millis(20);

// Keep the least-disturbed of three readings.
const READINGS: usize = 3;

/// Approximate items per batch of inputs, at either size.
const BATCH_ITEMS: usize = 1 << 20;

/// Setup time after which calibration stops growing the run count, once runs reach
/// [`LEAST_MEASURED`], so a test is not spent mostly making inputs.
const MOST_SETUP: Duration = Duration::from_secs(1);

const LEAST_MEASURED: Duration = Duration::from_millis(5);

/// Thread CPU work where the OS reports it, else wall time since `origin`.
fn work(origin: Instant) -> u64 {
    alpha2_os::thread_cpu_work()
        .unwrap_or_else(|| u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

#[derive(Clone, Copy, Default)]
struct Reading {
    work: u64,
    measured: Duration,
    setup: Duration,
}

#[derive(Clone, Copy)]
enum Inputs {
    /// Each run gets its own input.
    Own,
    /// Read-only inputs made once per reading: [`GROWTH`] small ones read in turn and one large,
    /// so the small size cannot run from a cache the large one overflows.
    Shared,
}

/// Measures `repeats` runs, excluding input setup and drop from the reading.
fn measure<T>(
    size: usize,
    repeats: usize,
    batch: usize,
    inputs: Inputs,
    origin: Instant,
    setup: &mut impl FnMut(usize) -> T,
    run: &mut impl FnMut(&mut T),
) -> Reading {
    let mut reading = Reading::default();
    let mut left = repeats;
    while left > 0 {
        let count = match inputs {
            Inputs::Own => left.min(batch),
            Inputs::Shared => left,
        };
        let made = Instant::now();
        let mut batch_inputs: Vec<T> = match inputs {
            Inputs::Own => (0..count).map(|_| setup(size)).collect(),
            Inputs::Shared => (0..batch).map(|_| setup(size)).collect(),
        };
        reading.setup += made.elapsed();
        let (started, wall) = (work(origin), Instant::now());
        match inputs {
            Inputs::Own => batch_inputs.iter_mut().for_each(&mut *run),
            Inputs::Shared => {
                let shared = batch_inputs.len();
                (0..count).for_each(|index| run(&mut batch_inputs[index % shared]));
            }
        }
        reading.work += work(origin).saturating_sub(started);
        reading.measured += wall.elapsed();
        let dropped = Instant::now();
        drop(batch_inputs);
        reading.setup += dropped.elapsed();
        left -= count;
    }
    reading
}

/// Asserts that `run` over a [`GROWTH`] times larger input takes less than [`BOUND`] times the
/// work, which linear and `n log n` steps pass and quadratic ones fail.
///
/// `setup` is not measured. Each run gets its own input, so a step that caches results is
/// measured computing them. Use [`assert_linear_shared`] for steps that only read their input.
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

/// [`assert_linear`] with a custom work ratio bound.
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
    compare(what, small, bound, Inputs::Own, &mut setup, &mut run);
}

/// [`assert_linear`] for a step that only reads its input, so runs can share inputs.
///
/// # Panics
/// When the large input takes [`BOUND`] times the small one's work or more,
/// naming `what`.
pub fn assert_linear_shared<T>(
    what: &str,
    small: usize,
    setup: impl FnMut(usize) -> T,
    run: impl FnMut(&T),
) {
    assert_growth_below_shared(what, small, BOUND, setup, run);
}

/// [`assert_growth_below`] for a step that only reads its input.
///
/// # Panics
/// When the large input takes `bound` times the small one's work or more,
/// naming `what`.
pub fn assert_growth_below_shared<T>(
    what: &str,
    small: usize,
    bound: u64,
    mut setup: impl FnMut(usize) -> T,
    mut run: impl FnMut(&T),
) {
    compare(
        what,
        small,
        bound,
        Inputs::Shared,
        &mut setup,
        &mut |input: &mut T| run(input),
    );
}

fn compare<T>(
    what: &str,
    small: usize,
    bound: u64,
    inputs: Inputs,
    setup: &mut impl FnMut(usize) -> T,
    run: &mut impl FnMut(&mut T),
) {
    // Keep both sizes on the same core type on hybrid CPUs.
    alpha2_os::prefer_fast_cores();
    let origin = Instant::now();
    let large = small * GROWTH;
    let large_batch = (BATCH_ITEMS / large).max(1);
    let small_batch = large_batch * GROWTH;
    let (mut setup_total, mut measured_total) = (Duration::ZERO, Duration::ZERO);
    let mut read = |size: usize, repeats: usize| {
        let batch = match (inputs, size == large) {
            (Inputs::Own, true) => large_batch,
            (Inputs::Own, false) => small_batch,
            (Inputs::Shared, true) => 1,
            (Inputs::Shared, false) => GROWTH,
        };
        let reading = measure(size, repeats, batch, inputs, origin, setup, run);
        setup_total += reading.setup;
        measured_total += reading.measured;
        reading
    };
    let mut repeats = 1;
    let mut least_small = loop {
        let reading = read(small, repeats);
        if reading.measured >= LEAST_RUN
            || (reading.setup > MOST_SETUP && reading.measured >= LEAST_MEASURED)
            || repeats >= 1 << 16
        {
            break reading.work;
        }
        repeats *= 2;
    };
    let mut least_large = u64::MAX;
    // Alternate sizes so a slow stretch affects both.
    for reading in 0..READINGS {
        least_large = least_large.min(read(large, repeats).work);
        if reading + 1 < READINGS {
            least_small = least_small.min(read(small, repeats).work);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = least_large as f64 / least_small.max(1) as f64;
    let (setup_ms, measured_ms) = (setup_total.as_millis(), measured_total.as_millis());
    eprintln!(
        "{what}: {GROWTH}x the input took {ratio:.2}x the work ({repeats} runs each; setup \
         {setup_ms} ms / measured {measured_ms} ms)"
    );
    assert!(
        least_large < least_small.saturating_mul(bound),
        "{what}: {GROWTH}x the input ({small} -> {large}, {repeats} runs each) took {ratio:.1}x the \
         work ({least_small} -> {least_large}); the bound is {bound}x (linear work takes about \
         {GROWTH}x, quadratic {}x); setup {setup_ms} ms / measured {measured_ms} ms",
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
    fn linear_work_over_a_shared_input_passes() {
        assert_linear_shared(
            "a sum over a shared input",
            200_000,
            |size| (0..size as u64).collect::<Vec<_>>(),
            |values| sum_pairs(values, false),
        );
    }

    #[test]
    #[should_panic(expected = "a shared sum over every pair")]
    fn quadratic_work_over_a_shared_input_fails() {
        assert_linear_shared(
            "a shared sum over every pair",
            500,
            |size| (0..size as u64).collect::<Vec<_>>(),
            |values| sum_pairs(values, true),
        );
    }

    #[test]
    fn a_shared_input_is_made_once_per_reading() {
        const SMALL: usize = 200_000;
        let made = std::cell::Cell::new([0_usize; 2]);
        let ran = std::cell::Cell::new([0_usize; 2]);
        let count = |cell: &std::cell::Cell<[usize; 2]>, size: usize| {
            let mut counts = cell.get();
            counts[usize::from(size != SMALL)] += 1;
            cell.set(counts);
        };
        assert_linear_shared(
            "a sum over a counted input",
            SMALL,
            |size| {
                count(&made, size);
                (0..size as u64).collect::<Vec<_>>()
            },
            |values| {
                count(&ran, values.len());
                sum_pairs(values, false);
            },
        );
        let ([made_small, made_large], [_, ran_large]) = (made.get(), ran.get());
        assert_eq!(made_large, READINGS, "one large input per reading");
        assert!(
            ran_large >= made_large * 4,
            "{made_large} large inputs for {ran_large} runs"
        );
        assert_eq!(made_small % GROWTH, 0, "{made_small} small inputs");
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
