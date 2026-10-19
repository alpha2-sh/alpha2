use crate::{MediaTime, TimeRangeError};
use serde::{Deserialize, Serialize};

/// A half-open timeline interval `[start, end)`.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(try_from = "Wire")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimeRange {
    start: MediaTime,
    end: MediaTime,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(rename = "TimeRange")
)]
/// A half-open interval `[start, end)` in timeline ticks; `end` may not precede `start`.
struct Wire {
    /// Inclusive start.
    start: MediaTime,
    /// Exclusive end.
    end: MediaTime,
}
impl TryFrom<Wire> for TimeRange {
    type Error = TimeRangeError;
    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        Self::new(wire.start, wire.end)
    }
}
impl TimeRange {
    /// Creates an ordered range.
    ///
    /// # Errors
    /// Returns an error if end precedes start.
    pub const fn new(start: MediaTime, end: MediaTime) -> Result<Self, TimeRangeError> {
        if end.0 < start.0 {
            Err(TimeRangeError::EndBeforeStart)
        } else {
            Ok(Self { start, end })
        }
    }
    /// Inclusive start.
    #[must_use]
    pub const fn start(self) -> MediaTime {
        self.start
    }
    /// Exclusive end.
    #[must_use]
    pub const fn end(self) -> MediaTime {
        self.end
    }
    /// Whether the range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.0 == self.end.0
    }
    /// Checked duration.
    #[must_use]
    pub const fn duration(self) -> Option<MediaTime> {
        self.end.checked_sub(self.start)
    }
    /// Half-open membership.
    #[must_use]
    pub const fn contains(self, time: MediaTime) -> bool {
        self.start.0 <= time.0 && time.0 < self.end.0
    }
    /// Whether the ranges share an instant. An empty range holds none, so it
    /// intersects nothing; ranges that only touch share none either.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.intersection(other).is_some()
    }
    /// Their non-empty intersection.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Option<Self> {
        let start = if self.start.0 > other.start.0 {
            self.start
        } else {
            other.start
        };
        let end = if self.end.0 < other.end.0 {
            self.end
        } else {
            other.end
        };
        if start.0 < end.0 {
            Some(Self { start, end })
        } else {
            None
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn half_open() {
        let range = TimeRange::new(MediaTime(1), MediaTime(2)).unwrap();
        assert!(range.contains(MediaTime(1)));
        assert!(!range.contains(MediaTime(2)));
    }

    #[test]
    fn ranges_intersect_where_they_share_an_instant() {
        let range = |start, end| TimeRange::new(MediaTime(start), MediaTime(end)).unwrap();
        // An empty range holds no instant, so it meets nothing, even inside another.
        assert!(!range(5, 5).intersects(range(0, 10)));
        assert!(!range(0, 10).intersects(range(5, 5)));
        assert!(!range(0, 0).intersects(range(0, 10)));
        assert!(!range(5, 5).intersects(range(5, 5)));
        // Ranges that only touch share no instant.
        assert!(!range(0, 5).intersects(range(5, 10)));
        assert!(range(0, 6).intersects(range(5, 10)));
        assert!(range(2, 3).intersects(range(0, 10)));
        let ranges = (-3..4)
            .flat_map(|start| (start..4).map(move |end| range(start, end)))
            .collect::<Vec<_>>();
        for &a in &ranges {
            for &b in &ranges {
                let shared = (-3..4).any(|t| a.contains(MediaTime(t)) && b.contains(MediaTime(t)));
                assert_eq!(a.intersects(b), shared, "{a:?} {b:?}");
                assert_eq!(a.intersection(b).is_some(), shared, "{a:?} {b:?}");
            }
        }
    }
}
