//! Paths drawn far past the canvas, brought within it before tiny-skia sees
//! them.
//!
//! tiny-skia 0.12 clips a path to the pixmap with `f32` math and then walks
//! it in fixed point. With device coordinates of a million or so (a shape
//! scaled up a thousandfold, a few hundred bytes of Lottie), the clipped
//! edges land a little outside the pixmap and the scan converter indexes
//! past its row: a panic, which in the browser module aborts it, and which
//! natively can panic again while unwinding and abort the process. Measured
//! (`skia-range` harness): no panic in millions of random paths, fills,
//! strokes, and masks within [`MAX_DEVICE`] of the canvas, panics at
//! `+/-1e6`.
//!
//! So a path whose device bounds reach past [`MAX_DEVICE`] is moved into
//! device space here, in `f64`, and every point of it outside the canvas
//! (and [`MARGIN`] pixels around it) is projected onto that rectangle's
//! edge: each coordinate clamped to the rectangle's range. Projection never
//! moves a point across the inside of the rectangle, so the path's winding
//! number at every point inside it is unchanged, and so is what any fill rule
//! paints there; what it adds lies on the edge, outside the canvas. A line is
//! split where it crosses the rectangle's edge lines, where projection
//! bends it; a curve is split until each piece is inside the rectangle
//! (kept as it is), on the outer side of one edge line (projected to a line
//! along it), or too small to matter (a line).

use kurbo::{Affine, CubicBez, ParamCurve, PathEl, Point, QuadBez, Shape};
use resvg::tiny_skia::{self, PathBuilder};

/// How far past the canvas, in device pixels, a path may reach and still be
/// drawn as it is.
pub(super) const MAX_DEVICE: f64 = 65_536.0;

/// How far outside the canvas the rectangle a far path is projected onto
/// lies: antialiasing touches one pixel past an edge, and the projected
/// edges must touch none of the canvas.
const MARGIN: f64 = 64.0;

/// A curve piece smaller than this, in device pixels, is drawn as a line.
const FLAT: f64 = 1.0 / 64.0;

/// How many times a curve is halved at most.
const MAX_DEPTH: u32 = 64;

/// Whether `shape`, drawn through `transform` on a `width` x `height`
/// canvas and reaching `reach` user units past its outline (a stroke's),
/// stays within [`MAX_DEVICE`] of the canvas.
pub(super) fn within(
    shape: &impl Shape,
    transform: Affine,
    reach: f64,
    (width, height): (u32, u32),
) -> bool {
    let bounds = shape.bounding_box().inflate(reach, reach);
    let device = transform.transform_rect_bbox(bounds);
    device.x0 >= -MAX_DEVICE
        && device.y0 >= -MAX_DEVICE
        && device.x1 <= f64::from(width) + MAX_DEVICE
        && device.y1 <= f64::from(height) + MAX_DEVICE
}

/// `shape` through `transform` in device pixels, projected onto the canvas
/// and [`MARGIN`] around it: what filling it paints on the canvas, by
/// either rule. `None` when nothing remains (or a coordinate is not a
/// number).
pub(super) fn fill_path(
    shape: &impl Shape,
    transform: Affine,
    (width, height): (u32, u32),
) -> Option<tiny_skia::Path> {
    let mut clip = Clip {
        left: -MARGIN,
        top: -MARGIN,
        right: f64::from(width) + MARGIN,
        bottom: f64::from(height) + MARGIN,
        builder: PathBuilder::new(),
        at: Point::ZERO,
        start: Point::ZERO,
        open: false,
    };
    for element in shape.path_elements(0.1) {
        match transform * element {
            PathEl::MoveTo(p) => {
                clip.close();
                clip.move_to(p);
            }
            PathEl::LineTo(p) => clip.line(p),
            PathEl::QuadTo(p1, p2) => clip.quad(QuadBez::new(clip.at, p1, p2), 0),
            PathEl::CurveTo(p1, p2, p3) => clip.cubic(CubicBez::new(clip.at, p1, p2, p3), 0),
            PathEl::ClosePath => clip.close(),
        }
    }
    clip.close();
    clip.builder.finish()
}

