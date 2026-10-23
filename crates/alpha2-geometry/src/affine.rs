//! Affine transforms built from `libm`, and lengths from `sqrt` alone, so
//! they are the same bits on every target.

use kurbo::{Affine, Point, Vec2};

/// A clockwise rotation by `degrees` in pixel space (y down), about the
/// origin.
#[must_use]
pub fn rotation(degrees: f64) -> Affine {
    let radians = degrees.to_radians();
    let (sin, cos) = (libm::sin(radians), libm::cos(radians));
    Affine::new([cos, sin, -sin, cos, 0.0, 0.0])
}

/// A clockwise rotation by `degrees` about `center`.
#[must_use]
pub fn rotation_about(degrees: f64, center: Point) -> Affine {
    Affine::translate(center.to_vec2()) * rotation(degrees) * Affine::translate(-center.to_vec2())
}

/// A group's transform in pixel space: translate by minus `anchor`, scale,
/// rotate clockwise, then translate to `position`.
#[must_use]
pub fn placement(position: Point, anchor: Point, scale: Vec2, degrees: f64) -> Affine {
    Affine::translate(position.to_vec2())
        * rotation(degrees)
        * Affine::scale_non_uniform(scale.x, scale.y)
        * Affine::translate(-anchor.to_vec2())
}

/// The length of `vector`, from `sqrt` alone.
#[must_use]
pub fn length(vector: Vec2) -> f64 {
    (vector.x * vector.x + vector.y * vector.y).sqrt()
}

/// The distance between two points, from `sqrt` alone.
#[must_use]
pub fn distance(a: Point, b: Point) -> f64 {
    length(b - a)
}

/// The largest factor `affine` stretches a length by: its larger singular
/// value. Zero for a transform that collapses everything.
#[must_use]
pub fn max_scale(affine: Affine) -> f64 {
    let [xx, yx, xy, yy, _, _] = affine.as_coeffs();
    // The singular values are the square roots of the eigenvalues of M^T M.
    let first = xx * xx + yx * yx;
    let second = xy * xy + yy * yy;
    let cross = xx * xy + yx * yy;
    let half_sum = f64::midpoint(first, second);
    let half_difference = 0.5 * (first - second);
    let root = (half_difference * half_difference + cross * cross).sqrt();
    (half_sum + root).max(0.0).sqrt()
}

/// Whether `affine` can be inverted: a finite, non-zero determinant.
#[must_use]
pub fn is_invertible(affine: Affine) -> bool {
    let determinant = affine.determinant();
    determinant.is_finite() && determinant.abs() > 1e-12
}
