use core::fmt;
use serde::{Deserialize, Serialize};

/// Fixed alpha2 timeline ticks per second.
pub const TICKS_PER_SECOND: i64 = 120_000;

/// Largest magnitude of a serialized time: 2^53 - 1 ticks (about 2,378 years), the largest
/// integer a JavaScript number holds exactly, so every host reads JSON times the same.
pub const MAX_SERIALIZED_TICKS: i64 = (1 << 53) - 1;

/// [`presentation_slack`] for an unknown time base: 0.5 ms plus one tick (61 ticks), as for
/// milliseconds. Also the cap for streams whose timestamps show no rounding.
pub const PRESENTATION_SLACK: MediaTime = MediaTime(TICKS_PER_SECOND / 2_000 + 1);

/// Most leading packets whose timestamps feed [`rounding_quantum`], also capped by
/// [`ROUNDING_SCAN_BYTES`].
pub const ROUNDING_SCAN_FRAMES: usize = 64;

/// Byte cap on the leading packets read for [`rounding_quantum`]. A native decoder holds the
/// packets it read until it decodes them, so this keeps a 4K stream to about a second.
pub const ROUNDING_SCAN_BYTES: u64 = 16 * 1024 * 1024;

/// The unit a stream's timestamps are rounded to, in time-base units, or 0 when none shows.
///
/// `timestamps` are the stream's first presentation timestamps in any order. Rounding turns a
/// frame interval into two lengths one unit apart (29.97 fps in milliseconds is 33 and 34 ms), so
/// the unit is the GCD of the differences between intervals. Equal intervals mean exact frames.
/// Differences of at least half the shortest interval are a variable frame rate, not rounding.
/// The unit also has to explain the timestamps: every one lies on the first's grid of units, and
/// one constant period, frames skipped allowed, places each within a unit of it. Otherwise
/// (stills held for unequal times, two exact frame rates mixed) the gaps are real and 0 is
/// returned. Fewer than three distinct timestamps return 0.
#[must_use]
pub fn rounding_quantum(timestamps: &[i64]) -> i64 {
    let mut sorted = timestamps.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let intervals = sorted
        .windows(2)
        .map(|pair| i128::from(pair[1]) - i128::from(pair[0]))
        .collect::<Vec<_>>();
    let (Some(&first), Some(&shortest)) = (intervals.first(), intervals.iter().min()) else {
        return 0;
    };
    let quantum = intervals.iter().fold(0_i128, |quantum, &interval| {
        gcd(quantum, (interval - first).abs())
    });
    if quantum == 0
        || quantum.saturating_mul(2) >= shortest
        || !fits_one_period(&sorted, &intervals, shortest, quantum)
    {
        return 0;
    }
    i64::try_from(quantum).unwrap_or(0)
}

/// A numerator and a positive denominator.
type Fraction = (i128, i128);

/// Whether `sorted` is one constant period rounded to `unit`: every timestamp congruent to the
/// first modulo `unit`, and a period `P` with `|(t_k - t_j) - (N_k - N_j) P| <= unit` for every
/// pair, `N` counting frames. An interval is `round(d / p0)` frames, `p0` the mean of the
/// intervals within a unit of the shortest. An overflow means no period.
fn fits_one_period(sorted: &[i64], intervals: &[i128], shortest: i128, unit: i128) -> bool {
    let first = i128::from(sorted[0]);
    if sorted
        .iter()
        .any(|&time| (i128::from(time) - first).rem_euclid(unit) != 0)
    {
        return false;
    }
    let (sum, count) = intervals
        .iter()
        .filter(|&&interval| interval - shortest <= unit)
        .fold((0_i128, 0_i128), |(sum, count), &interval| {
            (sum.saturating_add(interval), count + 1)
        });
    let mut frames = vec![0_i128];
    for &interval in intervals {
        // round(interval * count / sum), halves up.
        let Some(scaled) = interval
            .checked_mul(2 * count)
            .and_then(|twice| twice.checked_add(sum))
        else {
            return false;
        };
        let Some(next) = frames
            .last()
            .and_then(|&last| last.checked_add(scaled / (2 * sum)))
        else {
            return false;
        };
        frames.push(next);
    }
    // The largest lower bound and the smallest upper bound on P, as fractions.
    let (mut low, mut high): (Option<Fraction>, Option<Fraction>) = (None, None);
    let below = |a: Fraction, b: Fraction| -> Option<bool> {
        Some(a.0.checked_mul(b.1)? < b.0.checked_mul(a.1)?)
    };
    for j in 0..sorted.len() {
        for k in j + 1..sorted.len() {
            let span = i128::from(sorted[k]) - i128::from(sorted[j]);
            let count = frames[k] - frames[j];
            if count <= 0 {
                return false;
            }
            let (from, to) = ((span - unit, count), (span + unit, count));
            match low.map(|low| below(low, from)) {
                None | Some(Some(true)) => low = Some(from),
                Some(Some(false)) => {}
                Some(None) => return false,
            }
            match high.map(|high| below(to, high)) {
                None | Some(Some(true)) => high = Some(to),
                Some(Some(false)) => {}
                Some(None) => return false,
            }
        }
    }
    match (low, high) {
        (Some(low), Some(high)) => below(high, low) == Some(false),
        _ => false,
    }
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// How much later than source time `t` a decoded frame's timestamp may be and still be the
/// frame shown at `t`, for a `num / den` time base rounded to `quantum` units
/// ([`rounding_quantum`], 0 for none).
///
/// A frame timestamp is its true time rounded to the stream's unit and then to the tick grid,
/// while sampled times are floored ([`crate::FrameRate::frame_start`]). A 29.97 fps Matroska
/// file stores its third frame (66.73 ms) at 67 ms, 32 ticks after the project frame that shows
/// it. The slack is half the rounding unit (or half the time-base unit when no rounding shows,
/// capped at [`PRESENTATION_SLACK`]) rounded up to a tick, plus one tick. Hosts show the last
/// frame at or before [`MediaTime::presentation_deadline`].
///
/// A non-positive `num` or `den` returns [`PRESENTATION_SLACK`]; a negative `quantum` means none.
#[must_use]
pub fn presentation_slack(num: i64, den: i64, quantum: i64) -> MediaTime {
    if num <= 0 || den <= 0 {
        return PRESENTATION_SLACK;
    }
    let (units, cap) = if quantum > 0 {
        (i128::from(quantum), i128::MAX)
    } else {
        (1, i128::from(PRESENTATION_SLACK.0))
    };
    // Half the rounding in ticks, rounded up: (ticks per second * num * units) / (2 * den).
    let (rounding, halves) = (
        i128::from(TICKS_PER_SECOND)
            .saturating_mul(i128::from(num))
            .saturating_mul(units),
        2 * i128::from(den),
    );
    let half = rounding.saturating_add(halves - 1) / halves;
    let slack = half.saturating_add(1).min(cap);
    MediaTime(i64::try_from(slack).unwrap_or(MAX_SERIALIZED_TICKS))
}

/// A signed position or duration in timeline ticks (120 000 per second).
///
/// Deserialization rejects magnitudes above [`MAX_SERIALIZED_TICKS`].
#[derive(
    Copy, Clone, Debug, Default, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize,
)]
#[serde(try_from = "i64")]
pub struct MediaTime(pub i64);