/// `path`, as tiny-skia stores it, as a kurbo shape.
pub(super) fn kurbo_path(path: &tiny_skia::Path) -> kurbo::BezPath {
    use tiny_skia::PathSegment;
    let point = |p: tiny_skia::Point| Point::new(f64::from(p.x), f64::from(p.y));
    let mut bez = kurbo::BezPath::new();
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(p) => bez.move_to(point(p)),
            PathSegment::LineTo(p) => bez.line_to(point(p)),
            PathSegment::QuadTo(p1, p) => bez.quad_to(point(p1), point(p)),
            PathSegment::CubicTo(p1, p2, p) => bez.curve_to(point(p1), point(p2), point(p)),
            PathSegment::Close => bez.close_path(),
        }
    }
    bez
}

struct Clip {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    builder: PathBuilder,
    /// The current point, unprojected.
    at: Point,
    /// Where the subpath started: the current point once it closes.
    start: Point,
    /// Whether a subpath is started.
    open: bool,
}

impl Clip {
    fn project(&self, p: Point) -> (f32, f32) {
        // Within the canvas and its margin: exact enough in f32.
        #[allow(clippy::cast_possible_truncation)]
        (
            p.x.clamp(self.left, self.right) as f32,
            p.y.clamp(self.top, self.bottom) as f32,
        )
    }

    fn inside(&self, p: Point) -> bool {
        (self.left..=self.right).contains(&p.x) && (self.top..=self.bottom).contains(&p.y)
    }

    fn move_to(&mut self, p: Point) {
        let (x, y) = self.project(p);
        self.builder.move_to(x, y);
        self.at = p;
        self.start = p;
        self.open = true;
    }

    fn line_to(&mut self, p: Point) {
        if !self.open {
            self.move_to(self.at);
        }
        let (x, y) = self.project(p);
        self.builder.line_to(x, y);
        self.at = p;
    }

    /// Ends the subpath with the line back to its start that filling it
    /// draws, closed or not: projected like any other, not the line between
    /// the projected ends.
    fn close(&mut self) {
        if self.open {
            self.line(self.start);
            self.builder.close();
            self.open = false;
        }
        self.at = self.start;
    }

    /// A line to `to`, split where it crosses an edge line of the
    /// rectangle: between those, projection moves it as a line.
    fn line(&mut self, to: Point) {
        let from = self.at;
        let mut cuts = [0.0_f64; 4];
        let mut count = 0;
        for (a, b, edge) in [
            (from.x, to.x, self.left),
            (from.x, to.x, self.right),
            (from.y, to.y, self.top),
            (from.y, to.y, self.bottom),
        ] {
            if (a < edge) != (b < edge) {
                let t = (edge - a) / (b - a);
                if t > 0.0 && t < 1.0 {
                    cuts[count] = t;
                    count += 1;
                }
            }
        }
        let cuts = &mut cuts[..count];
        cuts.sort_by(f64::total_cmp);
        for t in cuts.iter() {
            self.line_to(from.lerp(to, *t));
        }
        self.line_to(to);
    }

    /// Where `points` (a curve's control points, which hold it) lie: inside
    /// the rectangle, or all on the outer side of one of its edge lines.
    fn side(&self, points: &[Point]) -> Side {
        if points.iter().all(|p| self.inside(*p)) {
            Side::Inside
        } else if points.iter().all(|p| p.x <= self.left)
            || points.iter().all(|p| p.x >= self.right)
            || points.iter().all(|p| p.y <= self.top)
            || points.iter().all(|p| p.y >= self.bottom)
        {
            Side::Outside
        } else {
            Side::Across
        }
    }

    fn small(points: &[Point]) -> bool {
        let first = points[0];
        points
            .iter()
            .all(|p| (p.x - first.x).abs() < FLAT && (p.y - first.y).abs() < FLAT)
    }

