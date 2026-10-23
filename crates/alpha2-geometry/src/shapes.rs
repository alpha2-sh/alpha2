//! Parametric shapes as Bezier paths in pixel space, with the start points
//! and directions After Effects and Lottie give them, so a trim cuts them
//! where it cuts there.

use crate::affine::rotation_about;
use kurbo::{BezPath, Point, Vec2};

/// The four-cubic circle constant: a quarter circle's control points lie
/// this fraction of the radius along its end tangents.
pub const KAPPA: f64 = 0.552_284_749_830_793_6;

/// A rectangle of full `size` about `center`, corners rounded by `radius`
/// (capped at half the shorter side), turned `degrees` clockwise. It starts
/// at its top-right corner (below the corner's arc when rounded) and runs
/// clockwise.
#[must_use]
pub fn rectangle(center: Point, size: Vec2, radius: f64, degrees: f64) -> BezPath {
    let (hw, hh) = (size.x.abs() * 0.5, size.y.abs() * 0.5);
    let r = radius.max(0.0).min(hw).min(hh);
    let (left, right, top, bottom) = (center.x - hw, center.x + hw, center.y - hh, center.y + hh);
    let mut path = BezPath::new();
    if r <= 0.0 {
        path.move_to((right, top));
        path.line_to((right, bottom));
        path.line_to((left, bottom));
        path.line_to((left, top));
        path.close_path();
    } else {
        let k = r * (1.0 - KAPPA);
        path.move_to((right, top + r));
        path.line_to((right, bottom - r));
        path.curve_to(
            (right, bottom - k),
            (right - k, bottom),
            (right - r, bottom),
        );
        path.line_to((left + r, bottom));
        path.curve_to((left + k, bottom), (left, bottom - k), (left, bottom - r));
        path.line_to((left, top + r));
        path.curve_to((left, top + k), (left + k, top), (left + r, top));
        path.line_to((right - r, top));
        path.curve_to((right - k, top), (right, top + k), (right, top + r));
        path.close_path();
    }
    turned(path, center, degrees)
}

/// The ellipse inscribed in a full `size` about `center`, turned `degrees`
/// clockwise: four cubics from its top, clockwise.
#[must_use]
pub fn ellipse(center: Point, size: Vec2, degrees: f64) -> BezPath {
    let (rx, ry) = (size.x.abs() * 0.5, size.y.abs() * 0.5);
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    let (x, y) = (center.x, center.y);
    let mut path = BezPath::new();
    path.move_to((x, y - ry));
    path.curve_to((x + kx, y - ry), (x + rx, y - ky), (x + rx, y));
    path.curve_to((x + rx, y + ky), (x + kx, y + ry), (x, y + ry));
    path.curve_to((x - kx, y + ry), (x - rx, y + ky), (x - rx, y));
    path.curve_to((x - rx, y - ky), (x - kx, y - ry), (x, y - ry));
    path.close_path();
    turned(path, center, degrees)
}

/// A star or polygon, as a Lottie polystar draws it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Polystar {
    /// Whether it has inner corners.
    pub star: bool,
    /// Its centre, in pixels.
    pub center: Point,
    /// Its points; a fraction is dropped.
    pub points: f64,
    /// The points' distance from the centre, in pixels.
    pub outer_radius: f64,
    /// A star's inner corners' distance from the centre, in pixels.
    pub inner_radius: f64,
    /// How round the points are, in `0..=1`.
    pub outer_roundness: f64,
    /// How round a star's inner corners are, in `0..=1`.
    pub inner_roundness: f64,
    /// Clockwise rotation, in degrees.
    pub degrees: f64,
}

/// A polystar's outline: its first point straight up from the centre before
/// rotation, the rest clockwise. Each corner's tangents run along the circle
/// through it, as long as the roundness times a quarter (polygon) or half
/// (star) of the circle's arc per corner, as Lottie rounds them.
#[must_use]
pub fn polystar(star: &Polystar) -> BezPath {
    // Floored as Lottie floors it; validation keeps it at three or more.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let points = star.points.max(0.0).floor().min(1e4) as u32;
    let mut path = BezPath::new();
    if points == 0 {
        return path;
    }
    let corners = if star.star { points * 2 } else { points };
    let step = std::f64::consts::TAU / f64::from(corners);
    let arc = |radius: f64| std::f64::consts::TAU * radius / f64::from(corners * 2);
    let mut vertices = Vec::with_capacity(corners as usize);
    for corner in 0..corners {
        let outer = !star.star || corner % 2 == 0;
        let (radius, roundness) = if outer {
            (star.outer_radius, star.outer_roundness)
        } else {
            (star.inner_radius, star.inner_roundness)
        };
        let angle = -std::f64::consts::FRAC_PI_2 + step * f64::from(corner);
        let (sin, cos) = (libm::sin(angle), libm::cos(angle));
        let offset = Vec2::new(radius * cos, radius * sin);
        // Along the circle, clockwise: the direction of travel.
        let tangent = Vec2::new(-sin, cos);
        let segment = if star.star {
            arc(radius)
        } else {
            arc(radius) * 0.5
        };
        let handle = tangent * (segment * roundness);
        vertices.push((star.center + offset, handle));
    }
    path.move_to(vertices[0].0);
    for index in 0..vertices.len() {
        let (from, from_handle) = vertices[index];
        let (to, to_handle) = vertices[(index + 1) % vertices.len()];
        if from_handle == Vec2::ZERO && to_handle == Vec2::ZERO {
            path.line_to(to);
        } else {
            path.curve_to(from + from_handle, to - to_handle, to);
        }
    }
    path.close_path();
    turned(path, star.center, star.degrees)
}

/// One vertex of a Bezier subpath, in pixels: a point and its tangents
/// relative to it.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Vertex {
    /// Where it lies.
    pub point: Point,
    /// The control point of the segment ending here, relative to `point`.
    pub in_tangent: Vec2,
    /// The control point of the segment starting here, relative to `point`.
    pub out_tangent: Vec2,
}

/// Appends a subpath through `vertices` to `path`: a cubic between each
/// pair, a line where both tangents are zero, and, when `closed`, back to
/// the first. A single vertex is a degenerate subpath of no length.
pub fn append_subpath(path: &mut BezPath, vertices: &[Vertex], closed: bool) {
    let Some(first) = vertices.first() else {
        return;
    };
    path.move_to(first.point);
    let segment = |path: &mut BezPath, from: &Vertex, to: &Vertex| {
        if from.out_tangent == Vec2::ZERO && to.in_tangent == Vec2::ZERO {
            path.line_to(to.point);
        } else {
            path.curve_to(
                from.point + from.out_tangent,
                to.point + to.in_tangent,
                to.point,
            );
        }
    };
    for pair in vertices.windows(2) {
        segment(path, &pair[0], &pair[1]);
    }
    if closed {
        if vertices.len() > 1 {
            segment(path, &vertices[vertices.len() - 1], first);
        }
        path.close_path();
    }
}

fn turned(path: BezPath, center: Point, degrees: f64) -> BezPath {
    if degrees == 0.0 {
        path
    } else {
        rotation_about(degrees, center) * path
    }
}