#[cfg(feature = "schema")]
impl schemars::JsonSchema for MediaTime {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MediaTime".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A signed position or duration in timeline ticks (120 000 per second), at most the largest integer a JavaScript number holds exactly either way.",
            "type": "integer",
            "format": "int64",
            "minimum": -MAX_SERIALIZED_TICKS,
            "maximum": MAX_SERIALIZED_TICKS,
        })
    }
}

impl TryFrom<i64> for MediaTime {
    type Error = String;

    fn try_from(ticks: i64) -> Result<Self, Self::Error> {
        if ticks.unsigned_abs() > MAX_SERIALIZED_TICKS.unsigned_abs() {
            return Err(format!(
                "a time of {ticks} ticks is beyond {MAX_SERIALIZED_TICKS} either way"
            ));
        }
        Ok(Self(ticks))
    }
}

impl MediaTime {
    /// Timeline origin.
    pub const ZERO: Self = Self(0);
    /// Creates a value from ticks.
    #[must_use]
    pub const fn from_ticks(ticks: i64) -> Self {
        Self(ticks)
    }
    /// Returns raw ticks.
    #[must_use]
    pub const fn ticks(self) -> i64 {
        self.0
    }
    /// Latest timestamp a decoded frame may carry and still be shown at this source time.
    #[must_use]
    pub const fn presentation_deadline(self, slack: Self) -> Self {
        Self(self.0.saturating_add(slack.0))
    }
    /// Creates a value from whole seconds, saturating at the `i64` bounds.
    #[must_use]
    pub const fn from_seconds(seconds: i64) -> Self {
        Self(seconds.saturating_mul(TICKS_PER_SECOND))
    }
    /// Checked addition.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
    /// Checked subtraction.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl fmt::Display for MediaTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ticks", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seconds_and_overflow() {
        assert_eq!(MediaTime::from_seconds(1).ticks(), TICKS_PER_SECOND);
        assert_eq!(MediaTime(i64::MAX).checked_add(MediaTime(1)), None);
    }

    #[test]
    fn a_time_base_bounds_how_late_a_shown_frame_may_be() {
        // Milliseconds round by up to 60 ticks; 1/90000 by two thirds of one; 1/30000 by two.
        assert_eq!(presentation_slack(1, 1_000, 0), MediaTime(61));
        assert_eq!(presentation_slack(1, 90_000, 0), MediaTime(2));
        assert_eq!(presentation_slack(1, 30_000, 0), MediaTime(3));
        assert_eq!(presentation_slack(1, 15_360, 0), MediaTime(5));
        assert_eq!(presentation_slack(1, TICKS_PER_SECOND, 0), MediaTime(2));
        // A coarse unit with no rounding is capped at the millisecond slack.
        assert_eq!(presentation_slack(1_001, 30_000, 0), PRESENTATION_SLACK);
        assert_eq!(presentation_slack(1, 600, 0), PRESENTATION_SLACK);
        assert_eq!(presentation_slack(0, 1, 0), PRESENTATION_SLACK);
        assert_eq!(presentation_slack(1, 0, 0), PRESENTATION_SLACK);
        assert_eq!(presentation_slack(1, 0, 16), PRESENTATION_SLACK);
        assert_eq!(
            MediaTime(8_008).presentation_deadline(presentation_slack(1, 1_000, 0)),
            MediaTime(8_069)
        );
    }

    #[test]
    fn a_rounding_the_timestamps_show_bounds_it_however_coarse() {
        // An MP4 that kept a Matroska file's milliseconds in 1/16000, 29.97 fps in 1/600 and 1/100.
        assert_eq!(presentation_slack(1, 16_000, 16), MediaTime(61));
        assert_eq!(presentation_slack(1, 600, 1), MediaTime(101));
        assert_eq!(presentation_slack(1, 100, 1), MediaTime(601));
        assert_eq!(presentation_slack(1, 1_000, 1), MediaTime(61));
        assert_eq!(presentation_slack(1, 90_000, 90), MediaTime(61));
        assert_eq!(presentation_slack(1, 1_000, -1), MediaTime(61));
        assert_eq!(
            presentation_slack(i64::MAX, 1, i64::MAX),
            MediaTime(MAX_SERIALIZED_TICKS)
        );
    }

    /// The first `count` frame starts of `num / den` fps, rounded to the nearest of `1 / base` s.
    fn rounded(num: i64, den: i64, base: i64, count: i64) -> Vec<i64> {
        (0..count)
            .map(|frame| (2 * frame * den * base + num) / (2 * num))
            .collect()
    }

    #[test]
    fn the_spacing_of_the_first_timestamps_shows_their_rounding() {
        // 29.97 fps in milliseconds (33, 34), the same kept in 1/16000 (528, 544), in 1/600
        // (20, 21) and in 1/100 (3, 4).
        assert_eq!(rounding_quantum(&rounded(30_000, 1_001, 1_000, 64)), 1);
        let remuxed = rounded(30_000, 1_001, 1_000, 64)
            .into_iter()
            .map(|ms| ms * 16)
            .collect::<Vec<_>>();
        assert_eq!(rounding_quantum(&remuxed), 16);
        assert_eq!(rounding_quantum(&rounded(30_000, 1_001, 600, 64)), 1);
        assert_eq!(rounding_quantum(&rounded(30_000, 1_001, 100, 64)), 1);
        // Exact frames: 30 fps in 1/600, 25 fps in milliseconds, 29.97 in 1001/30000.
        assert_eq!(rounding_quantum(&rounded(30, 1, 600, 64)), 0);
        assert_eq!(rounding_quantum(&rounded(25, 1, 1_000, 64)), 0);
        assert_eq!(rounding_quantum(&(0..64).collect::<Vec<_>>()), 0);
        // 60 and 30 fps frames in an exact 1/90000 time base are a variable rate, not a rounding.
        assert_eq!(
            rounding_quantum(&[0, 1_500, 3_000, 6_000, 7_500, 10_500]),
            0
        );
        // A screen recording in milliseconds: 60 and 30 fps frames, each rounded.
        assert_eq!(rounding_quantum(&[0, 17, 33, 67, 83, 100, 133, 150]), 1);
        // Decode order, repeats, and too few frames to show a spacing.
        assert_eq!(rounding_quantum(&[67, 0, 33, 33, 100, 133]), 1);
        assert_eq!(rounding_quantum(&[0, 33]), 0);
        assert_eq!(rounding_quantum(&[]), 0);
        assert_eq!(rounding_quantum(&[i64::MIN, 0, i64::MAX]), 1);
    }

    #[test]
    fn a_coarse_unit_counts_only_when_one_period_explains_the_timestamps() {
        // Stills held 3, 3, 3, 4, 4 and 4 s: gaps of whole seconds, not a rounding.
        let stills = |units: i64| {
            [0, 3, 6, 9, 13, 17, 21]
                .iter()
                .map(|second| second * units)
                .collect::<Vec<_>>()
        };
        assert_eq!(rounding_quantum(&stills(90_000)), 0);
        assert_eq!(rounding_quantum(&stills(1_000)), 0);
        // Exact 29.97 and 23.976 fps frames mixed in 1/90000 (3003 and 3754).
        assert_eq!(
            rounding_quantum(&[0, 3_003, 6_757, 9_760, 13_514, 16_517]),
            0
        );
        // Stills at 0, 3, 7, 10 and 14 s are also a 3.5 s period rounded to seconds.
        assert_eq!(
            rounding_quantum(&[0, 270_000, 630_000, 900_000, 1_260_000]),
            90_000
        );
        // AAC in Matroska copied into a 1/48000 track: 0, 1024, then every frame 8 short.
        let remux = [0, 1_024]
            .into_iter()
            .chain((2..64).map(|k| k * 1_024 - 8))
            .collect::<Vec<_>>();
        assert_eq!(rounding_quantum(&remux), 8);
    }
}