    fn quad(&mut self, quad: QuadBez, depth: u32) {
        let points = [quad.p0, quad.p1, quad.p2];
        match self.side(&points) {
            Side::Inside => {
                if !self.open {
                    self.move_to(self.at);
                }
                let ((x1, y1), (x, y)) = (self.project(quad.p1), self.project(quad.p2));
                self.builder.quad_to(x1, y1, x, y);
                self.at = quad.p2;
            }
            Side::Across if depth < MAX_DEPTH && !Self::small(&points) && finite(&points) => {
                let (a, b) = quad.subdivide();
                self.quad(a, depth + 1);
                self.quad(b, depth + 1);
            }
            // Projected onto one edge line, or too small to bend: a line.
            Side::Outside | Side::Across => self.line_to(quad.p2),
        }
    }

    fn cubic(&mut self, cubic: CubicBez, depth: u32) {
        let points = [cubic.p0, cubic.p1, cubic.p2, cubic.p3];
        match self.side(&points) {
            Side::Inside => {
                if !self.open {
                    self.move_to(self.at);
                }
                let ((x1, y1), (x2, y2), (x, y)) = (
                    self.project(cubic.p1),
                    self.project(cubic.p2),
                    self.project(cubic.p3),
                );
                self.builder.cubic_to(x1, y1, x2, y2, x, y);
                self.at = cubic.p3;
            }
            Side::Across if depth < MAX_DEPTH && !Self::small(&points) && finite(&points) => {
                let (a, b) = cubic.subdivide();
                self.cubic(a, depth + 1);
                self.cubic(b, depth + 1);
            }
            Side::Outside | Side::Across => self.line_to(cubic.p3),
        }
    }
}

fn finite(points: &[Point]) -> bool {
    points.iter().all(|p| p.x.is_finite() && p.y.is_finite())
}

enum Side {
    Inside,
    Outside,
    Across,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurbo::{BezPath, Ellipse, Rect};
    use resvg::tiny_skia::{FillRule, Mask, Transform};

    const SIZE: (u32, u32) = (61, 47);

    /// Coverage filling `shape` through `transform` paints on a small
    /// canvas.
    fn coverage(path: &tiny_skia::Path, rule: FillRule, transform: Transform) -> Vec<u8> {
        let mut mask = Mask::new(SIZE.0, SIZE.1).expect("mask");
        mask.fill_path(path, rule, true, transform);
        mask.data().to_vec()
    }

    /// The largest and the summed difference between two coverages.
    fn difference(a: &[u8], b: &[u8]) -> (u8, u64) {
        a.iter().zip(b).fold((0, 0), |(worst, total), (a, b)| {
            (worst.max(a.abs_diff(*b)), total + u64::from(a.abs_diff(*b)))
        })
    }

