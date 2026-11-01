//! Where an audio stream's frames start when its container rounds their timestamps.

use crate::media_time::rounding_quantum;

/// Frame start samples of an audio stream, every `period` samples from `anchor`, inferred from
/// its first packet timestamps ([`audio_frame_grid`]).
///
/// Containers that round timestamps (Matroska and `WebM` to the millisecond) place a frame up to
/// half a unit (24 samples at 48 kHz) off its true start, and a seek and a straight decode are off
/// by different amounts. For fixed-length frames (AAC's 1024 samples, Opus's 20 ms) the first
/// timestamps together locate the grid far more precisely, so snapping to it makes a seek give
/// the same samples as a straight decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrameGrid {
    /// A source sample at which a frame starts.
    pub anchor: i64,
    /// Samples per frame.
    pub period: i64,
    /// Farthest a frame timestamp, in samples, can be from its true start: half the rounding
    /// unit plus half a sample, rounded.
    pub start_tolerance: i64,
    /// Farthest the end of a trimmed frame (shorter than `period`, as after Opus pre-skip) can
    /// be from the grid. The trim is rounded again, so this is a full unit plus a sample.
    pub trimmed_tolerance: i64,
}

impl AudioFrameGrid {
    /// Start of a `frames`-sample frame whose timestamp says `at`: snapped to the grid when
    /// within tolerance, otherwise `at`.
    ///
    /// A frame shorter than [`Self::period`] is first tried as a trimmed frame ending on the grid.
    #[must_use]
    pub fn place(&self, at: i64, frames: i64) -> i64 {
        if (1..self.period).contains(&frames) {
            let end = at.saturating_add(frames);
            let shift = self.shift_onto(end);
            if shift.abs() <= self.trimmed_tolerance {
                return at.saturating_add(shift);
            }
        }
        let shift = self.shift_onto(at);
        if shift.abs() <= self.start_tolerance {
            return at.saturating_add(shift);
        }
        at
    }

    fn shift_onto(&self, position: i64) -> i64 {
        let offset =
            (i128::from(position) - i128::from(self.anchor)).rem_euclid(i128::from(self.period));
        let shift = if offset * 2 <= i128::from(self.period) {
            -offset
        } else {
            i128::from(self.period) - offset
        };
        i64::try_from(shift).unwrap_or(0)
    }
}

/// The frame grid shown by an audio stream's first packet timestamps (any order, in `num / den`
/// second units) at `sample_rate`, or `None`.
///
/// The rounding unit is [`rounding_quantum`], or one time-base unit when none shows. Every
/// timestamp must lie within half a unit of the grid. Among whole-sample periods that fit, the one
/// with the most slack wins (shortest on ties), and the anchor is the middle of its feasible range.
///
/// Returns `None` when half a unit is under a sample (timestamps are already exact), when fewer
/// than three distinct timestamps are given or none fits (variable frame sizes, a missing packet),
/// when no period exceeds twice the trimmed tolerance, or when an argument is not positive.
#[must_use]
pub fn audio_frame_grid(
    timestamps: &[i64],
    num: i64,
    den: i64,
    sample_rate: u32,
) -> Option<AudioFrameGrid> {
    if num <= 0 || den <= 0 || sample_rate == 0 {
        return None;
    }
    let mut sorted = timestamps.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let (&first, &last) = (sorted.first()?, sorted.last()?);
    let count = i128::try_from(sorted.len()).ok()?;
    if count < 3 {
        return None;
    }
    let den = i128::from(den);
    // Samples per time-base unit, times `den`.
    let unit = i128::from(num) * i128::from(sample_rate);
    // Rounding unit in samples, times `den`.
    let rounding = unit * i128::from(rounding_quantum(&sorted).max(1));
    if rounding < 2 * den {
        return None;
    }
    // Twice the trimmed tolerance must stay under the period for a timestamp to name one frame.
    let trimmed_tolerance = (rounding + den).div_euclid(den);
    // The span is off by up to a unit at either end, which bounds the candidate periods.
    let span = (i128::from(last) - i128::from(first)) * unit;
    let frames = count - 1;
    let shortest = (span - rounding)
        .div_euclid(den * frames)
        .max(2 * trimmed_tolerance + 1);
    let longest = (span + rounding).div_euclid(den * frames) + 1;
    let mut best: Option<(i128, i128, i128)> = None;
    for period in shortest..=longest {
        if let Some((low, high)) = fit(&sorted, unit, rounding, den, period)
            && best.is_none_or(|(_, best_low, best_high)| high - low > best_high - best_low)
        {
            best = Some((period, low, high));
        }
    }
    let (period, low, high) = best?;
    let anchor = (low + high + 2 * den).div_euclid(4 * den);
    Some(AudioFrameGrid {
        anchor: i64::try_from(anchor).ok()?,
        period: i64::try_from(period).ok()?,
        start_tolerance: i64::try_from((rounding + 2 * den).div_euclid(2 * den)).ok()?,
        trimmed_tolerance: i64::try_from(trimmed_tolerance).ok()?,
    })
}

