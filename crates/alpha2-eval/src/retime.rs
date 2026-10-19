use alpha2_model::{LoopMode, TimeMap};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

/// The frames a reversed clip, or a ping-pong loop's backward pass, mirrors
/// by: `ticks / per` timeline ticks each, frame `k` starting at
/// `floor(k * ticks / per)` as the project's frames do.
///
/// Pictures mirror by project frame, so a reversed clip shows the forward
/// clip's frames in the opposite order: at the start of each frame, the
/// instant the forward clip shows at the start of the mirrored frame, at any
/// rate and wherever the clip starts and ends. Sound mirrors by tick, as
/// close as ticks come to the sample the mixer mirrors by.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Mirror {
    ticks: i128,
    per: i128,
}

impl Mirror {
    /// One tick.
    pub(crate) const TICK: Self = Self { ticks: 1, per: 1 };

    /// One frame at `rate`.
    pub(crate) fn frame(rate: FrameRate) -> Self {
        Self {
            ticks: i128::from(TICKS_PER_SECOND) * i128::from(rate.denominator()),
            per: i128::from(rate.numerator()),
        }
    }

    /// The tick frame `frame` starts at.
    fn start_of(self, frame: i128) -> Option<i128> {
        Some(frame.checked_mul(self.ticks)?.div_euclid(self.per))
    }

    /// The frame containing tick `tick`: the last starting at or before it.
    fn frame_of(self, tick: i128) -> Option<i128> {
        Some(
            tick.checked_add(1)?
                .checked_mul(self.per)?
                .checked_sub(1)?
                .div_euclid(self.ticks),
        )
    }

    /// The forward offset `floor(e' * scale / length)` the mirror of
    /// `elapsed` shows, within a window of `length` ticks starting at
    /// timeline tick `start`.
    ///
    /// With `k0` and `k1` the first and last frames starting in the window
    /// and `k` the frame containing `start + elapsed` (`k0` before it, with
    /// no time into it), `e'` is `start_of(k0 + k1 - k)` less the time into
    /// `k`, from the window's start, or zero where that falls before it. A
    /// window no frame starts in shows its start.
    fn offset(self, start: i128, length: i128, elapsed: i128, scale: i128) -> Option<i128> {
        let first = self.frame_of(start.checked_sub(1)?)?.checked_add(1)?;
        let last = self.frame_of(start.checked_add(length)?.checked_sub(1)?)?;
        if first > last {
            return Some(0);
        }
        let tick = start.checked_add(elapsed)?;
        let frame = self.frame_of(tick)?;
        let (frame, into) = if frame < first {
            (first, 0)
        } else {
            (frame, tick.checked_sub(self.start_of(frame)?)?)
        };
        let mirrored = self
            .start_of(first.checked_add(last)?.checked_sub(frame)?)?
            .checked_sub(into)?
            .checked_sub(start)?;
        if mirrored <= 0 {
            return Some(0);
        }
        Some(mirrored.checked_mul(scale)?.div_euclid(length))
    }
}

/// The source instant a clip shows at timeline `time`, in whole ticks.
///
/// With `e = time - timeline.start`, `T` the timeline duration, `S` the
/// source duration, and `mirror` the frames reversing mirrors by:
///
/// - forward: `source.start + floor(e * S / T)`;
/// - reverse: the forward instant at the mirrored frame, less the time into
///   the frame ([`Mirror`]), and the source start where that falls before
///   zero;
/// - freeze: its instant;
/// - a repeating loop: `source.start + (e mod S)`;
/// - a ping-pong loop: the same on even passes, and on odd ones the pass
///   mirrored in the same way.
///
/// Returns `None` for an empty range or a value that does not fit in ticks.
pub(crate) fn map_source_time(
    timeline: TimeRange,
    source: TimeRange,
    time_map: TimeMap,
    mirror: Mirror,
    time: MediaTime,
) -> Option<MediaTime> {
    let timeline_duration = i128::from(timeline.duration()?.ticks());
    let source_duration = i128::from(source.duration()?.ticks());
    if timeline_duration == 0 || source_duration == 0 {
        return None;
    }
    let elapsed = i128::from(time.ticks()) - i128::from(timeline.start().ticks());
    let offset = match time_map {
        TimeMap::Forward => elapsed
            .checked_mul(source_duration)?
            .div_euclid(timeline_duration),
        TimeMap::Reverse => mirror.offset(
            i128::from(timeline.start().ticks()),
            timeline_duration,
            elapsed,
            source_duration,
        )?,
        TimeMap::Freeze { source_time } => return Some(source_time),
        TimeMap::Loop { mode } => {
            let within = elapsed.rem_euclid(source_duration);
            let pass = elapsed.div_euclid(source_duration);
            match mode {
                LoopMode::PingPong if pass.rem_euclid(2) == 1 => mirror.offset(
                    i128::from(timeline.start().ticks())
                        .checked_add(pass.checked_mul(source_duration)?)?,
                    source_duration,
                    within,
                    source_duration,
                )?,
                LoopMode::Repeat | LoopMode::PingPong => within,
            }
        }
    };
    let mapped = i128::from(source.start().ticks()).checked_add(offset)?;
    i64::try_from(mapped).ok().map(MediaTime::from_ticks)
}

/// The source instant a clip's picture shows at timeline `time`: its time
/// map read with the reverse and ping-pong mirror of one project frame at
/// `frame_rate`, exactly as evaluation draws it.
///
/// Editors that key a clip's source composition, or show where a clip reads
/// its source, take the time from here. `tests/golden/time-rules-corpus.json` pins
/// it for the TypeScript port.
///
/// Returns `None` for an empty range or a value that does not fit in ticks.
#[must_use]
pub fn picture_source_time(
    timeline: TimeRange,
    source: TimeRange,
    time_map: TimeMap,
    frame_rate: FrameRate,
    time: MediaTime,
) -> Option<MediaTime> {
    map_source_time(timeline, source, time_map, Mirror::frame(frame_rate), time)
}
