//! How fast a device works through predicted cost: milliseconds per unit of
//! effect cost per pixel.
//!
//! Renderers split a frame's work into submissions that each stay under the
//! watchdog's budget ([`crate::submission_budget_ms`]), predicting each pass's
//! time as its cost in units, times the pixels it covers, times this rate. A
//! short calibration draw seeds it, at twice what it measured; every heavy
//! frame's measured GPU time then lowers it at most a tenth per frame when
//! the device turns out faster, never below what the calibration itself
//! measured.
//!
//! A frame's GPU time is measured from its completion callback, which runs
//! when the host next polls the device: an upper bound that grows with
//! whatever the host did meanwhile (a slow software encode, a decode wait),
//! by hundreds of times on a fast GPU drawing a light frame. So a measurement
//! can show the rate is too high, never that it is too low; raising the rate
//! on one would cut light frames into many submissions for nothing. No pixel
//! depends on the rate: only where the work is cut into submissions.

use std::sync::Mutex;

/// The rate assumed until a calibration lands, in milliseconds per unit
/// pixel: twice the slowest rate a calibration seeded on the GPUs measured
/// (integrated ones), so a browser frame drawn while its calibration is
/// still in flight is cut too finely, not too coarsely.
pub const UNCALIBRATED_MS_PER_UNIT_PIXEL: f64 = 2.0e-8;

#[derive(Debug, Default)]
struct RateState {
    /// The rate predictions use; `None` until seeded.
    ms_per_unit_pixel: Option<f64>,
    /// What the calibration draw measured: the floor of the rate.
    calibrated: Option<f64>,
    /// A calibration was started and has not landed.
    calibrating: bool,
}

/// One device's rate, shared by every context clone and renderer on it.
#[derive(Debug, Default)]
pub struct GpuRate {
    state: Mutex<RateState>,
}

impl GpuRate {
    fn state(&self) -> std::sync::MutexGuard<'_, RateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The rate to predict with, in milliseconds per unit pixel, once a
    /// calibration seeded it.
    #[must_use]
    pub fn ms_per_unit_pixel(&self) -> Option<f64> {
        self.state().ms_per_unit_pixel
    }

    /// Whether the caller should run the calibration draw now: true once,
    /// for the first caller that asks before the rate is seeded.
    pub fn begin_calibration(&self) -> bool {
        let mut state = self.state();
        if state.ms_per_unit_pixel.is_some() || state.calibrating {
            return false;
        }
        state.calibrating = true;
        true
    }

    /// Seeds the rate from a calibration draw that took
    /// `measured_ms_per_unit_pixel`: twice that, to stay on the safe side of
    /// a measurement made on a cold device.
    pub fn seed(&self, measured_ms_per_unit_pixel: f64) {
        let mut state = self.state();
        state.calibrating = false;
        if !(measured_ms_per_unit_pixel.is_finite() && measured_ms_per_unit_pixel > 0.0) {
            return;
        }
        state.calibrated = Some(measured_ms_per_unit_pixel);
        let seeded = 2.0 * measured_ms_per_unit_pixel;
        state.ms_per_unit_pixel = Some(
            state
                .ms_per_unit_pixel
                .map_or(seeded, |rate| rate.max(seeded)),
        );
    }

    /// Lets a later caller calibrate again: the draw failed or never landed.
    pub fn abandon_calibration(&self) {
        self.state().calibrating = false;
    }

    /// Folds in a frame whose work, `unit_pixels` of predicted cost, was
    /// measured as at most `measured_ms` of GPU time: a rate above the
    /// measured one comes down by at most a tenth, not below the calibration.
    pub fn observe(&self, measured_ms: f64, unit_pixels: f64) {
        if !(measured_ms.is_finite() && unit_pixels.is_finite() && unit_pixels > 0.0) {
            return;
        }
        let measured = measured_ms / unit_pixels;
        let mut state = self.state();
        let (Some(rate), Some(floor)) = (state.ms_per_unit_pixel, state.calibrated) else {
            return;
        };
        if measured < rate {
            state.ms_per_unit_pixel = Some((rate * 0.9).max(measured).max(floor));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GpuRate;

    #[test]
    fn the_rate_falls_slowly_to_the_calibration_and_never_rises_on_a_measurement() {
        let rate = GpuRate::default();
        assert_eq!(rate.ms_per_unit_pixel(), None);
        rate.observe(10.0, 1.0);
        assert_eq!(
            rate.ms_per_unit_pixel(),
            None,
            "nothing to fold into before a seed"
        );
        assert!(rate.begin_calibration());
        assert!(!rate.begin_calibration(), "one calibration at a time");
        rate.seed(1.0);
        assert_eq!(rate.ms_per_unit_pixel(), Some(2.0));
        // A completion seen late measures more than the GPU spent: never a reason to raise it.
        rate.observe(8.0, 2.0);
        assert_eq!(rate.ms_per_unit_pixel(), Some(2.0));
        rate.observe(0.0, 2.0);
        assert_eq!(rate.ms_per_unit_pixel(), Some(1.8));
        for _ in 0..100 {
            rate.observe(0.0, 2.0);
        }
        assert_eq!(rate.ms_per_unit_pixel(), Some(1.0));
        assert!(!rate.begin_calibration());
    }
}
