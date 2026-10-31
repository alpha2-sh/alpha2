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

/// The longest the inputs of one reading take to make before the small
/// size stops growing its runs, once those runs take [`LEAST_MEASURED`]: a
/// step that is quick next to its input's making would otherwise spend
/// most of a test making inputs.
const MOST_SETUP: Duration = Duration::from_secs(1);

/// The least the runs of a reading take for [`MOST_SETUP`] to end the
/// calibration: still many times the resolution of the clock.
const LEAST_MEASURED: Duration = Duration::from_millis(5);

/// The calling thread's work so far: its CPU work where the operating system
/// reports it, else the time since `origin`.
fn work(origin: Instant) -> u64 {
    alpha2_os::thread_cpu_work()
        .unwrap_or_else(|| u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

/// One reading: the work of the runs, the wall time they took, and the wall
/// time making and dropping their inputs took.
#[derive(Clone, Copy, Default)]
struct Reading {
    work: u64,
    measured: Duration,
    setup: Duration,
}

/// How a reading gives its runs their inputs.
#[derive(Clone, Copy)]
enum Inputs {
    /// Every run an input of its own, made `batch` at a time.
    Own,
    /// Inputs that every run of the reading reads only, made once per
    /// reading: [`GROWTH`] of the small size, read in turn, and one of the
    /// large. Both sizes then read as many bytes between two runs over one
    /// input, so the small one does not run from a cache the large one
    /// overflows.
    Shared,
}

/// The reading of `run` over `repeats` inputs of `size`, made by `setup` and
/// dropped `batch` at a time (or `batch` inputs read in turn by all of them,
/// [`Inputs::Shared`]), the making and dropping outside the reading.
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
            // The shared inputs hold no more memory however many runs read them.
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

/// Asserts that `run` over an input `setup` makes of [`GROWTH`] times `small`
/// takes less than [`BOUND`] times the work it takes over one of `small`:
/// that its work grows slower than the size to the power 1.5, as linear and
/// `n log n` work does and quadratic work does not.
///
/// `setup` makes an input of a size and is not measured; `run` is. Each run
/// gets an input of its own, so a step that caches what it computed is
/// measured computing it; the inputs are made a batch at a time, so no more
/// than about a million items' worth are held at once. The small input is
/// run as many times as takes [`LEAST_RUN`] together (or fewer, when making
/// their inputs takes over a second), and the large one as many times. A
/// step that only reads its input takes [`assert_linear_shared`], which makes
/// far fewer.
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
    compare(what, small, bound, Inputs::Own, &mut setup, &mut run);
}

/// [`assert_linear`] for a step that only reads its input: the runs of a
/// reading share a few inputs ([`GROWTH`] small ones, one large), so a test
/// whose inputs take far longer to make than to run is not spent making them.
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

/// [`assert_growth_below`] for a step that only reads its input, as
/// [`assert_linear_shared`] is to [`assert_linear`].
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

/// The comparison behind every assertion here.
fn compare<T>(
    what: &str,
    small: usize,
    bound: u64,
    inputs: Inputs,
    setup: &mut impl FnMut(usize) -> T,
    run: &mut impl FnMut(&mut T),
) {
    // Both sizes are read on one kind of core on a CPU that has two.
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
    // The run that reaches the least time is the first reading of the small size.
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
    // Alternated, so a slow stretch of the machine falls on both sizes.
    for reading in 0..READINGS {
        least_large = least_large.min(read(large, repeats).work);
        if reading + 1 < READINGS {
            least_small = least_small.min(read(small, repeats).work);
        }
    }
    // Precision does not matter in a ratio for a message.
    #[allow(clippy::cast_precision_loss)]
    let ratio = least_large as f64 / least_small.max(1) as f64;
    let (setup_ms, measured_ms) = (setup_total.as_millis(), measured_total.as_millis());
    // Shown with `--nocapture`, and with a failure.
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
        let made = std::cell::Cell::new(0_usize);
        let ran = std::cell::Cell::new(0_usize);
        assert_linear_shared(
            "a sum over a counted input",
            200_000,
            |size| {
                made.set(made.get() + 1);
                (0..size as u64).collect::<Vec<_>>()
            },
            |values| {
                ran.set(ran.get() + 1);
                sum_pairs(values, false);
            },
        );
        assert!(
            made.get() * 4 <= ran.get(),
            "{} inputs for {} runs",
            made.get(),
            ran.get()
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
