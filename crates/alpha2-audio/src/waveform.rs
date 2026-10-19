//! Multi-resolution waveform summaries for timeline display.
//!
//! A timeline draws a clip's audio as a column per pixel: the extremes of the
//! samples under it and their RMS. [`WaveformSummary`] computes those columns
//! at a base bucket size and at coarser zoom levels, each four times the
//! last, from planar samples streamed in blocks of any size.
//!
//! Level 0 is computed from the samples. Every coarser bucket is the fold of
//! four finer ones: its minimum and maximum are theirs exactly, and its RMS
//! comes from their sums of squares, accumulated in `f64` (a squared `f32` is
//! exact there). Results do not depend on how the input is split into blocks,
//! and they are bit-identical natively and as wasm. Extremes compare in
//! `f32::total_cmp` order, -0 below +0: `f32::min` and `max` may return
//! either zero when they compare equal, and aarch64 and `x86_64` differ. A
//! NaN or infinite sample counts as silence, as it plays in a mix.

/// The extremes and RMS of one run of samples of one channel.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct WaveformBucket {
    /// Smallest sample.
    pub min: f32,
    /// Largest sample.
    pub max: f32,
    /// Root mean square of the samples.
    pub rms: f32,
}

/// One zoom level of a [`Waveform`].
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformLevel {
    /// Samples per bucket at this level. The last bucket may cover fewer.
    pub bucket_samples: usize,
    /// Buckets per channel, in time order.
    pub channels: Vec<Vec<WaveformBucket>>,
}

/// A finished waveform summary: its levels, finest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Waveform {
    /// Samples summarized per channel.
    pub frames: u64,
    /// Levels from the base bucket size up, each four times coarser.
    pub levels: Vec<WaveformLevel>,
}

/// Why a waveform summary cannot be configured or fed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WaveformError {
    /// No channels, a zero bucket, or no levels.
    InvalidShape,
    /// A block's channel count or plane lengths do not match.
    MismatchedBlock,
}

impl std::fmt::Display for WaveformError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidShape => "a waveform needs channels, a positive bucket, and a level",
            Self::MismatchedBlock => "the block's channels do not match the waveform's",
        })
    }
}

impl std::error::Error for WaveformError {}

/// Buckets folded into one bucket of the next level.
const FOLD: usize = 4;
/// Most zoom levels: a 1-sample base reaches 4^15 samples, over six hours at
/// 48 kHz, which is coarser than any timeline shows.
pub const MAX_WAVEFORM_LEVELS: usize = 16;

/// A bucket still being filled: exact extremes and the sum of squares.
#[derive(Copy, Clone, Debug)]
struct Accumulator {
    min: f32,
    max: f32,
    sum_squares: f64,
    samples: u64,
    parts: usize,
}

impl Accumulator {
    const EMPTY: Self = Self {
        min: f32::INFINITY,
        max: f32::NEG_INFINITY,
        sum_squares: 0.0,
        samples: 0,
        parts: 0,
    };

    fn is_empty(&self) -> bool {
        self.samples == 0
    }

    /// Adds one sample. A NaN or infinite one counts as silence, as a mix
    /// plays it and the loudness meter measures it.
    fn add_sample(&mut self, sample: f32) {
        let sample = if sample.is_finite() { sample } else { 0.0 };
        self.min = lower(self.min, sample);
        self.max = higher(self.max, sample);
        let square = f64::from(sample);
        self.sum_squares += square * square;
        self.samples += 1;
    }

