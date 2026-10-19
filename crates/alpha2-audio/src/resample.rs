//! The one sample-rate converter both hosts use to bring a source's audio to
//! the project rate.
//!
//! Output sample `n` sits at source position `n * source_rate / target_rate`,
//! an exact fraction, and is a band-limited interpolation of the source
//! samples around it: a Kaiser-windowed sinc with the shape of
//! `libswresample`'s default (32 taps at unity, cutoff 0.97 of the lower
//! Nyquist frequency, beta 9), widened when the rate falls. The result depends
//! only on the source samples and their positions, never on how they were
//! split into calls, so a decode that seeks and one that reads straight
//! through produce the same samples. Every coefficient is computed with
//! `libm`'s portable functions and every sum runs in one fixed order, so the
//! same samples give bit-identical output natively and in wasm.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use core::fmt;
use core::ops::Range;

/// Taps on each side of an output sample when the rate does not fall.
const HALF_TAPS: u64 = 16;
/// Kaiser window shape.
const KAISER_BETA: f64 = 9.0;
/// Passband edge as a fraction of the lower of the two Nyquist frequencies.
const CUTOFF: f64 = 0.97;
/// The most coefficients one converter keeps (1 MiB). A rate pair with more
/// distinct phases than fit shares the nearest of evenly spaced phases; one
/// whose filter alone (`2 * half` taps) would not fit is refused.
const MAX_TABLE_COEFFICIENTS: u64 = 1 << 18;
/// The highest rate either side may have.
pub const MAX_RESAMPLE_RATE: u32 = 768_000;
/// The most the rate may fall: a source rate at most this many times the
/// target rate. A falling rate widens the filter in proportion (16 taps a
/// side per factor), and past this fall the filter alone would not fit the
/// table. 768 kHz converts down to 94 Hz.
pub const MAX_DOWNSAMPLE_FACTOR: u32 = (MAX_TABLE_COEFFICIENTS / (2 * HALF_TAPS)) as u32;

/// Why a converter could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResampleError {
    /// The source rate asked for.
    pub source_rate: u32,
    /// The target rate asked for.
    pub target_rate: u32,
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot resample {} Hz to {} Hz: both rates must be 1 to {MAX_RESAMPLE_RATE} Hz \
             and the source rate at most {MAX_DOWNSAMPLE_FACTOR} times the target rate",
            self.source_rate, self.target_rate
        )
    }
}

impl std::error::Error for ResampleError {}

/// A band-limited converter from one sample rate to another.
#[derive(Clone, Debug)]
pub struct Resampler {
    source_rate: u32,
    target_rate: u32,
    /// `source_rate / gcd`: source samples per `target_step` output samples.
    source_step: u64,
    /// `target_rate / gcd`: the number of distinct output phases.
    target_step: u64,
    /// Taps on each side; zero for the identity.
    half: u64,
    /// Phases kept in `table`: `target_step`, or fewer when they do not fit.
    phases: u64,
    /// `phases` rows of `2 * half` coefficients. Row `q` weighs source
    /// samples `i - half + 1 ..= i + half` for an output at `i + q / phases`.
    table: Vec<f32>,
}

