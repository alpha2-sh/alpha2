//! Arc lengths of path segments, from `+ - * /` and `sqrt` alone.
//!
//! kurbo's arc length goes through `hypot`, a platform library call; these
//! integrate the speed with a fixed 16-point Gauss-Legendre rule, halving a
//! segment while the halves disagree with the whole, so the result is the
//! same bits everywhere.

use kurbo::{ParamCurve, ParamCurveDeriv, PathSeg, Vec2};

/// How close an arc length is computed, in pixels: the halving stops once
/// the halves agree with the whole within this.
pub const ACCURACY: f64 = 1e-3;

/// Halvings at most, so a pathological segment still ends.
const MAX_DEPTH: u32 = 12;

/// The 16-point Gauss-Legendre rule on `[-1, 1]`: positive abscissas and
/// their weights (the rule is symmetric).
const GAUSS_16: [(f64, f64); 8] = [
    (0.095_012_509_837_637_44, 0.189_450_610_455_068_5),
    (0.281_603_550_779_258_9, 0.182_603_415_044_923_6),
    (0.458_016_777_657_227_4, 0.169_156_519_395_002_5),
    (0.617_876_244_402_643_8, 0.149_595_988_816_576_7),
    (0.755_404_408_355_003, 0.124_628_971_255_533_9),
    (0.865_631_202_387_831_7, 0.095_158_511_682_492_78),
    (0.944_575_023_073_232_6, 0.062_253_523_938_647_89),
    (0.989_400_934_991_649_9, 0.027_152_459_411_754_09),
];

fn speed(derivative: Vec2) -> f64 {
    crate::affine::length(derivative)
}

/// The speed of `segment` at parameter `t`.
fn segment_speed(segment: &PathSeg, t: f64) -> f64 {
    match segment {
        PathSeg::Line(line) => speed(line.p1 - line.p0),
        PathSeg::Quad(quad) => speed(quad.deriv().eval(t).to_vec2()),
        PathSeg::Cubic(cubic) => speed(cubic.deriv().eval(t).to_vec2()),
    }
}

fn gauss(segment: &PathSeg, t0: f64, t1: f64) -> f64 {
    let half = 0.5 * (t1 - t0);
    let middle = f64::midpoint(t0, t1);
    let mut sum = 0.0;
    for (x, w) in GAUSS_16 {
        sum += w
            * (segment_speed(segment, middle - half * x)
                + segment_speed(segment, middle + half * x));
    }
    sum * half
}

fn adaptive(segment: &PathSeg, t0: f64, t1: f64, whole: f64, depth: u32) -> f64 {
    let middle = f64::midpoint(t0, t1);
    let left = gauss(segment, t0, middle);
    let right = gauss(segment, middle, t1);
    if depth >= MAX_DEPTH || (left + right - whole).abs() <= ACCURACY * 0.5 {
        return left + right;
    }
    adaptive(segment, t0, middle, left, depth + 1) + adaptive(segment, middle, t1, right, depth + 1)
}

/// The length of `segment` between parameters `t0` and `t1`.
#[must_use]
pub fn length_between(segment: &PathSeg, t0: f64, t1: f64) -> f64 {
    if t1 <= t0 {
        return 0.0;
    }
    if let PathSeg::Line(line) = segment {
        return speed(line.p1 - line.p0) * (t1 - t0);
    }
    let whole = gauss(segment, t0, t1);
    adaptive(segment, t0, t1, whole, 0)
}

/// The length of the whole of `segment`.
#[must_use]
pub fn length(segment: &PathSeg) -> f64 {
    length_between(segment, 0.0, 1.0)
}

/// The parameter at which `segment` has run `distance` of its `total`
/// length (which [`length`] gave), within [`ACCURACY`]; 0 or 1 past its
/// ends.
#[must_use]
pub fn parameter_at(segment: &PathSeg, distance: f64, total: f64) -> f64 {
    if distance <= 0.0 || total <= 0.0 {
        return 0.0;
    }
    if distance >= total {
        return 1.0;
    }
    if let PathSeg::Line(_) = segment {
        return distance / total;
    }
    // Bisection on the length so far: monotonic, so it cannot leave the bracket.
    let (mut low, mut high) = (0.0_f64, 1.0_f64);
    let mut t = distance / total;
    for _ in 0..60 {
        let run = length_between(segment, 0.0, t);
        let error = run - distance;
        if error.abs() <= ACCURACY * 0.5 {
            break;
        }
        if error < 0.0 {
            low = t;
        } else {
            high = t;
        }
        t = f64::midpoint(low, high);
    }
    t
}

/// The point at parameter `t`, by de Casteljau, as kurbo evaluates it.
#[must_use]
pub fn point_at(segment: &PathSeg, t: f64) -> kurbo::Point {
    segment.eval(t)
}