    fn add_part(&mut self, part: &Self) {
        self.min = lower(self.min, part.min);
        self.max = higher(self.max, part.max);
        self.sum_squares += part.sum_squares;
        self.samples += part.samples;
        self.parts += 1;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn bucket(&self) -> WaveformBucket {
        WaveformBucket {
            min: self.min,
            max: self.max,
            rms: (self.sum_squares / self.samples as f64).sqrt() as f32,
        }
    }
}

/// The smaller of `current` and `sample`, -0 below +0.
fn lower(current: f32, sample: f32) -> f32 {
    if sample.total_cmp(&current).is_lt() {
        sample
    } else {
        current
    }
}

/// The larger of `current` and `sample`, +0 above -0.
fn higher(current: f32, sample: f32) -> f32 {
    if sample.total_cmp(&current).is_gt() {
        sample
    } else {
        current
    }
}

/// Builds a [`Waveform`] from planar samples streamed in blocks.
#[derive(Clone, Debug)]
pub struct WaveformSummary {
    base_bucket: usize,
    levels: Vec<WaveformLevel>,
    /// The open bucket of each level, per channel: `open[level][channel]`.
    open: Vec<Vec<Accumulator>>,
    frames: u64,
}

impl WaveformSummary {
    /// A summary of `channels` channels whose finest buckets hold
    /// `base_bucket` samples, with `levels` zoom levels in all.
    ///
    /// # Errors
    /// Returns [`WaveformError::InvalidShape`] for zero channels, a zero
    /// bucket, or a level count outside `1..=MAX_WAVEFORM_LEVELS`.
    pub fn new(channels: usize, base_bucket: usize, levels: usize) -> Result<Self, WaveformError> {
        if channels == 0 || base_bucket == 0 || !(1..=MAX_WAVEFORM_LEVELS).contains(&levels) {
            return Err(WaveformError::InvalidShape);
        }
        let mut bucket_samples = base_bucket;
        let mut summary_levels = Vec::with_capacity(levels);
        for _ in 0..levels {
            summary_levels.push(WaveformLevel {
                bucket_samples,
                channels: vec![Vec::new(); channels],
            });
            bucket_samples = bucket_samples.saturating_mul(FOLD);
        }
        Ok(Self {
            base_bucket,
            levels: summary_levels,
            open: vec![vec![Accumulator::EMPTY; channels]; levels],
            frames: 0,
        })
    }

    /// Adds one block of planar samples, one slice per channel, all of one
    /// length.
    ///
    /// # Errors
    /// Returns [`WaveformError::MismatchedBlock`] for a block with another
    /// channel count or ragged planes.
    pub fn push(&mut self, planes: &[&[f32]]) -> Result<(), WaveformError> {
        let channels = self.levels[0].channels.len();
        let Some(frames) = planes.first().map(|plane| plane.len()) else {
            return Err(WaveformError::MismatchedBlock);
        };
        if planes.len() != channels || planes.iter().any(|plane| plane.len() != frames) {
            return Err(WaveformError::MismatchedBlock);
        }
        for index in 0..frames {
            for (channel, plane) in planes.iter().enumerate() {
                self.open[0][channel].add_sample(plane[index]);
            }
            if self.open[0][0].samples == self.base_bucket as u64 {
                self.close(0);
            }
        }
        self.frames += frames as u64;
        Ok(())
    }

    /// Closes the open bucket of `level` on every channel and folds it into
    /// the next level, closing that too once it has all its parts.
    fn close(&mut self, level: usize) {
        let channels = self.open[level].len();
        for channel in 0..channels {
            let finished = self.open[level][channel];
            self.levels[level].channels[channel].push(finished.bucket());
            if let Some(next) = self.open.get_mut(level + 1) {
                next[channel].add_part(&finished);
            }
            self.open[level][channel] = Accumulator::EMPTY;
        }
        if level + 1 < self.open.len() && self.open[level + 1][0].parts == FOLD {
            self.close(level + 1);
        }
    }

