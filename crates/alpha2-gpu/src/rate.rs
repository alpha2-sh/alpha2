//! Device throughput in milliseconds per unit of effect cost per pixel.
//!
//! Renderers split frames into submissions under [`crate::submission_budget_ms`], predicting
//! each pass as cost times pixels times a rate. A calibration draw seeds the device rate at
//! [`MARGIN`] times its measurement, and that measurement is the rate's floor. A pass whose own
//! draws measured slower than the floor is predicted at its own rate, scaled the same way.
//!
//! A frame measured from an idle queue is at most its GPU time, so it may lower the rate toward
//! the floor. Only a frame whose completion was also seen as it happened may raise the rate. A
//! frame far under a floor that splits frames asks for the calibration to be drawn again. The
//! rate affects only submission splitting, never pixels.

use std::collections::HashMap;
use std::sync::Mutex;

/// The rate assumed before calibration, in milliseconds per unit pixel: twice the slowest
/// measured rate (integrated GPUs), so early frames are split too finely rather than too
/// coarsely.
pub const UNCALIBRATED_MS_PER_UNIT_PIXEL: f64 = 2.0e-8;

/// What a measurement is multiplied by to become a rate.
pub const MARGIN: f64 = 2.0;

/// The most a rate falls per frame, as a fraction of itself.
const LOWEST_STEP: f64 = 0.9;

/// A frame this far under the floor disagrees with the calibration.
const DISAGREEMENT: f64 = 4.0;

/// A frame raises the rate only when it measured at least this share of its budget.
const RAISING_SHARE_OF_BUDGET: f64 = 0.25;

/// A frame disagrees with the calibration only when it was predicted at least this share of its
/// budget, so the floor could split it.
const SPLITTING_SHARE_OF_BUDGET: f64 = 0.5;

/// A calibration within this factor of the previous one confirms it.
const CONFIRMING: f64 = 2.0;

/// Pass rates kept per device before they are all dropped and measured again.
const PASS_RATES_KEPT: usize = 1024;

/// How a frame's measured span relates to the GPU time it measures.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SpanTrust {
    /// The queue was idle at the first submit, but the completion may have been seen late.
    UpperBound,
    /// The queue was idle at the first submit and the completion was seen as it happened.
    Exact,
}

/// One measured frame, as [`GpuRate::observe`] folds it in.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FrameMeasurement {
    /// Milliseconds from the first submit to the completion.
    pub measured_ms: f64,
    /// Milliseconds the plan predicted.
    pub predicted_ms: f64,
    /// The device rate the frame was planned at.
    pub planned_ms_per_unit_pixel: f64,
    /// The submission budget the frame was planned under, in milliseconds.
    pub budget_ms: f64,
    /// How far `measured_ms` can be trusted.
    pub trust: SpanTrust,
}

/// A kind of pass at a size class, whose own rate a device keeps.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct PassKey {
    pass: u64,
    size_class: u32,
}

impl PassKey {
    /// The pass identified by `pass` over `pixels` pixels. Sizes within a factor of the square
    /// root of two share a class.
    #[must_use]
    pub fn new(pass: u64, pixels: u64) -> Self {
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a pixel count's doubled logarithm is far below 2^32"
        )]
        let size_class = (2.0 * (pixels.max(1) as f64).log2()).ceil() as u32;
        Self { pass, size_class }
    }
}

#[derive(Debug)]
struct RateState {
    ms_per_unit_pixel: Option<f64>,
    /// The calibration measurement, which is the floor of the rate.
    calibrated: Option<f64>,
    calibrating: bool,
    recalibration_requested: bool,
    disagreements: u32,
    /// Disagreeing frames needed before the calibration is drawn again.
    patience: u32,
    passes: HashMap<PassKey, f64>,
}

impl Default for RateState {
    fn default() -> Self {
        Self {
            ms_per_unit_pixel: None,
            calibrated: None,
            calibrating: false,
            recalibration_requested: false,
            disagreements: 0,
            patience: 1,
            passes: HashMap::new(),
        }
    }
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

    /// The rate over the floor, or [`MARGIN`] before calibration.
    #[must_use]
    pub fn margin(&self) -> f64 {
        let state = self.state();
        match (state.ms_per_unit_pixel, state.calibrated) {
            (Some(rate), Some(floor)) => (rate / floor).max(1.0),
            _ => MARGIN,
        }
    }