impl Resampler {
    /// A converter from `source_rate` to `target_rate`.
    ///
    /// Builds the whole coefficient table here, so call it off the real-time thread.
    ///
    /// # Errors
    /// Either rate is zero or above [`MAX_RESAMPLE_RATE`], or the source rate
    /// is more than [`MAX_DOWNSAMPLE_FACTOR`] times the target rate.
    pub fn new(source_rate: u32, target_rate: u32) -> Result<Self, ResampleError> {
        if !(1..=MAX_RESAMPLE_RATE).contains(&source_rate)
            || !(1..=MAX_RESAMPLE_RATE).contains(&target_rate)
            || u64::from(source_rate) > u64::from(MAX_DOWNSAMPLE_FACTOR) * u64::from(target_rate)
        {
            return Err(ResampleError {
                source_rate,
                target_rate,
            });
        }
        let divisor = gcd(u64::from(source_rate), u64::from(target_rate));
        let source_step = u64::from(source_rate) / divisor;
        let target_step = u64::from(target_rate) / divisor;
        if source_rate == target_rate {
            return Ok(Self {
                source_rate,
                target_rate,
                source_step,
                target_step,
                half: 0,
                phases: 1,
                table: Vec::new(),
            });
        }
        // Falling rates narrow the passband to the target's Nyquist frequency, and the filter
        // widens in proportion to keep its transition band.
        let factor = (f64::from(target_rate) / f64::from(source_rate)).min(1.0);
        let half = (HALF_TAPS * u64::from(source_rate)).div_ceil(u64::from(target_rate));
        let half = half.max(HALF_TAPS);
        let taps = 2 * half;
        debug_assert!(taps <= MAX_TABLE_COEFFICIENTS, "the fall is bounded above");
        let phases = target_step.min((MAX_TABLE_COEFFICIENTS / taps).max(1));
        let cutoff = CUTOFF * factor;
        let window_scale = 1.0 / bessel_i0(KAISER_BETA);
        let mut table = Vec::with_capacity(usize::try_from(phases * taps).unwrap_or(0));
        let mut row = vec![0.0_f64; usize::try_from(taps).unwrap_or(0)];
        for phase in 0..phases {
            let offset = phase as f64 / phases as f64;
            let mut sum = 0.0;
            for (tap, weight) in row.iter_mut().enumerate() {
                // The distance from the output to this source sample, in source samples.
                let distance = (tap as f64 + 1.0 - half as f64) - offset;
                let x = distance / half as f64;
                let window = if x.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(KAISER_BETA * libm::sqrt(1.0 - x * x)) * window_scale
                };
                *weight = cutoff * sinc(cutoff * distance) * window;
                sum += *weight;
            }
            // Unity gain at DC in every phase: a constant source stays constant.
            table.extend(row.iter().map(|weight| (*weight / sum) as f32));
        }
        Ok(Self {
            source_rate,
            target_rate,
            source_step,
            target_step,
            half,
            phases,
            table,
        })
    }

    /// Source samples per second.
    #[must_use]
    pub const fn source_rate(&self) -> u32 {
        self.source_rate
    }

    /// Output samples per second.
    #[must_use]
    pub const fn target_rate(&self) -> u32 {
        self.target_rate
    }

    /// Whether the two rates are equal: output sample `n` is source sample `n`.
    #[must_use]
    pub const fn is_identity(&self) -> bool {
        self.half == 0
    }

    /// Source samples on each side of an output sample that it weighs.
    #[must_use]
    pub fn half_taps(&self) -> i64 {
        i64::try_from(self.half).unwrap_or(i64::MAX)
    }

    /// The first output sample at or after source sample `source`.
    #[must_use]
    pub fn target_at_or_after(&self, source: i64) -> i64 {
        div_ceil(
            i128::from(source) * i128::from(self.target_step),
            i128::from(self.source_step),
        )
    }

    /// The first output sample that weighs a source sample at or after
    /// `source_end`: every output before it is known once the source is known
    /// up to `source_end`.
    #[must_use]
    pub fn target_known_before(&self, source_end: i64) -> i64 {
        // A phase rounded up to the next sample reads one sample further.
        let rounded = i64::from(self.phases < self.target_step);
        self.target_at_or_after(source_end.saturating_sub(self.half_taps() + rounded))
    }

    /// The source samples outputs `target` weigh.
    #[must_use]
    pub fn source_span(&self, target: Range<i64>) -> Range<i64> {
        if target.is_empty() {
            return 0..0;
        }
        let (first, _) = self.position(target.start);
        let (last, _) = self.position(target.end - 1);
        let half = self.half_taps();
        if half == 0 {
            return first..last + 1;
        }
        first - half + 1..last + half + 1
    }

    /// Writes outputs `target_start ..` into `output` from `input`, whose
    /// first sample is source sample `input_start`. The source is silent
    /// outside `input`.
    pub fn resample(&self, input: &[f32], input_start: i64, target_start: i64, output: &mut [f32]) {
        let len = i64::try_from(input.len()).unwrap_or(i64::MAX);
        let half = self.half_taps();
        let taps = usize::try_from(2 * self.half).unwrap_or(0);
        for (n, sample) in (target_start..).zip(output.iter_mut()) {
            let (index, phase) = self.position(n);
            if half == 0 {
                let at = index - input_start;
                *sample = if (0..len).contains(&at) {
                    input[usize::try_from(at).unwrap_or(0)]
                } else {
                    0.0
                };
                continue;
            }
            let first = index - half + 1 - input_start;
            let row = &self.table[phase * taps..(phase + 1) * taps];
            let mut sum = 0.0_f64;
            if first >= 0 && first + 2 * half <= len {
                let from = usize::try_from(first).unwrap_or(0);
                for (weight, value) in row.iter().zip(&input[from..from + taps]) {
                    sum += f64::from(*weight) * f64::from(*value);
                }
            } else {
                for (offset, weight) in (first..).zip(row) {
                    if (0..len).contains(&offset) {
                        sum += f64::from(*weight)
                            * f64::from(input[usize::try_from(offset).unwrap_or(0)]);
                    }
                }
            }
            *sample = sum as f32;
        }
    }

    /// The source sample at or before output `target`, and the table row of
    /// the fraction past it.
    fn position(&self, target: i64) -> (i64, usize) {
        let numerator = i128::from(target) * i128::from(self.source_step);
        let step = i128::from(self.target_step);
        let mut index = numerator.div_euclid(step);
        let remainder = numerator.rem_euclid(step);
        let phases = i128::from(self.phases);
        // The nearest kept phase, exact when every phase is kept.
        let mut phase = (remainder * phases * 2 + step).div_euclid(step * 2);
        if phase == phases {
            index += 1;
            phase = 0;
        }
        (
            i64::try_from(index).unwrap_or(i64::MAX),
            usize::try_from(phase).unwrap_or(0),
        )
    }
}

