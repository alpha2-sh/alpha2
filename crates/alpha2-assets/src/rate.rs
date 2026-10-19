//! A video's frame rate as a fingerprint records it, measured the same way on
//! every host so that two hosts' evidence of the same bytes agrees.
//!
//! A declared rate is no common ground: a browser demuxer reports none, and
//! `FFmpeg`'s guess is the stream's base rate, which for variable-rate media
//! is far from how often frames come. Every host instead measures the rate
//! its frames are presented at near the start, from the packets' times
//! alone (durations differ between demuxers), and rounds it to the rational
//! a stream would declare ([`rational_rate`]). The browser runtime's
//! `packages/alpha2-editor/src/browser/runtime/fingerprint.js` is this
//! module's twin; `tests/cases/media-evidence-corpus.json` holds both to the same
//! answers.

use alpha2_time::FrameRate;

/// Measures a frame rate from packets' presentation times, taken in the
/// order a file stores the packets.
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
    /// How many frames the measure spans, at least: about four seconds at
    /// 30 fps.
    pub const FRAMES: usize = 120;

    /// No packets yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: 0,
            first: f64::INFINITY,
            last: f64::NEG_INFINITY,
        }
    }

    /// Takes the next packet's presentation time in seconds. Returns whether
    /// the measure wants more: once it has [`Self::FRAMES`] it stops at the
    /// first packet presented after every one taken, so frames stored out of
    /// presentation order are not left out of the span.
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

    /// The frames taken over the span from the first presentation to the
    /// last, as [`rational_rate`] rounds it; `None` with fewer than two
    /// frames, or with no span between them.
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

/// A measured frame rate as the rational a stream declares: the NTSC rates
/// and whole rates exactly, others to a thousandth. `None` when the measure
/// is no rate a project holds: one that rounds to no frames, or a frame
/// shorter than a tick, which only broken timestamps give.
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

/// `value`, a whole number, as a `u32`, if it is one.
fn whole(value: f64) -> Option<u32> {
    (value >= 0.0 && value <= f64::from(u32::MAX)).then(|| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // In range, and whole.
        let whole = value as u32;
        whole
    })
}
