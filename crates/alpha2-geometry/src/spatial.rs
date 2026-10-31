//! Motion along spatial curves. A position keyframed with tangents travels along the cubic
//! between keyframes by arc length, at the speed its timing curve sets.

use crate::arclen;
use kurbo::{CubicBez, PathSeg, Point, Vec2};

/// The point `amount` of the arc length along the cubic from `from` (control `from +
/// out_tangent`) to `to` (control `to + in_tangent`), in a space scaled to screen proportions.
///
/// An `amount` outside `0..=1` continues straight along the end tangent, or along the chord when
/// that tangent is zero.
#[must_use]
pub fn point_along(
    from: Point,
    out_tangent: Vec2,
    in_tangent: Vec2,
    to: Point,
    amount: f64,
) -> Point {
    let curve = CubicBez::new(from, from + out_tangent, to + in_tangent, to);
    let segment = PathSeg::Cubic(curve);
    let total = arclen::length(&segment);
    if total <= 0.0 {
        return from;
    }
    if amount < 0.0 {
        let direction = unit(from - (from + out_tangent), from - to);
        return from + direction * (-amount * total);
    }
    if amount > 1.0 {
        let direction = unit(to - (to + in_tangent), to - from);
        return to + direction * ((amount - 1.0) * total);
    }
    let t = arclen::parameter_at(&segment, amount * total, total);
    arclen::point_at(&segment, t)
}

/// `primary` normalized, else `fallback` normalized, else zero.
fn unit(primary: Vec2, fallback: Vec2) -> Vec2 {
    for vector in [primary, fallback] {
        let length = crate::affine::length(vector);
        if length > 0.0 {
            return vector / length;
        }
    }
    Vec2::ZERO
}

/// The motion path between keyframe values as a polyline with `samples` points per segment.
#[must_use]
pub fn motion_polyline(keys: &[(Point, Vec2, Vec2)], samples: u32) -> Vec<Point> {
    let mut points = Vec::new();
    let Some(first) = keys.first() else {
        return points;
    };
    points.push(first.0);
    let samples = samples.max(1);
    for pair in keys.windows(2) {
        let (from, out_tangent, _) = pair[0];
        let (to, _, in_tangent) = pair[1];
        let curve = CubicBez::new(from, from + out_tangent, to + in_tangent, to);
        for step in 1..=samples {
            points.push(kurbo::ParamCurve::eval(
                &curve,
                f64::from(step) / f64::from(samples),
            ));
        }
    }
    points
}