/// The fastest playback whose filter narrows with its speed. Faster clips
/// keep this filter, so one output sample weighs at most
/// `2 * HALF_TAPS * MAX_FILTERED_SPEED` source samples.
pub const MAX_FILTERED_SPEED: f64 = 64.0;
/// Table entries per source sample of the interpolation kernel at unity.
const KERNEL_RESOLUTION: usize = 1_024;

/// The band-limited interpolation a clip played at another speed reads its
/// source through: the same Kaiser-windowed sinc as [`Resampler`] (16 taps a
/// side at unity, cutoff 0.97 of the Nyquist frequency, beta 9), at any real
/// source position. Played faster than unity, the cutoff falls to the output's
/// Nyquist frequency and the filter widens in proportion, as a falling rate
/// does, so what the speed raises past the output's Nyquist frequency is
/// removed rather than folded back. Slower, the source's Nyquist frequency
/// bounds it, so no image of the source's spectrum is heard.
///
/// Every weight comes from one table computed with `libm`'s portable
/// functions, interpolated linearly, and every sum runs in one fixed order,
/// so the same samples give bit-identical output natively and in wasm.
#[derive(Clone, Copy, Debug)]
pub struct Interpolator {
    /// Source samples per kernel unit: the speed, at least one and at most
    /// [`MAX_FILTERED_SPEED`].
    scale: f64,
    /// Taps on each side.
    half: i64,
}

impl Interpolator {
    /// The filter for a clip playing `speed` source samples per output
    /// sample. A speed that is not a positive finite number takes the unity
    /// filter.
    #[must_use]
    pub fn new(speed: f64) -> Self {
        let scale = if speed.is_finite() {
            speed.clamp(1.0, MAX_FILTERED_SPEED)
        } else {
            1.0
        };
        Self {
            scale,
            half: libm::ceil(HALF_TAPS as f64 * scale) as i64,
        }
    }

    /// Source samples on each side of a position that it weighs: an output
    /// at `whole + fraction` reads source samples
    /// `whole - half + 1 ..= whole + half`.
    #[must_use]
    pub const fn half_taps(&self) -> i64 {
        self.half
    }

    /// Writes into `weights` the `2 * half` weights, summing to one, of
    /// source samples `whole - half + 1 ..= whole + half` for a position
    /// `fraction` (in `0..1`) past source sample `whole`.
    pub fn weights(&self, fraction: f64, weights: &mut Vec<f64>) {
        let table = kernel_table();
        let taps = usize::try_from(2 * self.half).unwrap_or(0);
        weights.resize(taps, 0.0);
        let step = KERNEL_RESOLUTION as f64 / self.scale;
        let last = table.len() - 1;
        let weigh = |distance: f64| {
            let at = distance * step;
            // Distances are never negative.
            #[allow(clippy::cast_sign_loss)]
            let index = at as usize;
            if index >= last {
                0.0
            } else {
                let low = table[index];
                low + (table[index + 1] - low) * (at - index as f64)
            }
        };
        // Taps `0 ..= half - 1` lie at or before the position, the rest after it.
        let before = usize::try_from(self.half).unwrap_or(0);
        let mut sum = 0.0;
        for (tap, weight) in weights[..before].iter_mut().enumerate() {
            *weight = weigh((before - 1 - tap) as f64 + fraction);
            sum += *weight;
        }
        for (tap, weight) in weights[before..].iter_mut().enumerate() {
            *weight = weigh((tap + 1) as f64 - fraction);
            sum += *weight;
        }
        // Unity gain at DC at every position: a constant source stays constant.
        let scale = 1.0 / sum;
        for weight in weights.iter_mut() {
            *weight *= scale;
        }
    }
}

/// The interpolation kernel at unity, `sinc(CUTOFF * u) * kaiser(u / 16)`,
/// at `KERNEL_RESOLUTION` points per unit of `u` over `0..=16`.
fn kernel_table() -> &'static [f64] {
    static TABLE: std::sync::OnceLock<Vec<f64>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let points = HALF_TAPS as usize * KERNEL_RESOLUTION;
        let window_scale = 1.0 / bessel_i0(KAISER_BETA);
        (0..=points)
            .map(|index| {
                let u = index as f64 / KERNEL_RESOLUTION as f64;
                let x = u / HALF_TAPS as f64;
                if x >= 1.0 {
                    return 0.0;
                }
                let window = bessel_i0(KAISER_BETA * libm::sqrt(1.0 - x * x)) * window_scale;
                CUTOFF * sinc(CUTOFF * u) * window
            })
            .collect()
    })
}

fn div_ceil(numerator: i128, denominator: i128) -> i64 {
    let quotient =
        numerator.div_euclid(denominator) + i128::from(numerator.rem_euclid(denominator) != 0);
    i64::try_from(quotient).unwrap_or(if quotient < 0 { i64::MIN } else { i64::MAX })
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let angle = core::f64::consts::PI * x;
    libm::sin(angle) / angle
}

/// The zeroth-order modified Bessel function of the first kind, by its series.
fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while term > sum * 1e-17 {
        term *= quarter / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}
