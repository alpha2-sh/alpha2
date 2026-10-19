//! Timing curves shared by keyframe interpolation and transitions.
//!
//! The one parametric curve is CSS Easing Functions Level 1
//! `cubic-bezier(x1, y1, x2, y2)`: a cubic Bezier from (0, 0) to (1, 1) whose
//! inner control points are (x1, y1) and (x2, y2). Web animation, Lottie, and
//! motion tools share this definition, so a curve authored in any of them
//! means the same here. Named presets (`ease`, `ease-in`, ...) are values of it,
//! kept by hosts rather than the model.
//!
//! Evaluation is deterministic across targets: it uses `f64` addition,
//! subtraction, multiplication, and division only (no transcendental
//! functions, no fused multiply-add), with a fixed iteration budget, so wasm
//! and native produce identical bits.

use serde::{Deserialize, Serialize};

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing curve.
///
/// `x1` and `x2` lie in `0..=1`, which keeps the curve a function of its
/// input. `y1` and `y2` are unbounded: a curve may overshoot and come back.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CubicBezier {
    /// Horizontal coordinate of the first control point, in `0..=1`.
    pub x1: f32,
    /// Vertical coordinate of the first control point.
    pub y1: f32,
    /// Horizontal coordinate of the second control point, in `0..=1`.
    pub x2: f32,
    /// Vertical coordinate of the second control point.
    pub y2: f32,
}

/// Newton steps tried before bisection takes over.
const NEWTON_STEPS: usize = 8;
/// Bisection halvings; 2^-64 is far below the tolerance.
const BISECTION_STEPS: usize = 64;
/// Accepted distance between the curve's `x` and the requested progress.
const TOLERANCE: f64 = 1e-12;
/// A slope this flat would send a Newton step far outside the curve.
const MIN_SLOPE: f64 = 1e-9;

impl CubicBezier {
    /// Creates a curve from its two inner control points.
    #[must_use]
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Whether every coordinate is finite and `x1`, `x2` lie in `0..=1`.
    #[must_use]
    pub fn is_valid(self) -> bool {
        [self.x1, self.y1, self.x2, self.y2]
            .into_iter()
            .all(f32::is_finite)
            && (0.0..=1.0).contains(&self.x1)
            && (0.0..=1.0).contains(&self.x2)
    }

    /// The curve's output at linear `progress`, which is clamped to `0..=1`.
    ///
    /// The ends are exact: 0 maps to 0 and 1 to 1. Between them the output
    /// may leave `0..=1` when `y1` or `y2` does.
    #[must_use]
    pub fn apply(self, progress: f64) -> f64 {
        let x = progress.clamp(0.0, 1.0);
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let parameter = self.parameter_at(x);
        component(parameter, f64::from(self.y1), f64::from(self.y2))
    }

    /// The Bezier parameter whose `x` equals `x`, for `x` in `(0, 1)`.
    fn parameter_at(self, x: f64) -> f64 {
        self.newton(x).unwrap_or_else(|| self.bisection(x))
    }

    /// Newton's method from `t = x`, if it lands on the curve within
    /// `NEWTON_STEPS` without leaving `0..=1`.
    fn newton(self, x: f64) -> Option<f64> {
        let (x1, x2) = (f64::from(self.x1), f64::from(self.x2));
        let mut parameter = x;
        for _ in 0..NEWTON_STEPS {
            let error = component(parameter, x1, x2) - x;
            if error.abs() < TOLERANCE {
                return (0.0..=1.0).contains(&parameter).then_some(parameter);
            }
            let slope = slope(parameter, x1, x2);
            if slope.abs() < MIN_SLOPE {
                return None;
            }
            parameter -= error / slope;
        }
        None
    }

    /// Bisection on `0..=1`, where `x(t)` is non-decreasing because `x1` and
    /// `x2` lie in `0..=1`; it converges wherever the curve is flat.
    fn bisection(self, x: f64) -> f64 {
        let (x1, x2) = (f64::from(self.x1), f64::from(self.x2));
        let (mut low, mut high) = (0.0_f64, 1.0_f64);
        let mut parameter = x;
        for _ in 0..BISECTION_STEPS {
            let value = component(parameter, x1, x2);
            if (value - x).abs() < TOLERANCE {
                break;
            }
            if value < x {
                low = parameter;
            } else {
                high = parameter;
            }
            parameter = low + (high - low) * 0.5;
        }
        parameter
    }
}

/// One coordinate of the curve at parameter `t`, for inner control
/// coordinates `p1`, `p2` and ends 0 and 1:
/// `3 (1-t)^2 t p1 + 3 (1-t) t^2 p2 + t^3`, in Horner form.
fn component(t: f64, p1: f64, p2: f64) -> f64 {
    let c = 3.0 * p1;
    let b = 3.0 * (p2 - p1) - c;
    let a = 1.0 - c - b;
    ((a * t + b) * t + c) * t
}

/// The derivative of [`component`] with respect to `t`.
fn slope(t: f64, p1: f64, p2: f64) -> f64 {
    let c = 3.0 * p1;
    let b = 3.0 * (p2 - p1) - c;
    let a = 1.0 - c - b;
    (3.0 * a * t + 2.0 * b) * t + c
}

/// A timing curve from linear progress to eased progress, without a hold.
///
/// Transitions use it directly; keyframe [`crate::Interpolation`] adds `hold`.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", from = "EasingWire")]
pub enum Easing {
    /// Progress advances at a constant rate.
    Linear,
    /// Progress follows a CSS `cubic-bezier()` curve.
    CubicBezier(CubicBezier),
}