    /// Closes the partial buckets and returns the summary.
    #[must_use]
    pub fn finish(mut self) -> Waveform {
        for level in 0..self.open.len() {
            if !self.open[level][0].is_empty() {
                self.close(level);
            }
        }
        Waveform {
            frames: self.frames,
            levels: self.levels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WaveformBucket, WaveformError, WaveformSummary};

    /// The extremes of each level-0 bucket of one channel, as bits.
    fn extreme_bits(samples: &[f32], bucket: usize) -> Vec<(u32, u32)> {
        let mut summary = WaveformSummary::new(1, bucket, 1).expect("shape");
        summary.push(&[samples]).expect("block");
        summary.finish().levels[0].channels[0]
            .iter()
            .map(|bucket| (bucket.min.to_bits(), bucket.max.to_bits()))
            .collect()
    }

    #[test]
    fn a_signed_zero_extreme_is_the_same_on_every_target() {
        // f32::min and max may return either zero when they compare equal, and aarch64 and x86_64
        // pick differently: -0 is the smaller and +0 the larger, in whatever order they arrive, so
        // the bits are one answer everywhere.
        let (positive, negative) = (0.0_f32.to_bits(), (-0.0_f32).to_bits());
        let samples = [
            0.0, -0.0, 0.0, -0.0, // +0 first
            -0.0, 0.0, -0.0, 0.0, // -0 first
            -0.25, 0.0, -0.0, -0.0, // a negative extreme, a zero maximum
            0.25, -0.0, 0.0, 0.0, // a zero minimum, a positive extreme
            -0.0, -0.0, -0.0, -0.0, // one zero alone
        ];
        assert_eq!(
            extreme_bits(&samples, 4),
            [
                (negative, positive),
                (negative, positive),
                ((-0.25_f32).to_bits(), positive),
                (negative, 0.25_f32.to_bits()),
                (negative, negative),
            ]
        );
        let mut summary = WaveformSummary::new(1, 2, 2).expect("shape");
        summary.push(&[&samples[..8]]).expect("block");
        let coarse = summary.finish().levels[1].channels[0][0];
        assert_eq!(
            (coarse.min.to_bits(), coarse.max.to_bits()),
            (negative, positive)
        );
    }

    fn sine(frames: usize, period: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let phase = index as f32 / period as f32 * core::f32::consts::TAU;
                amplitude * libm::sinf(phase)
            })
            .collect()
    }

    #[test]
    fn a_sine_has_its_peak_and_its_rms() {
        // 64 samples per period, whole periods per bucket.
        let signal = sine(4_096, 64, 0.5);
        let mut summary = WaveformSummary::new(1, 256, 1).expect("shape");
        summary.push(&[&signal]).expect("block");
        let waveform = summary.finish();
        assert_eq!(waveform.frames, 4_096);
        let buckets = &waveform.levels[0].channels[0];
        assert_eq!(buckets.len(), 16);
        for bucket in buckets {
            assert!((bucket.max - 0.5).abs() < 1e-6, "{bucket:?}");
            assert!((bucket.min + 0.5).abs() < 1e-6, "{bucket:?}");
            assert!(
                (bucket.rms - 0.5 / 2.0_f32.sqrt()).abs() < 1e-5,
                "{bucket:?}"
            );
        }
    }

    #[test]
    fn coarser_levels_are_the_fold_of_finer_ones_and_match_direct_summaries() {
        let left = sine(10_000, 37, 0.8);
        let right = sine(10_000, 91, 0.3)
            .into_iter()
            .enumerate()
            .map(|(index, sample)| if index % 7 == 0 { -sample } else { sample })
            .collect::<Vec<_>>();
        let mut folded = WaveformSummary::new(2, 16, 3).expect("shape");
        folded.push(&[&left, &right]).expect("block");
        let folded = folded.finish();
        for (level, bucket_samples) in [(1, 64), (2, 256)] {
            let mut direct = WaveformSummary::new(2, bucket_samples, 1).expect("shape");
            direct.push(&[&left, &right]).expect("block");
            let direct = direct.finish();
            assert_eq!(folded.levels[level].bucket_samples, bucket_samples);
            for channel in 0..2 {
                let ours = &folded.levels[level].channels[channel];
                let theirs = &direct.levels[0].channels[channel];
                assert_eq!(ours.len(), theirs.len(), "level {level}");
                for (a, b) in ours.iter().zip(theirs) {
                    assert_eq!(a.min.to_bits(), b.min.to_bits());
                    assert_eq!(a.max.to_bits(), b.max.to_bits());
                    // The sums of squares add in another order; in f64 that moves the f32 RMS by at
                    // most one unit in the last place.
                    assert!((a.rms - b.rms).abs() <= f32::EPSILON * b.rms, "{a:?} {b:?}");
                }
            }
        }
        // 10 000 samples in buckets of 16: 625 full buckets; 157 and 40 above.
        assert_eq!(folded.levels[0].channels[0].len(), 625);
        assert_eq!(folded.levels[1].channels[0].len(), 157);
        assert_eq!(folded.levels[2].channels[0].len(), 40);
    }

