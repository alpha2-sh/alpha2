use crate::{FrameRateError, MediaTime, TICKS_PER_SECOND};
use core::fmt;
use serde::{Deserialize, Serialize};

/// A positive rational frame rate in reduced form, at most one frame per tick
/// (120 000 frames per second).
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(try_from = "Wire")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FrameRate {
    num: u32,
    den: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(rename = "FrameRate")
)]
/// An exact rational frame rate such as `{ "num": 30000, "den": 1001 }`.
///
/// A frame lasts at least one tick, so `num` is at most 120 000 times `den`
/// (120 000 frames per second); a faster rate does not parse.
struct Wire {
    /// Frames per `den` seconds. Positive, and at most 120 000 times `den`.
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    num: u32,
    /// Seconds per `num` frames. Positive.
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    den: u32,
}
impl TryFrom<Wire> for FrameRate {
    type Error = FrameRateError;
    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        Self::new(wire.num, wire.den)
    }
}
impl FrameRate {
    /// Creates and reduces a positive rate.
    ///
    /// # Errors
    /// Returns an error for zero components or sub-tick frame duration.
    pub const fn new(num: u32, den: u32) -> Result<Self, FrameRateError> {
        if num == 0 {
            return Err(FrameRateError::ZeroNumerator);
        }
        if den == 0 {
            return Err(FrameRateError::ZeroDenominator);
        }
        if (num as u64) > (TICKS_PER_SECOND as u64) * (den as u64) {
            return Err(FrameRateError::ExceedsTimelineResolution);
        }
        let divisor = gcd(num, den);
        Ok(Self {
            num: num / divisor,
            den: den / divisor,
        })
    }
    /// Reduced numerator.
    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.num
    }
    /// Reduced denominator.
    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.den
    }
    /// Exact floored start of a signed frame index, or `None` when it lies
    /// outside the `i64` tick range.
    #[must_use]
    pub fn frame_start(self, frame: i64) -> Option<MediaTime> {
        let value = i128::from(frame) * i128::from(TICKS_PER_SECOND) * i128::from(self.den);
        i64::try_from(value.div_euclid(i128::from(self.num)))
            .ok()
            .map(MediaTime)
    }
    /// Frame whose half-open interval contains `time`, or `None` when the
    /// index does not fit in an `i64`.
    #[must_use]
    pub fn frame_at_or_before(self, time: MediaTime) -> Option<i64> {
        let n = (i128::from(time.0) + 1) * i128::from(self.num);
        let d = i128::from(TICKS_PER_SECOND) * i128::from(self.den);
        i64::try_from(-(-n).div_euclid(d) - 1).ok()
    }
}
impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} fps", self.num, self.den)
    }
}
const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_frame_lasts_at_least_one_tick() {
        assert_eq!(
            FrameRate::new(120_000, 1).map(FrameRate::numerator),
            Ok(120_000)
        );
        assert_eq!(
            FrameRate::new(240_000, 2).map(|rate| (rate.numerator(), rate.denominator())),
            Ok((120_000, 1))
        );
        for (num, den) in [(120_001, 1), (240_001, 2), (u32::MAX, 35_791)] {
            assert_eq!(
                FrameRate::new(num, den),
                Err(FrameRateError::ExceedsTimelineResolution),
                "{num}/{den}"
            );
        }
        assert_eq!(
            FrameRate::new(u32::MAX, 35_792).map(FrameRate::numerator),
            Ok(u32::MAX)
        );
        assert_eq!(FrameRate::new(0, 1), Err(FrameRateError::ZeroNumerator));
        assert_eq!(FrameRate::new(1, 0), Err(FrameRateError::ZeroDenominator));
    }

    #[test]
    fn a_rate_reduces() {
        let rate = FrameRate::new(60, 2).unwrap();
        assert_eq!((rate.numerator(), rate.denominator()), (30, 1));
        assert_eq!(rate, FrameRate::new(30, 1).unwrap());
    }

    #[test]
    fn the_wire_form_parses_only_what_new_accepts() {
        let parse = |json: &str| serde_json::from_str::<FrameRate>(json);
        assert_eq!(
            parse(r#"{"num":60000,"den":2002}"#).unwrap(),
            FrameRate::new(30_000, 1_001).unwrap()
        );
        assert_eq!(
            parse(r#"{"num":120000,"den":1}"#).unwrap(),
            FrameRate::new(120_000, 1).unwrap()
        );
        for (json, message) in [
            (r#"{"num":120001,"den":1}"#, "exceeds timeline resolution"),
            (r#"{"num":240001,"den":2}"#, "exceeds timeline resolution"),
            (r#"{"num":0,"den":1}"#, "numerator must not be zero"),
            (r#"{"num":30,"den":0}"#, "denominator must not be zero"),
            (r#"{"num":4294967296,"den":1}"#, "expected u32"),
            (r#"{"num":30,"den":4294967296}"#, "expected u32"),
            (r#"{"num":-30,"den":1}"#, "expected u32"),
            (r#"{"num":30.0,"den":1}"#, "expected u32"),
            (r#"{"num":30,"den":1,"fps":30}"#, "unknown field `fps`"),
            (r#"{"num":30}"#, "missing field `den`"),
        ] {
            let error = parse(json).expect_err(json).to_string();
            assert!(error.contains(message), "{json}: {error}");
        }
    }

    /// Rates whose frame lasts a whole number of ticks (30/1, 30000/1001)
    /// and rates whose frame does not (the rest), where a frame's exact start
    /// falls between two ticks.
    const RATES: [(u32, u32); 11] = [
        (30, 1),
        (30_000, 1_001),
        (90, 1),
        (72, 1),
        (144, 1),
        (165, 1),
        (48_000, 1_001),
        (7, 1),
        (2_997, 100),
        (119_999, 1),
        (120_000, 1),
    ];

    #[test]
    fn a_frame_starts_at_the_tick_its_exact_start_falls_in() {
        let starts = |num, den| {
            let rate = FrameRate::new(num, den).unwrap();
            (-1..=3)
                .map(|frame| rate.frame_start(frame).unwrap().ticks())
                .collect::<Vec<_>>()
        };
        // 1333 1/3 ticks a frame.
        assert_eq!(starts(90, 1), [-1334, 0, 1333, 2666, 4000]);
        // 2502 1/2 ticks a frame.
        assert_eq!(starts(48_000, 1_001), [-2503, 0, 2502, 5005, 7507]);
        for (num, den) in RATES {
            let rate = FrameRate::new(num, den).unwrap();
            let exact =
                |frame: i64| i128::from(frame) * i128::from(TICKS_PER_SECOND) * i128::from(den);
            for frame in -1000..1000 {
                // start <= exact start < start + 1, in units of 1/num ticks.
                let start = i128::from(rate.frame_start(frame).unwrap().ticks());
                assert!(
                    start * i128::from(num) <= exact(frame)
                        && exact(frame) < (start + 1) * i128::from(num),
                    "{rate}: frame {frame} starts at {start}"
                );
            }
        }
    }

    #[test]
    fn every_tick_of_a_frame_names_that_frame() {
        for (num, den) in RATES {
            let rate = FrameRate::new(num, den).unwrap();
            for frame in -1000..1000 {
                let start = rate.frame_start(frame).unwrap();
                let next = rate.frame_start(frame + 1).unwrap();
                assert!(start < next, "{rate}: frame {frame} lasts no tick");
                for tick in [start.0, start.0 + (next.0 - start.0) / 2, next.0 - 1] {
                    assert_eq!(
                        rate.frame_at_or_before(MediaTime(tick)),
                        Some(frame),
                        "{rate}: tick {tick} of frame {frame} [{start}, {next})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_frame_starts_only_where_its_start_is_a_tick() {
        // At one frame a tick every frame starts on its tick; checked below.
        for (num, den) in RATES.into_iter().filter(|&rate| rate != (120_000, 1)) {
            let rate = FrameRate::new(num, den).unwrap();
            // Every tick is in a frame, at every rate.
            let last = rate.frame_at_or_before(MediaTime(i64::MAX)).unwrap();
            let first = rate.frame_at_or_before(MediaTime(i64::MIN)).unwrap();
            // The last frame starts within the timeline; the one after does not.
            assert!(rate.frame_start(last).is_some(), "{rate}: frame {last}");
            assert_eq!(rate.frame_start(last + 1), None, "{rate}: frame {last} + 1");
            // The first frame starts before the timeline unless it starts on its first tick.
            assert!(
                rate.frame_start(first)
                    .is_none_or(|start| start == MediaTime(i64::MIN)),
                "{rate}: frame {first}"
            );
            assert!(
                rate.frame_start(first + 1).is_some(),
                "{rate}: frame {first} + 1"
            );
            assert_eq!(rate.frame_start(i64::MAX), None, "{rate}");
            assert_eq!(rate.frame_start(i64::MIN), None, "{rate}");
        }
        let fastest = FrameRate::new(120_000, 1).unwrap();
        assert_eq!(
            fastest.frame_at_or_before(MediaTime(i64::MAX)),
            Some(i64::MAX)
        );
        assert_eq!(
            fastest.frame_at_or_before(MediaTime(i64::MIN)),
            Some(i64::MIN)
        );
        assert_eq!(fastest.frame_start(i64::MAX), Some(MediaTime(i64::MAX)));
        assert_eq!(fastest.frame_start(i64::MIN), Some(MediaTime(i64::MIN)));
    }
}