    /// Holds that the projection of `shape` covers the canvas as the shape
    /// does: against the shape finely flattened to lines (which tiny-skia
    /// draws exactly), it differs about as much as tiny-skia's own drawing of
    /// the shape does. tiny-skia flattens each curve coarsely, by its size,
    /// and projection hands it different pieces: along edges coverage may
    /// differ by up to 96 (six antialiasing steps, the most tiny-skia's own
    /// drawing differs by over these cases), and in all by a few pixels more
    /// than tiny-skia's own. A wrong winding would differ by whole pixels
    /// over whole areas.
    fn assert_close(shape: &impl Shape, transform: Affine, rule: FillRule, what: &str) {
        let mut lines = PathBuilder::new();
        kurbo::flatten(transform * shape.to_path(0.1), 0.002, |element| {
            #[allow(clippy::cast_possible_truncation)]
            match element {
                PathEl::MoveTo(p) => lines.move_to(p.x as f32, p.y as f32),
                PathEl::LineTo(p) => lines.line_to(p.x as f32, p.y as f32),
                PathEl::ClosePath => lines.close(),
                _ => unreachable!("flattened"),
            }
        });
        let exact = coverage(
            &lines.finish().expect("a path"),
            rule,
            Transform::identity(),
        );
        let own = coverage(
            &super::super::path_of(shape).expect("a path"),
            rule,
            super::super::skia_transform(transform),
        );
        let projected = fill_path(shape, transform, SIZE).map_or_else(
            || vec![0; exact.len()],
            |path| coverage(&path, rule, Transform::identity()),
        );
        let (own_worst, own_total) = difference(&exact, &own);
        let (worst, total) = difference(&exact, &projected);
        assert!(
            worst <= 96 && total <= own_total + 1_024,
            "{what}: projected coverage differs by up to {worst} ({total} in all), \
             tiny-skia's own by up to {own_worst} ({own_total})"
        );
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            #[allow(clippy::cast_precision_loss)]
            let unit = (self.0 >> 11) as f64 / (1_u64 << 53) as f64;
            unit
        }
        /// Near the canvas or up to `far` from it.
        fn coordinate(&mut self, far: f64) -> f64 {
            if self.next() < 0.5 {
                self.next() * 80.0 - 10.0
            } else {
                (self.next() * 2.0 - 1.0) * far
            }
        }
    }

    /// Paths reaching well past the canvas, at distances tiny-skia still
    /// draws exactly, cover each pixel the same whether drawn as they are or
    /// projected: lines, curves, several subpaths, both fill rules.
    #[test]
    fn a_projected_path_covers_the_canvas_as_the_path_does() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..2_000 {
            let far = [100.0, 1_000.0, 20_000.0][case % 3];
            let mut path = BezPath::new();
            for _ in 0..=case % 3 {
                path.move_to((rng.coordinate(far), rng.coordinate(far)));
                for _ in 0..=case % 5 {
                    let pick = rng.next();
                    if pick < 1.0 / 3.0 {
                        path.line_to((rng.coordinate(far), rng.coordinate(far)));
                    } else if pick < 2.0 / 3.0 {
                        path.quad_to(
                            (rng.coordinate(far), rng.coordinate(far)),
                            (rng.coordinate(far), rng.coordinate(far)),
                        );
                    } else {
                        path.curve_to(
                            (rng.coordinate(far), rng.coordinate(far)),
                            (rng.coordinate(far), rng.coordinate(far)),
                            (rng.coordinate(far), rng.coordinate(far)),
                        );
                    }
                }
                if rng.next() < 0.7 {
                    path.close_path();
                }
            }
            let rule = if case % 2 == 0 {
                FillRule::Winding
            } else {
                FillRule::EvenOdd
            };
            assert_close(
                &path,
                Affine::IDENTITY,
                rule,
                &format!("case {case}: {path:?}"),
            );
        }
    }

    #[test]
    fn shapes_scaled_far_past_the_canvas_are_projected_as_drawn() {
        let cases: [(&str, BezPath, Affine); 4] = [
            (
                "a rectangle a thousand canvases tall",
                Rect::new(-10.0, -10.0, 10.0, 10.0).to_path(0.1),
                Affine::translate((30.0, 20.0)) * Affine::scale_non_uniform(1.0, 2_000.0),
            ),
            (
                "an ellipse whose edge crosses the canvas",
                Ellipse::new((0.0, 0.0), (50.0, 30.0), 0.3).to_path(0.1),
                Affine::translate((-900.0, 20.0)) * Affine::scale(19.0),
            ),
            (
                "a rotated, sheared square",
                Rect::new(0.0, 0.0, 100.0, 100.0).to_path(0.1),
                Affine::new([3.0, 40.0, -25.0, 2.0, 30.0, -500.0]),
            ),
            (
                "a ring around the canvas",
                {
                    let mut ring = Ellipse::new((30.0, 23.0), (5_000.0, 5_000.0), 0.0).to_path(0.1);
                    ring.extend(Ellipse::new((30.0, 23.0), (20.0, 20.0), 0.0).to_path(0.1));
                    ring
                },
                Affine::IDENTITY,
            ),
        ];
        for (what, shape, transform) in cases {
            for rule in [FillRule::Winding, FillRule::EvenOdd] {
                assert_close(&shape, transform, rule, what);
            }
        }
    }

    #[test]
    fn a_path_within_reach_is_drawn_as_it_is() {
        let square = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(within(&square, Affine::scale(6_000.0), 0.0, SIZE));
        assert!(!within(&square, Affine::scale(7_000.0), 0.0, SIZE));
        assert!(!within(&square, Affine::IDENTITY, 70_000.0, SIZE));
        assert!(!within(&square, Affine::scale(f64::NAN), 0.0, SIZE));
    }
}