    #[test]
    fn block_boundaries_do_not_change_the_result() {
        let signal = sine(5_000, 23, 0.9);
        let whole = {
            let mut summary = WaveformSummary::new(1, 30, 4).expect("shape");
            summary.push(&[&signal]).expect("block");
            summary.finish()
        };
        for block in [1, 7, 29, 128, 4_999] {
            let mut summary = WaveformSummary::new(1, 30, 4).expect("shape");
            for chunk in signal.chunks(block) {
                summary.push(&[chunk]).expect("block");
            }
            assert_eq!(summary.finish(), whole, "block {block}");
        }
    }

    #[test]
    fn a_partial_last_bucket_covers_only_its_samples() {
        let mut summary = WaveformSummary::new(1, 4, 2).expect("shape");
        summary
            .push(&[&[0.5, -0.5, 0.5, -0.5, 1.0, 1.0]])
            .expect("block");
        let waveform = summary.finish();
        assert_eq!(
            waveform.levels[0].channels[0],
            vec![
                WaveformBucket {
                    min: -0.5,
                    max: 0.5,
                    rms: 0.5
                },
                WaveformBucket {
                    min: 1.0,
                    max: 1.0,
                    rms: 1.0
                },
            ]
        );
        let top = waveform.levels[1].channels[0][0];
        assert_eq!((top.min, top.max), (-0.5, 1.0));
        // sqrt((4 * 0.25 + 2 * 1) / 6) = sqrt(0.5)
        assert!((top.rms - 0.5_f32.sqrt()).abs() < 1e-7);
    }

    #[test]
    fn a_non_finite_sample_counts_as_silence_at_every_level() {
        // A mix and the loudness meter play a non-finite sample as silence, so the waveform does
        // too rather than blanking the RMS of every bucket above it.
        let samples = [
            0.1,
            f32::NAN,
            0.2,
            0.3,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            0.1,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.1,
        ];
        let mut summary = WaveformSummary::new(1, 4, 2).expect("shape");
        summary.push(&[&samples]).expect("block");
        let waveform = summary.finish();
        let silenced = samples.map(|sample| if sample.is_finite() { sample } else { 0.0 });
        let mut reference = WaveformSummary::new(1, 4, 2).expect("shape");
        reference.push(&[&silenced]).expect("block");
        assert_eq!(waveform, reference.finish());
        for level in &waveform.levels {
            for bucket in &level.channels[0] {
                assert!(
                    bucket.min.is_finite() && bucket.max.is_finite() && bucket.rms.is_finite(),
                    "{bucket:?}"
                );
            }
        }
        // A bucket of nothing but NaN is a silent bucket, not the empty sentinel.
        assert_eq!(
            waveform.levels[0].channels[0][1],
            WaveformBucket {
                min: 0.0,
                max: 0.0,
                rms: 0.0
            }
        );
    }

    #[test]
    fn shapes_and_blocks_are_checked() {
        assert_eq!(
            WaveformSummary::new(0, 1, 1).map(|_| ()),
            Err(WaveformError::InvalidShape)
        );
        assert_eq!(
            WaveformSummary::new(1, 0, 1).map(|_| ()),
            Err(WaveformError::InvalidShape)
        );
        assert_eq!(
            WaveformSummary::new(1, 1, 17).map(|_| ()),
            Err(WaveformError::InvalidShape)
        );
        let mut summary = WaveformSummary::new(2, 8, 1).expect("shape");
        assert_eq!(
            summary.push(&[&[0.0; 3], &[0.0; 2]]),
            Err(WaveformError::MismatchedBlock)
        );
        assert_eq!(
            summary.push(&[&[0.0; 3]]),
            Err(WaveformError::MismatchedBlock)
        );
    }
}