    /// Returns true once per calibration due, telling that caller to run the calibration draw:
    /// before the first seed, and after frames disagreed with the floor.
    pub fn begin_calibration(&self) -> bool {
        let mut state = self.state();
        let due = state.ms_per_unit_pixel.is_none() || state.recalibration_requested;
        if !due || state.calibrating {
            return false;
        }
        state.calibrating = true;
        true
    }

    /// Seeds the rate at [`MARGIN`] times `measured_ms_per_unit_pixel`, which becomes the floor.
    pub fn seed(&self, measured_ms_per_unit_pixel: f64) {
        let mut state = self.state();
        state.calibrating = false;
        if !(measured_ms_per_unit_pixel.is_finite() && measured_ms_per_unit_pixel > 0.0) {
            return;
        }
        if state.recalibration_requested {
            state.recalibration_requested = false;
            let confirmed = state.calibrated.is_some_and(|previous| {
                let ratio = measured_ms_per_unit_pixel / previous;
                (1.0 / CONFIRMING..=CONFIRMING).contains(&ratio)
            });
            state.patience = if confirmed {
                state.patience.saturating_mul(2)
            } else {
                1
            };
        }
        state.disagreements = 0;
        state.calibrated = Some(measured_ms_per_unit_pixel);
        state.ms_per_unit_pixel = Some(MARGIN * measured_ms_per_unit_pixel);
    }

    /// Allows a later caller to calibrate again after a failed draw.
    pub fn abandon_calibration(&self) {
        self.state().calibrating = false;
    }

    /// Folds in a measured frame. Any trust lowers the rate by at most a tenth per frame, never
    /// below the floor; an exact span over the prediction raises it to [`MARGIN`] times the
    /// measurement.
    pub fn observe(&self, frame: FrameMeasurement) {
        let measured = frame.measured_ms / frame.predicted_ms * frame.planned_ms_per_unit_pixel;
        if !(measured.is_finite() && measured >= 0.0 && frame.predicted_ms > 0.0) {
            return;
        }
        let mut state = self.state();
        let (Some(rate), Some(floor)) = (state.ms_per_unit_pixel, state.calibrated) else {
            return;
        };
        if measured < rate {
            state.ms_per_unit_pixel = Some((rate * LOWEST_STEP).max(measured).max(floor));
        } else if frame.trust == SpanTrust::Exact
            && frame.measured_ms > frame.predicted_ms
            && frame.measured_ms >= frame.budget_ms * RAISING_SHARE_OF_BUDGET
        {
            state.ms_per_unit_pixel = Some(MARGIN * measured);
        }
        if measured * DISAGREEMENT < floor
            && frame.predicted_ms >= frame.budget_ms * SPLITTING_SHARE_OF_BUDGET
        {
            state.disagreements = state.disagreements.saturating_add(1);
            if state.disagreements >= state.patience {
                state.disagreements = 0;
                state.recalibration_requested = true;
            }
        }
    }

    /// What draws of `pass` measured, in milliseconds per unit pixel.
    #[must_use]
    pub fn pass_rate(&self, pass: PassKey) -> Option<f64> {
        self.state().passes.get(&pass).copied()
    }

    /// Keeps what a draw of `pass` measured, replacing an earlier measurement.
    pub fn record_pass_rate(&self, pass: PassKey, measured_ms_per_unit_pixel: f64) {
        if !(measured_ms_per_unit_pixel.is_finite() && measured_ms_per_unit_pixel >= 0.0) {
            return;
        }
        let mut state = self.state();
        if state.passes.len() >= PASS_RATES_KEPT && !state.passes.contains_key(&pass) {
            state.passes.clear();
        }
        state.passes.insert(pass, measured_ms_per_unit_pixel);
    }

    /// The rate to predict `pass` at: the device rate, or the pass's own measurement scaled by
    /// [`Self::margin`] when that is slower.
    #[must_use]
    pub fn planning_rate(&self, device_rate: f64, pass: Option<PassKey>) -> f64 {
        let Some(measured) = pass.and_then(|pass| self.pass_rate(pass)) else {
            return device_rate;
        };
        device_rate.max(measured * self.margin())
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameMeasurement, GpuRate, PassKey, SpanTrust};

