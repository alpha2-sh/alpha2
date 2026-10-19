//! Where an audio stream's frames start when its container rounds their
//! timestamps.

use crate::media_time::rounding_quantum;

/// The source samples an audio stream's frames start at, read from its first
/// packets' timestamps ([`audio_frame_grid`]): every `period` samples from
/// `anchor`, at the stream's rate.
///
/// A container that rounds timestamps (Matroska and `WebM` to the millisecond)
/// stores a frame up to half its unit from where the frame starts, so the
/// timestamp alone says where a frame starts only to that half unit, which
/// is 24 samples at 48 kHz. A decode straight through places each frame
/// where the one before it ended and is off by whatever the first timestamp
/// was; a seek places the first frame after it by that frame's own timestamp
/// and is off by another amount. A codec whose frames are all one length
/// (AAC's 1024 samples, Opus's 20 ms) puts every frame on one grid, and the
/// first timestamps together say where it lies far more closely than any
/// one of them: placing a first frame on it gives the samples a decode
/// straight through gives, however it is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrameGrid {
    /// A source sample, at the stream's rate, that a frame starts at.
    pub anchor: i64,
    /// The samples in every frame.
    pub period: i64,
    /// The farthest a frame's timestamp, as a sample, is from the start it
    /// stands for: half the rounding unit and the half sample of rounding it
    /// to a sample, rounded down to a whole sample after adding one.
    pub start_tolerance: i64,
    /// The farthest the end of a frame shorter than `period` is from the
    /// grid: a decoder that drops a codec's delay from the first frame after
    /// a start or a seek (Opus's pre-skip) moves its timestamp by the
    /// samples dropped, rounded to the unit again, so twice the half unit
    /// and a sample.
    pub trimmed_tolerance: i64,
}

impl AudioFrameGrid {
    /// Where a frame of `frames` samples whose timestamp puts it at source
    /// sample `at` starts: on the grid when its timestamp is within the
    /// grid's rounding of it, otherwise at `at`.
    ///
    /// A frame shorter than [`Self::period`] is first taken as the end of a
    /// frame a decoder dropped the start of (Opus's pre-skip, after a start
    /// or a seek): it ends on the grid. Otherwise a frame starts on it.
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

    /// What moves `position` onto the nearest grid sample.
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

/// The frame grid an audio stream's first packet timestamps show, at
/// `sample_rate`, in a stream whose timestamps are whole multiples of
/// `num / den` seconds; `None` when they show none to place frames on.
///
/// `timestamps` are the stream's first packets' presentation timestamps in
/// any order: its first [`crate::ROUNDING_SCAN_FRAMES`] packets', fewer when
/// those before hold [`crate::ROUNDING_SCAN_BYTES`], as for
/// [`rounding_quantum`]. Their rounding unit is that one's quantum, or one
/// unit of the time base when they show none. The grid is one every
/// timestamp is within half a unit of: of the whole-sample periods within a
/// unit over their count of their span over it, the one that leaves the most
/// room, the shortest of those that leave as much; and of the anchors it
/// leaves, the one at the middle of where they can lie, rounded to a sample,
/// which for a rounding to the nearest unit is where the frames are. There
/// is none when:
///
/// - half the unit is under a sample (an MP4's time base is usually the
///   sample rate, MPEG-TS's 1/90000): a timestamp is then where its frame
///   starts;
/// - fewer than three distinct timestamps are given, or no grid holds them
///   all: the frames are not one length (Vorbis's long and short blocks, an
///   Opus stream that changes frame size), or a packet is missing;
/// - no period longer than twice the trimmed tolerance holds them, so a
///   timestamp could stand for either of two frames;
/// - `num`, `den` or `sample_rate` is not positive.
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
    // Samples of a time-base unit, times `den`.
    let unit = i128::from(num) * i128::from(sample_rate);
    // The rounding unit in samples, times `den`: twice the half unit.
    let rounding = unit * i128::from(rounding_quantum(&sorted).max(1));
    if rounding < 2 * den {
        return None;
    }
    // Twice the trimmed tolerance must stay under the period for a timestamp to name one frame.
    let trimmed_tolerance = (rounding + den).div_euclid(den);
    // The span is off by up to a unit at either end, so the period is within a unit over the
    // frames of the span over them. Of the lengths there, the grid is the one that holds every
    // timestamp with the most room, the shortest of those with as much.
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

/// Where the first frame of a grid of `period` samples that holds every one
/// of `sorted` can start, in samples times `2 * den`: each timestamp, less
/// the frames before it, within `rounding` (half a unit times `2 * den`) of
/// it. `None` when no start holds them all.
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

    /// Timestamps of `count` frames of `period` samples at `rate` from sample
    /// `start`, rounded to the nearest `1 / den` s, a half away from zero
    /// (as `FFmpeg`'s `av_rescale_q` rounds).
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
        // AAC in Matroska: 1024-sample frames from -1024 (the priming frame), stored at -21, 0, 21,
        // 43, 64 ms. A decode straight through would place every frame 16 samples late
        // (from -1008).
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
        // Opus in WebM, libopus's 312-sample pre-skip: 960-sample packets from -312 at -7, 14,
        // 34 ms.
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
        // The first frame after a seek that the decoder dropped 312 samples of: its packet at
        // 394 ms (18 888 samples, 393.5 ms) and the 6.5 ms it dropped, both rounded up, put it at
        // 401 ms.
        assert_eq!(grid.place(401 * 48, 648), 19_200);
        // The stream's own first frame, the same 312 samples dropped: at 0, where it ends on 648.
        assert_eq!(grid.place(0, 648), 0);
        // A timestamp farther from the grid than its rounding is where the frame is.
        assert_eq!(grid.place(1_608 + 26, 960), 1_634);
        assert_eq!(grid.place(19_200 + 50, 648), 19_250);
    }

    #[test]
    fn timestamps_that_place_frames_exactly_or_unevenly_show_no_grid() {
        // A time base of the sample rate (MP4) and MPEG-TS's 1/90000 put a frame within half a
        // sample.
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
        // Two timestamps say nothing of a grid, and a unit near a frame's length could name two.
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
