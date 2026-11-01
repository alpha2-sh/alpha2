//! Speed change that keeps pitch: waveform-similarity overlap-add (WSOLA).
//!
//! Grain `k` is written at output sample `k * hop` through a periodic Hann window twice the hop,
//! so neighbours sum to one. It is read at the caller's source position for `k * hop`, shifted
//! within the tolerance to best continue the previous grain. The search runs on the channel mean,
//! coarse on sums of four samples, then at full rate.
//!
//! Offsets chain from grain to grain, so chains are cut into segments of
//! [`STRETCH_SEGMENT_GRAINS`], each restarting from offset zero [`STRETCH_WARMUP_GRAINS`] grains
//! early. Every output sample therefore depends only on its index, never on blocks or seeks.
//! Sums run in a fixed order without FMA, so native and wasm agree bit for bit.

use std::f32::consts::PI;

/// Grains whose offsets one chain decides.
pub const STRETCH_SEGMENT_GRAINS: i64 = 32;

/// Grains a segment's chain runs before its first, from offset zero.
pub const STRETCH_WARMUP_GRAINS: i64 = 8;

/// `samples` from index `start`; silent elsewhere and wherever a sample is not finite.
#[derive(Clone, Copy, Debug)]
pub struct StretchSignal<'a> {
    /// Index of the first sample.
    pub start: i64,
    /// The samples.
    pub samples: &'a [f32],
}

impl StretchSignal<'_> {
    fn at(&self, index: i64) -> f32 {
        let value = index
            .checked_sub(self.start)
            .and_then(|at| usize::try_from(at).ok())
            .and_then(|at| self.samples.get(at).copied())
            .unwrap_or(0.0);
        if value.is_finite() { value } else { 0.0 }
    }
}

/// WSOLA's grid at one sample rate.
#[derive(Clone, Debug)]
pub struct Stretch {
    hop: i64,
    tolerance: i64,
    window: Vec<f32>,
}

impl Stretch {
    /// A 20 ms hop, 40 ms window and 10 ms tolerance at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let rate = f64::from(sample_rate.max(1));
        #[allow(clippy::cast_possible_truncation)]
        let hop = ((rate * 0.020).round() as i64).max(1);
        #[allow(clippy::cast_possible_truncation)]
        let tolerance = (rate * 0.010).round() as i64;
        let length = 2 * hop;
        #[allow(clippy::cast_precision_loss)]
        let window = (0..length)
            .map(|index| {
                let value = libm::sinf(PI * index as f32 / length as f32);
                value * value
            })
            .collect();
        Self {
            hop,
            tolerance,
            window,
        }
    }

    /// Output samples between grains.
    #[must_use]
    pub const fn hop(&self) -> i64 {
        self.hop
    }

    /// How far, in samples, a grain may move from its position.
    #[must_use]
    pub const fn tolerance(&self) -> i64 {
        self.tolerance
    }

    /// Samples one grain spans.
    #[must_use]
    pub const fn window(&self) -> i64 {
        2 * self.hop
    }

    /// The first and last grains whose windows touch output samples `[first, end)`.
    #[must_use]
    pub fn grains(&self, first: i64, end: i64) -> (i64, i64) {
        (
            first.div_euclid(self.hop) - 1,
            (end - 1).div_euclid(self.hop),
        )
    }

    /// The segment whose chain decides grain `grain`'s offset.
    #[must_use]
    pub const fn segment(grain: i64) -> i64 {
        grain.div_euclid(STRETCH_SEGMENT_GRAINS)
    }

    /// The inclusive grain range `segment`'s chain runs over, warm-up included.
    #[must_use]
    pub const fn chain(segment: i64) -> (i64, i64) {
        let first = segment * STRETCH_SEGMENT_GRAINS;
        (
            first - STRETCH_WARMUP_GRAINS,
            first + STRETCH_SEGMENT_GRAINS - 1,
        )
    }

    /// The signal samples `[from, to)` that deciding and reading grains `first ..= last` can
    /// touch, with grain `k` at `position(k)`.
    pub fn reach(&self, first: i64, last: i64, position: impl Fn(i64) -> i64) -> (i64, i64) {
        let span = self.window() + self.tolerance;
        let mut from = i64::MAX;
        let mut to = i64::MIN;
        let mut previous = None;
        for grain in first..=last {
            let at = position(grain);
            from = from.min(at - self.tolerance);
            to = to.max(at + span);
            if let Some(previous) = previous {
                to = to.max(previous + self.hop + span);
            }
            previous = Some(at);
        }
        (from, to)
    }

    /// The signal samples `[from, to)` that reading output `[first, end)` can touch, including
    /// the full chains of their segments.
    pub fn reach_output(&self, first: i64, end: i64, position: impl Fn(i64) -> i64) -> (i64, i64) {
        let (low, high) = self.grains(first, end);
        let mut reach = (i64::MAX, i64::MIN);
        for segment in Self::segment(low)..=Self::segment(high) {
            let (from, to) = Self::chain(segment);
            let (from, to) = self.reach(from, to, &position);
            reach = (reach.0.min(from), reach.1.max(to));
        }
        reach
    }

    /// The offsets `segment`'s chain gives its grains, in order, searched on `mono`.
    pub fn offsets(
        &self,
        segment: i64,
        position: impl Fn(i64) -> i64,
        mono: StretchSignal<'_>,
    ) -> Vec<i64> {
        let (first, last) = Self::chain(segment);
        let own = segment * STRETCH_SEGMENT_GRAINS;
        let (from, to) = self.reach(first, last, &position);
        // Sums of four samples from each index, for the coarse search.
        let boxes: Vec<f32> = (from..to)
            .map(|index| {
                ((mono.at(index) + mono.at(index + 1)) + mono.at(index + 2)) + mono.at(index + 3)
            })
            .collect();
        let boxes = StretchSignal {
            start: from,
            samples: &boxes,
        };
        let mut offsets = Vec::with_capacity(usize::try_from(STRETCH_SEGMENT_GRAINS).unwrap_or(0));
        let mut read = position(first);
        for grain in first + 1..=last {
            let at = position(grain);
            let offset = self.search(read + self.hop, at, mono, boxes);
            read = at + offset;
            if grain >= own {
                offsets.push(offset);
            }
        }
        offsets
    }

    /// The offset within the tolerance with the largest normalized correlation between `at` and
    /// `reference`; ties go to the smallest magnitude, then the negative.
    fn search(
        &self,
        reference: i64,
        at: i64,
        mono: StretchSignal<'_>,
        boxes: StretchSignal<'_>,
    ) -> i64 {
        let length = self.window();
        let coarse = self.tolerance / 4;
        let mut best = (0, f64::NEG_INFINITY);
        for step in nearest_first(-coarse, coarse) {
            let score = correlation(boxes, reference, at + 4 * step, length / 4, 4);
            if score > best.1 {
                best = (step, score);
            }
        }
        let centre = 4 * best.0;
        let mut best = (0, f64::NEG_INFINITY);
        let low = (centre - 4).max(-self.tolerance);
        let high = (centre + 4).min(self.tolerance);
        for offset in nearest_first(low, high) {
            let score = correlation(mono, reference, at + offset, length, 1);
            if score > best.1 {
                best = (offset, score);
            }
        }
        best.0
    }

    /// Adds each windowed grain `(k, read)` from `signal` into `output`, which starts at output
    /// sample `first`.
    pub fn overlap_add(
        &self,
        first: i64,
        output: &mut [f32],
        grains: impl IntoIterator<Item = (i64, i64)>,
        signal: StretchSignal<'_>,
    ) {
        let end = first + i64::try_from(output.len()).unwrap_or(i64::MAX);
        for (grain, read) in grains {
            let start = grain * self.hop;
            let from = start.max(first);
            let to = (start + self.window()).min(end);
            for sample in from..to {
                let index = usize::try_from(sample - start).unwrap_or(0);
                let out = usize::try_from(sample - first).unwrap_or(0);
                output[out] += self.window[index] * signal.at(read + (sample - start));
            }
        }
    }
}