    fn frame(measured_ms: f64, predicted_ms: f64, trust: SpanTrust) -> FrameMeasurement {
        FrameMeasurement {
            measured_ms,
            predicted_ms,
            planned_ms_per_unit_pixel: predicted_ms,
            budget_ms: 1_000.0,
            trust,
        }
    }

    #[test]
    fn the_rate_falls_slowly_to_the_calibration_on_any_trusted_frame() {
        let rate = GpuRate::default();
        assert_eq!(rate.ms_per_unit_pixel(), None);
        rate.observe(frame(10.0, 1.0, SpanTrust::Exact));
        assert_eq!(
            rate.ms_per_unit_pixel(),
            None,
            "nothing to fold before a seed"
        );
        assert!(rate.begin_calibration());
        assert!(!rate.begin_calibration(), "one calibration at a time");
        rate.seed(1.0);
        assert_eq!(rate.ms_per_unit_pixel(), Some(2.0));
        assert!((rate.margin() - 2.0).abs() < 1e-12);
        rate.observe(frame(1.0, 2.0, SpanTrust::UpperBound));
        assert_eq!(rate.ms_per_unit_pixel(), Some(1.8));
        for _ in 0..100 {
            rate.observe(frame(0.0, 2.0, SpanTrust::UpperBound));
        }
        assert_eq!(rate.ms_per_unit_pixel(), Some(1.0));
        assert!(!rate.begin_calibration());
    }

    #[test]
    fn only_an_exact_material_frame_over_its_prediction_raises_the_rate() {
        let rate = GpuRate::default();
        rate.seed(1.0);
        rate.observe(frame(500.0, 2.0, SpanTrust::UpperBound));
        assert_eq!(rate.ms_per_unit_pixel(), Some(2.0), "a late completion");
        rate.observe(frame(100.0, 2.0, SpanTrust::Exact));
        assert_eq!(
            rate.ms_per_unit_pixel(),
            Some(2.0),
            "under a quarter of the budget"
        );
        rate.observe(frame(400.0, 2.0, SpanTrust::Exact));
        assert_eq!(rate.ms_per_unit_pixel(), Some(800.0));
    }

    #[test]
    fn frames_far_under_a_splitting_floor_ask_for_calibration_with_growing_patience() {
        let rate = GpuRate::default();
        rate.seed(1_000.0);
        rate.observe(frame(1.0, 100.0, SpanTrust::UpperBound));
        assert!(!rate.begin_calibration(), "predicted under half the budget");
        rate.observe(frame(1.0, 2_000.0, SpanTrust::UpperBound));
        assert!(rate.begin_calibration());
        rate.seed(10.0);
        assert_eq!(
            rate.ms_per_unit_pixel(),
            Some(20.0),
            "a lower calibration replaces"
        );
        for patience in [1, 2, 4] {
            for _ in 1..patience {
                rate.observe(frame(0.1, 2_000.0, SpanTrust::UpperBound));
                assert!(!rate.begin_calibration());
            }
            rate.observe(frame(0.1, 2_000.0, SpanTrust::UpperBound));
            assert!(rate.begin_calibration());
            rate.seed(12.0);
        }
    }

    #[test]
    fn a_pass_slower_than_the_device_is_planned_at_its_own_rate() {
        let rate = GpuRate::default();
        rate.seed(1.0);
        let scattered = PassKey::new(7, 1920 * 1080);
        let coherent = PassKey::new(8, 1920 * 1080);
        assert_eq!(rate.planning_rate(2.0, Some(scattered)), 2.0);
        rate.record_pass_rate(scattered, 10.0);
        rate.record_pass_rate(coherent, 0.1);
        assert_eq!(rate.planning_rate(2.0, Some(scattered)), 20.0);
        assert_eq!(rate.planning_rate(2.0, Some(coherent)), 2.0);
        assert_eq!(rate.pass_rate(PassKey::new(7, 3840 * 2160)), None);
        assert_eq!(rate.pass_rate(PassKey::new(7, 1920 * 1088)), Some(10.0));
    }
}
