use core::fmt;

/// Invalid rational frame rate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameRateError {
    /// Zero numerator.
    ZeroNumerator,
    /// Zero denominator.
    ZeroDenominator,
    /// Frame duration is below one tick.
    ExceedsTimelineResolution,
}
impl std::error::Error for FrameRateError {}
impl fmt::Display for FrameRateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroNumerator => "frame-rate numerator must not be zero",
            Self::ZeroDenominator => "frame-rate denominator must not be zero",
            Self::ExceedsTimelineResolution => "frame rate exceeds timeline resolution",
        })
    }
}

/// Invalid time range.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimeRangeError {
    /// End precedes start.
    EndBeforeStart,
}
impl std::error::Error for TimeRangeError {}
impl fmt::Display for TimeRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("time-range end must not precede start")
    }
}