/// The wire form of [`Easing`].
///
/// serde lets an internally tagged unit variant ignore extra fields even with
/// `deny_unknown_fields`, so `{"type": "linear", "x1": 0}` would parse. Empty
/// struct variants reject them, as the published schema does.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Easing"))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum EasingWire {
    /// Progress advances at a constant rate.
    Linear {},
    /// Progress follows a CSS `cubic-bezier()` curve.
    CubicBezier(CubicBezier),
}

impl From<EasingWire> for Easing {
    fn from(wire: EasingWire) -> Self {
        match wire {
            EasingWire::Linear {} => Self::Linear,
            EasingWire::CubicBezier(curve) => Self::CubicBezier(curve),
        }
    }
}

impl Easing {
    /// Whether the curve's parameters are usable.
    #[must_use]
    pub fn is_valid(self) -> bool {
        match self {
            Self::Linear => true,
            Self::CubicBezier(curve) => curve.is_valid(),
        }
    }

    /// Eased progress for linear `progress`, which is clamped to `0..=1`.
    #[must_use]
    pub fn apply(self, progress: f64) -> f64 {
        match self {
            Self::Linear => progress.clamp(0.0, 1.0),
            Self::CubicBezier(curve) => curve.apply(progress),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CubicBezier, Easing};

    const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);

    #[test]
    fn css_ease_matches_its_reference_value() {
        // CSS `ease` at x = 0.5 is 0.80240338758485699 at t = 0.69957243870638562, computed
        // independently by 200-step bisection in 50-digit decimal arithmetic. The f32 control
        // points move it by under 1e-9.
        assert!((EASE.apply(0.5) - 0.802_403_387_584_857).abs() < 1e-9);
        assert!((EASE.parameter_at(0.5) - 0.699_572_438_706_385_6).abs() < 1e-9);
        let ease_in_out = CubicBezier::new(0.42, 0.0, 0.58, 1.0);
        // 0.42 and 0.58 are not exact in f32, so the symmetry is to ~1e-8.
        assert!((ease_in_out.apply(0.5) - 0.5).abs() < 1e-7);
        for x in [0.1, 0.25, 0.4] {
            let mirrored = 1.0 - ease_in_out.apply(1.0 - x);
            assert!((ease_in_out.apply(x) - mirrored).abs() < 1e-7);
        }
    }

    #[test]
    fn ends_are_exact_and_input_is_clamped() {
        for curve in [EASE, CubicBezier::new(0.0, -2.0, 1.0, 3.0)] {
            assert_eq!(curve.apply(0.0).to_bits(), 0.0_f64.to_bits());
            assert_eq!(curve.apply(1.0).to_bits(), 1.0_f64.to_bits());
            assert_eq!(curve.apply(-4.0).to_bits(), 0.0_f64.to_bits());
            assert_eq!(curve.apply(9.0).to_bits(), 1.0_f64.to_bits());
        }
    }

    #[test]
    fn the_linear_curve_is_the_identity() {
        let linear = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for index in 0..=64 {
            let x = f64::from(index) / 64.0;
            assert!((linear.apply(x) - x).abs() < 1e-12, "{x}");
            assert!((Easing::Linear.apply(x) - x).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn flat_curves_fall_back_to_bisection_and_stay_accurate() {
        // x(t) = t^3 is flat at t = 0, and x(t) = 1 - (1 - t)^3 at t = 1: Newton from t = x
        // overshoots on the first and has no slope on the second.
        let cases = [
            (CubicBezier::new(0.0, 0.0, 0.0, 0.0), 0.001, 0.1),
            (CubicBezier::new(1.0, 1.0, 1.0, 1.0), 0.999, 0.9),
        ];
        for (curve, x, parameter) in cases {
            assert_eq!(curve.newton(x), None, "Newton must give up on {curve:?}");
            let found = curve.parameter_at(x);
            assert!((found - parameter).abs() < 1e-9, "{curve:?}: {found}");
            let x_back = super::component(found, f64::from(curve.x1), f64::from(curve.x2));
            assert!((x_back - x).abs() < 1e-12);
        }
    }

    #[test]
    fn overshooting_curves_leave_the_unit_interval_between_the_ends() {
        let back = CubicBezier::new(0.34, 1.56, 0.64, 1.0);
        let peak = (1..100)
            .map(|index| back.apply(f64::from(index) / 100.0))
            .fold(f64::MIN, f64::max);
        assert!(peak > 1.05, "ease-out-back overshoots: {peak}");
        assert!((Easing::CubicBezier(back).apply(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn control_points_outside_the_unit_square_are_rejected_horizontally_only() {
        assert!(CubicBezier::new(0.0, -5.0, 1.0, 5.0).is_valid());
        assert!(!CubicBezier::new(-0.1, 0.0, 1.0, 1.0).is_valid());
        assert!(!CubicBezier::new(0.0, 0.0, 1.1, 1.0).is_valid());
        assert!(!CubicBezier::new(0.0, f32::NAN, 1.0, 1.0).is_valid());
        assert!(!Easing::CubicBezier(CubicBezier::new(0.0, 0.0, 2.0, 1.0)).is_valid());
    }

    #[test]
    fn the_wire_form_is_a_tagged_object() {
        let easing: Easing =
            serde_json::from_str(r#"{"type":"cubic_bezier","x1":0.42,"y1":0,"x2":0.58,"y2":1}"#)
                .expect("cubic bezier easing");
        assert_eq!(
            easing,
            Easing::CubicBezier(CubicBezier::new(0.42, 0.0, 0.58, 1.0))
        );
        let linear: Easing = serde_json::from_str(r#"{"type":"linear"}"#).expect("linear");
        assert_eq!(linear, Easing::Linear);
        assert!(serde_json::from_str::<Easing>(r#""linear""#).is_err());
        assert!(serde_json::from_str::<Easing>(r#"{"type":"linear","x1":0}"#).is_err());
    }
}
