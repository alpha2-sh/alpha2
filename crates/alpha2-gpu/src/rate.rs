//! Device throughput in milliseconds per unit of effect cost per pixel.
//!
//! Renderers split frames into submissions under [`crate::submission_budget_ms`], predicting
//! each pass as cost times pixels times this rate. A calibration draw seeds it at twice the
//! measurement; heavy frames then lower it by at most a tenth per frame, never below the
//! calibration.
//!
//! Frame GPU time comes from the completion callback, which fires at the next poll, so it is
//! only an upper bound. Measurements can therefore lower the rate but never raise it. The rate
//! affects only submission splitting, never pixels.

use std::sync::Mutex;

/// The rate assumed before calibration, in milliseconds per unit pixel: twice the slowest
/// measured rate (integrated GPUs), so early frames are split too finely rather than too
/// coarsely.
pub const UNCALIBRATED_MS_PER_UNIT_PIXEL: f64 = 2.0e-8;

#[derive(Debug, Default)]
struct RateState {
    ms_per_unit_pixel: Option<f64>,
    /// The calibration measurement, which is the floor of the rate.
    calibrated: Option<f64>,
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

    /// The rate in milliseconds per unit pixel, once seeded.
    #[must_use]
    pub fn ms_per_unit_pixel(&self) -> Option<f64> {
        self.state().ms_per_unit_pixel
    }

    /// Returns true once, telling the first caller before seeding to run the calibration draw.
    pub fn begin_calibration(&self) -> bool {
        let mut state = self.state();
        if state.ms_per_unit_pixel.is_some() || state.calibrating {
            return false;
        }
        state.calibrating = true;
        true
    }

    /// Seeds the rate at twice `measured_ms_per_unit_pixel`, allowing for a cold device.
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

    /// Allows a later caller to calibrate again after a failed draw.
    pub fn abandon_calibration(&self) {
        self.state().calibrating = false;
    }

    /// Folds in a frame of `unit_pixels` predicted cost that took at most `measured_ms`. A
    /// higher rate drops by at most a tenth, never below the calibration.
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
        // A late completion overstates GPU time, so it never raises the rate.
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
