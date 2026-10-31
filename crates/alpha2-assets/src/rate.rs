//! Frame-rate measurement for fingerprints, identical on every host.
//!
//! Declared rates are unreliable (browser demuxers report none, and `FFmpeg` reports the base
//! rate, which is wrong for variable-rate media), so every host measures the presentation rate
//! near the start from packet timestamps alone and rounds it with [`rational_rate`].
//! `tests/cases/media-evidence-corpus.json` pins the expected results for all hosts.

use alpha2_time::FrameRate;

/// Measures a frame rate from packet presentation times, fed in storage order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRateEvidence {
    count: usize,
    first: f64,
    last: f64,
}

impl Default for FrameRateEvidence {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameRateEvidence {
    /// Minimum number of frames measured (about four seconds at 30 fps).
    pub const FRAMES: usize = 120;

    /// Creates an empty measurement.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: 0,
            first: f64::INFINITY,
            last: f64::NEG_INFINITY,
        }
    }

    /// Adds a packet's presentation time in seconds and returns whether more are wanted.
    ///
    /// After [`Self::FRAMES`] packets it stops at the first packet later than all seen, so
    /// reordered frames still fall inside the span.
    pub fn push(&mut self, seconds: f64) -> bool {
        if !seconds.is_finite() {
            return true;
        }
        if self.count >= Self::FRAMES && seconds > self.last {
            return false;
        }
        self.first = self.first.min(seconds);
        self.last = self.last.max(seconds);
        self.count += 1;
        true
    }

    /// The measured rate rounded by [`rational_rate`], or `None` with fewer than two frames or a
    /// zero span.
    #[must_use]
    pub fn rate(&self) -> Option<FrameRate> {
        let span = self.last - self.first;
        if self.count < 2 || span.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        #[allow(clippy::cast_precision_loss)] // A packet count is far below 2^52.
        let frames = (self.count - 1) as f64;
        rational_rate(frames / span)
    }
}

/// Rounds a measured rate to the rational a stream would declare.
///
/// NTSC and whole rates are matched exactly, others are rounded to a thousandth. Returns `None`
/// for rates a project cannot hold, which only broken timestamps produce.
#[must_use]
pub fn rational_rate(measured: f64) -> Option<FrameRate> {
    if !measured.is_finite() || measured <= 0.0 {
        return None;
    }
    for base in [24.0, 30.0, 48.0, 60.0, 120.0] {
        if (measured - base * 1_000.0 / 1_001.0).abs() < 0.005 {
            return whole(base * 1_000.0).and_then(|num| FrameRate::new(num, 1_001).ok());
        }
    }
    let rounded = measured.round();
    let (num, den) = if (measured - rounded).abs() < 0.005 {
        (rounded, 1)
    } else {
        ((measured * 1_000.0).round(), 1_000)
    };
    whole(num).and_then(|num| FrameRate::new(num, den).ok())
}

fn whole(value: f64) -> Option<u32> {
    (value >= 0.0 && value <= f64::from(u32::MAX)).then(|| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // In range, and whole.
        let whole = value as u32;
        whole
    })
}
