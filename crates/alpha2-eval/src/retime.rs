use alpha2_model::{LoopMode, TimeMap};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

/// Frame grid that reverse playback and ping-pong backward passes mirror by: frame `k` starts
/// at `floor(k * ticks / per)`, like project frames.
///
/// Pictures mirror by project frame, so a reversed clip shows the forward clip's frames in
/// reverse order at any rate and clip bounds. Sound mirrors by tick, the closest ticks get to the
/// mixer's per-sample mirror.
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

    fn start_of(self, frame: i128) -> Option<i128> {
        Some(frame.checked_mul(self.ticks)?.div_euclid(self.per))
    }

    fn frame_of(self, tick: i128) -> Option<i128> {
        Some(
            tick.checked_add(1)?
                .checked_mul(self.per)?
                .checked_sub(1)?
                .div_euclid(self.ticks),
        )
    }

    /// Forward offset `floor(e' * scale / length)` shown by the mirror of `elapsed` in a window
    /// of `length` ticks from `start`.
    ///
    /// With `k0`/`k1` the first/last frames starting in the window and `k` the frame containing
    /// `start + elapsed` (clamped to `k0`), `e'` is `start_of(k0 + k1 - k)` minus the time into
    /// `k`, relative to the window start and floored at zero. A window with no frame start gives 0.
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
/// - reverse: the forward instant at the mirrored frame less the time into the frame
///   ([`Mirror`]), clamped to the source start;
/// - freeze: its instant;
/// - a repeating loop: `source.start + (e mod S)`;
/// - a ping-pong loop: the same on even passes, mirrored on odd ones.
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

/// The source instant a clip's picture shows at timeline `time`, mirroring by project frames at
/// `frame_rate` exactly as evaluation draws it.
///
/// `tests/golden/time-rules-corpus.json` pins this for the TypeScript port.
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

/// One stretch of a clip's timeline over which the shown source time moves one way.
#[derive(Copy, Clone)]
enum Direction {
    Rising,
    Falling,
    Still,
}

/// The stretches of `timeline` over which `time_map` moves the shown source time one way: the
/// whole clip, or one per loop pass.
fn monotone_stretches(
    timeline: TimeRange,
    source: TimeRange,
    time_map: TimeMap,
) -> Option<Vec<(i64, i64, Direction)>> {
    let (start, end) = (timeline.start().ticks(), timeline.end().ticks());
    let source_length = source.duration()?.ticks();
    if start >= end || source_length <= 0 {
        return Some(Vec::new());
    }
    Some(match time_map {
        TimeMap::Forward => vec![(start, end, Direction::Rising)],
        TimeMap::Reverse => vec![(start, end, Direction::Falling)],
        TimeMap::Freeze { .. } => vec![(start, end, Direction::Still)],
        TimeMap::Loop { mode } => {
            let mut stretches = Vec::new();
            let mut pass = 0_i64;
            let mut from = start;
            while from < end {
                let to = from.checked_add(source_length)?.min(end);
                let direction = if mode == LoopMode::PingPong && pass % 2 == 1 {
                    Direction::Falling
                } else {
                    Direction::Rising
                };
                stretches.push((from, to, direction));
                from = to;
                pass += 1;
            }
            stretches
        }
    })
}

/// The first tick in `from..to` at which `reached` holds, for a `reached` that never stops
/// holding once it does; `to` when it never holds.
fn first_reached(from: i64, to: i64, reached: impl Fn(i64) -> bool) -> i64 {
    let (mut low, mut high) = (from, to);
    while low < high {
        let middle = low + (high - low) / 2;
        if reached(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

/// The timeline ranges of a clip at which its picture shows a source time in
/// `[range_start, range_end)`: the full preimage of [`picture_source_time`].
///
/// Forward and stretched clips give one range, reversed clips one mirrored range, loops one per
/// pass that reaches the interval, and a freeze the whole clip when its instant lies inside the
/// interval. Ranges are in timeline order and disjoint. `None` for a time that does not fit in
/// ticks.
#[must_use]
pub fn source_range_to_timeline(
    timeline: TimeRange,
    source: TimeRange,
    time_map: TimeMap,
    frame_rate: FrameRate,
    range_start: MediaTime,
    range_end: MediaTime,
) -> Option<Vec<TimeRange>> {
    if range_start >= range_end {
        return Some(Vec::new());
    }
    let shown = |tick: i64| {
        picture_source_time(timeline, source, time_map, frame_rate, MediaTime(tick))
            .map_or(i64::MIN, MediaTime::ticks)
    };
    let (low, high) = (range_start.ticks(), range_end.ticks());
    let mut ranges: Vec<TimeRange> = Vec::new();
    for (from, to, direction) in monotone_stretches(timeline, source, time_map)? {
        let (first, last) = match direction {
            Direction::Rising => (
                first_reached(from, to, |tick| shown(tick) >= low),
                first_reached(from, to, |tick| shown(tick) >= high),
            ),
            Direction::Falling => (
                first_reached(from, to, |tick| shown(tick) < high),
                first_reached(from, to, |tick| shown(tick) < low),
            ),
            Direction::Still => {
                if (low..high).contains(&shown(from)) {
                    (from, to)
                } else {
                    (from, from)
                }
            }
        };
        if first < last {
            let range = TimeRange::new(MediaTime(first), MediaTime(last)).ok()?;
            match ranges.last_mut() {
                Some(previous) if previous.end() == range.start() => {
                    *previous = TimeRange::new(previous.start(), range.end()).ok()?;
                }
                _ => ranges.push(range),
            }
        }
    }
    Some(ranges)
}

/// The timeline times at which a clip's picture reaches source time `instant`: per stretch over
/// which the shown time moves one way, the first tick at which it has reached `instant` in that
/// stretch's direction, if the stretch passes it. A freeze passes nothing.
#[must_use]
pub fn source_instant_to_timeline(
    timeline: TimeRange,
    source: TimeRange,
    time_map: TimeMap,
    frame_rate: FrameRate,
    instant: MediaTime,
) -> Option<Vec<MediaTime>> {
    let shown = |tick: i64| {
        picture_source_time(timeline, source, time_map, frame_rate, MediaTime(tick))
            .map_or(i64::MIN, MediaTime::ticks)
    };
    let at = instant.ticks();
    let mut times = Vec::new();
    for (from, to, direction) in monotone_stretches(timeline, source, time_map)? {
        let found = match direction {
            Direction::Rising if shown(from) <= at => {
                first_reached(from, to, |tick| shown(tick) >= at)
            }
            Direction::Falling if shown(from) >= at => {
                first_reached(from, to, |tick| shown(tick) <= at)
            }
            _ => to,
        };
        if found < to {
            times.push(MediaTime(found));
        }
    }
    Some(times)
}