/// Feasible first-frame starts (samples times `2 * den`) for a `period` grid within `rounding`
/// of every timestamp, or `None`.
fn fit(
    sorted: &[i64],
    unit: i128,
    rounding: i128,
    den: i128,
    period: i128,
) -> Option<(i128, i128)> {
    let (mut low, mut high) = (i128::MIN, i128::MAX);
    for (index, &timestamp) in sorted.iter().enumerate() {
        let index = i128::try_from(index).ok()?;
        let center = 2 * i128::from(timestamp) * unit - 2 * index * period * den;
        low = low.max(center - rounding);
        high = high.min(center + rounding);
    }
    (low <= high).then_some((low, high))
}

#[cfg(test)]
mod tests {
    use super::{AudioFrameGrid, audio_frame_grid};

    /// Frame timestamps rounded to `1 / den` s, half away from zero like `FFmpeg`'s `av_rescale_q`.
    fn rounded(start: i64, period: i64, rate: i64, den: i64, count: i64) -> Vec<i64> {
        (0..count)
            .map(|k| {
                let scaled = 2 * (start + k * period) * den;
                scaled.signum() * ((scaled.abs() + rate) / (2 * rate))
            })
            .collect()
    }

    #[test]
    fn rounded_timestamps_show_where_their_frames_start() {
        // AAC in Matroska with a priming frame at -1024; a straight decode would start at -1008.
        let aac = rounded(-1_024, 1_024, 48_000, 1_000, 64);
        assert_eq!(&aac[..5], [-21, 0, 21, 43, 64]);
        let grid = audio_frame_grid(&aac, 1, 1_000, 48_000).expect("a grid");
        assert_eq!(
            grid,
            AudioFrameGrid {
                anchor: -1_024,
                period: 1_024,
                start_tolerance: 25,
                trimmed_tolerance: 49,
            }
        );
        // Opus in WebM with libopus's 312-sample pre-skip.
        let opus = rounded(-312, 960, 48_000, 1_000, 64);
        assert_eq!(&opus[..3], [-7, 14, 34]);
        let grid = audio_frame_grid(&opus, 1, 1_000, 48_000).expect("a grid");
        assert_eq!((grid.anchor, grid.period), (-312, 960));
        // At 44.1 kHz a millisecond is 44.1 samples.
        let aac = rounded(-1_024, 1_024, 44_100, 1_000, 64);
        let grid = audio_frame_grid(&aac, 1, 1_000, 44_100).expect("a grid");
        assert_eq!((grid.anchor, grid.period), (-1_024, 1_024));
        assert_eq!((grid.start_tolerance, grid.trimmed_tolerance), (23, 45));
    }

    #[test]
    fn a_first_frame_is_placed_on_the_grid() {
        let grid = audio_frame_grid(&rounded(-312, 960, 48_000, 1_000, 64), 1, 1_000, 48_000)
            .expect("a grid");
        assert_eq!(grid.place(34 * 48, 960), 1_608);
        // A post-seek frame trimmed by 312 samples: 393.5 ms + 6.5 ms, each rounded up, is 401 ms.
        assert_eq!(grid.place(401 * 48, 648), 19_200);
        assert_eq!(grid.place(0, 648), 0);
        // Beyond tolerance, the timestamp is used as is.
        assert_eq!(grid.place(1_608 + 26, 960), 1_634);
        assert_eq!(grid.place(19_200 + 50, 648), 19_250);
    }

    #[test]
    fn timestamps_that_place_frames_exactly_or_unevenly_show_no_grid() {
        // Sample-rate (MP4) and 1/90000 (MPEG-TS) time bases are exact to half a sample.
        let exact: Vec<i64> = (0..64).map(|k| k * 1_024).collect();
        assert_eq!(audio_frame_grid(&exact, 1, 48_000, 48_000), None);
        let ts: Vec<i64> = (0..64).map(|k| 126_000 + k * 1_920).collect();
        assert_eq!(audio_frame_grid(&ts, 1, 90_000, 48_000), None);
        // Vorbis in WebM: long and short blocks, 0, 3, 15, 36, 58 ms.
        assert_eq!(
            audio_frame_grid(&[0, 3, 15, 36, 58, 79, 100], 1, 1_000, 48_000),
            None
        );
        let mut gap = rounded(0, 1_024, 48_000, 1_000, 64);
        gap.remove(10);
        assert_eq!(audio_frame_grid(&gap, 1, 1_000, 48_000), None);
        assert_eq!(audio_frame_grid(&[0, 21], 1, 1_000, 48_000), None);
        assert_eq!(
            audio_frame_grid(&rounded(0, 1_024, 48_000, 50, 64), 1, 50, 48_000),
            None
        );
        assert_eq!(audio_frame_grid(&[0, 21, 43], 0, 1_000, 48_000), None);
        assert_eq!(audio_frame_grid(&[0, 21, 43], 1, 1_000, 0), None);
    }

    #[test]
    fn timestamps_that_keep_milliseconds_in_a_finer_time_base_show_their_grid() {
        // Matroska's milliseconds copied into a 1/48000 time base: multiples of 48.
        let copied: Vec<i64> = rounded(-1_024, 1_024, 48_000, 1_000, 64)
            .into_iter()
            .map(|ms| ms * 48)
            .collect();
        let grid = audio_frame_grid(&copied, 1, 48_000, 48_000).expect("a grid");
        assert_eq!((grid.anchor, grid.period), (-1_024, 1_024));
    }
}
