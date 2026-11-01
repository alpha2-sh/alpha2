//! Counts what usvg makes of path data before it parses it.
//!
//! svgtypes splits each arc into cubics (count grows with the sixth root of the radius) and pops
//! them from the front of a list, so an arc of `n` cubics costs about `n^2/2` moves. A radius of
//! `1e30` is about 66,000 cubics and a second of work. Every resulting vertex can carry a marker.

use std::f64::consts::TAU;

use svgtypes::{PathParser, PathSegment};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PathData {
    /// One per segment, plus each arc's cubics.
    pub(super) vertices: u64,
    /// Conversion cost beyond parsing: arc cubics made and moved.
    pub(super) steps: u64,
}

/// Counts segments up to the first error, where usvg stops reading.
pub(super) fn path_data(d: &str) -> PathData {
    let mut data = PathData::default();
    let (mut x, mut y) = (0.0_f64, 0.0_f64);
    let (mut start_x, mut start_y) = (0.0_f64, 0.0_f64);
    // The control point a smooth curve reflects does not move the pen.
    for segment in PathParser::from(d) {
        let Ok(segment) = segment else {
            break;
        };
        data.vertices = data.vertices.saturating_add(1);
        let to = |abs: bool, to_x: f64, to_y: f64| {
            if abs {
                (to_x, to_y)
            } else {
                (x + to_x, y + to_y)
            }
        };
        match segment {
            PathSegment::MoveTo {
                abs,
                x: to_x,
                y: to_y,
            } => {
                (x, y) = to(abs, to_x, to_y);
                (start_x, start_y) = (x, y);
            }
            PathSegment::LineTo {
                abs,
                x: to_x,
                y: to_y,
            }
            | PathSegment::SmoothQuadratic {
                abs,
                x: to_x,
                y: to_y,
            }
            | PathSegment::Quadratic {
                abs,
                x: to_x,
                y: to_y,
                ..
            }
            | PathSegment::SmoothCurveTo {
                abs,
                x: to_x,
                y: to_y,
                ..
            }
            | PathSegment::CurveTo {
                abs,
                x: to_x,
                y: to_y,
                ..
            } => (x, y) = to(abs, to_x, to_y),
            PathSegment::HorizontalLineTo { abs, x: to_x } => x = if abs { to_x } else { x + to_x },
            PathSegment::VerticalLineTo { abs, y: to_y } => y = if abs { to_y } else { y + to_y },
            PathSegment::ClosePath { .. } => (x, y) = (start_x, start_y),
            PathSegment::EllipticalArc {
                abs,
                rx,
                ry,
                x_axis_rotation,
                large_arc,
                sweep,
                x: to_x,
                y: to_y,
            } => {
                let end = to(abs, to_x, to_y);
                let arc = kurbo::SvgArc {
                    from: kurbo::Point::new(x, y),
                    to: kurbo::Point::new(end.0, end.1),
                    radii: kurbo::Vec2::new(rx, ry),
                    x_rotation: x_axis_rotation.to_radians(),
                    large_arc,
                    sweep,
                };
                let cubics = kurbo::Arc::from_svg_arc(&arc).map_or(1, |arc| cubics(&arc));
                data.vertices = data.vertices.saturating_add(cubics - 1);
                data.steps = data.steps.saturating_add(arc_steps(cubics));
                (x, y) = end;
            }
        }
    }
    data
}

/// Cubics kurbo 0.13 splits `arc` into at svgtypes' tolerance (`Arc::append_iter`).
fn cubics(arc: &kurbo::Arc) -> u64 {
    let scaled_err = arc.radii.x.max(arc.radii.y) / 0.1;
    let n_err = (1.1163 * scaled_err).powf(1.0 / 6.0).max(3.999_999);
    let n = (n_err * arc.sweep_angle.abs() * (1.0 / TAU)).ceil();
    // Saturating: an infinite count is past any budget.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = n as u64;
    n.max(1)
}

/// About two steps per cubic plus the front-removal moves, at sixteen moves per step (a step is
/// about 7.5 ns natively).
fn arc_steps(cubics: u64) -> u64 {
    cubics
        .saturating_mul(2)
        .saturating_add(cubics.saturating_mul(cubics) / 16)
}

pub(super) fn points(list: &str) -> u64 {
    svgtypes::PointsParser::from(list).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_vertices_and_arcs_their_cubics() {
        assert_eq!(
            path_data("M 0 0 L 10 0 H 20 V 10 Q 1 1 2 2 T 3 3 C 1 1 2 2 3 3 S 1 1 2 2 Z"),
            PathData {
                vertices: 9,
                steps: 0
            }
        );
        // A half circle of radius 10: two cubics (kurbo makes at least four a circle).
        let half = path_data("M 0 0 A 10 10 0 0 0 20 0");
        assert_eq!(half.vertices, 1 + 2);
        assert_eq!(half.steps, arc_steps(2));
        // Relative to where the pen is: a half circle of radius 1e29.
        let far = path_data("M 1 0 a 1 1 0 0 0 2e29 0 Z");
        assert!(far.vertices > 50_000, "{far:?}");
        assert!(far.steps > 1 << 27, "{far:?}");
        // usvg stops at the first error.
        assert_eq!(path_data("M 0 0 L 1 1 X 2 2 L 3 3").vertices, 2);
    }

    #[test]
    fn the_counted_cubics_are_the_ones_svgtypes_makes() {
        for d in [
            "M 0 0 A 10 10 0 0 0 20 0",
            "M 3 4 A 7 2 30 1 1 -20 9",
            "M 0 0 a 2.5 2.5 0 0 0 0.4 1e6",
            "M 0 0 A 0 0 0 0 0 5 5",
        ] {
            let made = svgtypes::SimplifyingPathParser::from(d).flatten().count() as u64;
            assert_eq!(path_data(d).vertices, made, "{d}");
        }
    }
}