/// A [`Stretch`] that caches the last segments' offsets, so sequential reads search each once.
#[derive(Clone, Debug)]
pub struct StretchReader {
    stretch: Stretch,
    segments: Vec<(i64, Vec<i64>)>,
}

impl StretchReader {
    /// An empty reader at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self {
            stretch: Stretch::new(sample_rate),
            segments: Vec::new(),
        }
    }

    /// The grid.
    #[must_use]
    pub const fn stretch(&self) -> &Stretch {
        &self.stretch
    }

    /// Adds output samples from `first` into each of `outputs` from the matching `channels`
    /// signal, searching on `mono`. Outputs should start silent.
    pub fn render(
        &mut self,
        first: i64,
        outputs: &mut [&mut [f32]],
        position: impl Fn(i64) -> i64,
        channels: &[StretchSignal<'_>],
        mono: StretchSignal<'_>,
    ) {
        let Some(length) = outputs.first().map(|output| output.len()) else {
            return;
        };
        if length == 0 {
            return;
        }
        let end = first + i64::try_from(length).unwrap_or(i64::MAX);
        let (low, high) = self.stretch.grains(first, end);
        let segments = Stretch::segment(low)..=Stretch::segment(high);
        self.segments
            .retain(|(segment, _)| segments.contains(segment));
        for segment in segments.clone() {
            if !self.segments.iter().any(|(kept, _)| *kept == segment) {
                let offsets = self.stretch.offsets(segment, &position, mono);
                self.segments.push((segment, offsets));
            }
        }
        let offset = |grain: i64| {
            let segment = Stretch::segment(grain);
            let index = grain - segment * STRETCH_SEGMENT_GRAINS;
            self.segments
                .iter()
                .find(|(kept, _)| *kept == segment)
                .and_then(|(_, offsets)| offsets.get(usize::try_from(index).ok()?).copied())
                .unwrap_or(0)
        };
        let reads: Vec<(i64, i64)> = (low..=high)
            .map(|grain| (grain, position(grain) + offset(grain)))
            .collect();
        for (output, signal) in outputs.iter_mut().zip(channels) {
            self.stretch
                .overlap_add(first, output, reads.iter().copied(), *signal);
        }
    }
}

/// `low ..= high` in order of magnitude, negatives first.
fn nearest_first(low: i64, high: i64) -> impl Iterator<Item = i64> {
    let mut values: Vec<i64> = (low..=high).collect();
    values.sort_by_key(|&value| (value.abs(), value));
    values.into_iter()
}

/// Correlation of `count` samples `stride` apart from `a` and `b`, normalized by the root energy
/// at `b`; zero where silent.
fn correlation(signal: StretchSignal<'_>, a: i64, b: i64, count: i64, stride: i64) -> f64 {
    let mut dot = 0.0_f64;
    let mut energy = 0.0_f64;
    for index in 0..count {
        let x = f64::from(signal.at(a + index * stride));
        let y = f64::from(signal.at(b + index * stride));
        dot += x * y;
        energy += y * y;
    }
    if energy > 0.0 {
        dot / energy.sqrt()
    } else {
        0.0
    }
}

#[cfg(test)]
#[path = "stretch_tests.rs"]
mod tests;
