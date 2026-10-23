//! Paths drawn far past the canvas, brought near it before they are
//! rasterized.
//!
//! The rasterizer accumulates in `f32` relative to its clip: a coordinate
//! of a million pixels (a shape scaled up a thousandfold) would lose the
//! precision coverage needs. So a path whose bounds reach past
//! [`MAX_DEVICE`] has every point outside the canvas (and [`MARGIN`] pixels
//! around it) projected onto that rectangle's edge, each coordinate clamped
//! to its range. Projection never moves a point across the inside of the
//! rectangle, so the path's winding number at every point inside it is
//! unchanged, and so is what any fill rule paints there; what it adds lies
//! on the edge, outside the canvas. A line is split where it crosses the
//! rectangle's edge lines, where projection bends it; a curve is split until
//! each piece is inside the rectangle (kept), on the outer side of one edge
//! line (projected to a line along it), or too small to matter (a line).

use kurbo::{BezPath, CubicBez, ParamCurve, PathEl, Point, QuadBez};

/// How far past the canvas, in device pixels, a path may reach and still be
/// rasterized as it is.
pub const MAX_DEVICE: f64 = 65_536.0;

/// How far outside the canvas the rectangle a far path is projected onto
/// lies: antialiasing touches one pixel past an edge, and the projected
/// edges must touch none of the canvas.
const MARGIN: f64 = 64.0;

/// A curve piece smaller than this, in device pixels, is drawn as a line.
const FLAT: f64 = 1.0 / 64.0;

/// How many times a curve is halved at most.
const MAX_DEPTH: u32 = 64;

/// `path` as it fills a `width` by `height` canvas: itself when it stays
/// within [`MAX_DEVICE`] of the canvas, or projected onto the canvas and a
/// 64-pixel margin around it. Every subpath of the projection is closed, as
/// filling closes it. A non-finite coordinate leaves nothing.
#[must_use]
pub fn project_far(path: &BezPath, width: u32, height: u32) -> BezPath {
    let bounds = kurbo::Shape::bounding_box(path);
    let (w, h) = (f64::from(width), f64::from(height));
    let finite = bounds.x0.is_finite()
        && bounds.y0.is_finite()
        && bounds.x1.is_finite()
        && bounds.y1.is_finite();
    if !finite {
        return BezPath::new();
    }
    if bounds.x0 >= -MAX_DEVICE
        && bounds.y0 >= -MAX_DEVICE
        && bounds.x1 <= w + MAX_DEVICE
        && bounds.y1 <= h + MAX_DEVICE
    {
        return path.clone();
    }
    let mut clip = Clip {
        left: -MARGIN,
        top: -MARGIN,
        right: w + MARGIN,
        bottom: h + MARGIN,
        out: BezPath::new(),
        at: Point::ZERO,
        start: Point::ZERO,
        open: false,
    };
    for element in path.elements() {
        match *element {
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
    clip.out
}

struct Clip {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    out: BezPath,
    /// The current point, unprojected.
    at: Point,
    /// Where the subpath started: the current point once it closes.
    start: Point,
    /// Whether a subpath is started.
    open: bool,
}

enum Side {
    Inside,
    Outside,
    Across,
}

impl Clip {
    fn project(&self, p: Point) -> Point {
        Point::new(
            p.x.clamp(self.left, self.right),
            p.y.clamp(self.top, self.bottom),
        )
    }

    fn inside(&self, p: Point) -> bool {
        (self.left..=self.right).contains(&p.x) && (self.top..=self.bottom).contains(&p.y)
    }

    fn move_to(&mut self, p: Point) {
        let projected = self.project(p);
        self.out.move_to(projected);
        self.at = p;
        self.start = p;
        self.open = true;
    }

    fn line_to(&mut self, p: Point) {
        if !self.open {
            self.move_to(self.at);
        }
        let projected = self.project(p);
        self.out.line_to(projected);
        self.at = p;
    }

    /// Ends the subpath with the line back to its start that filling it
    /// draws, projected like any other.
    fn close(&mut self) {
        if self.open {
            self.line(self.start);
            self.out.close_path();
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

    /// Where `points` (a curve's control points, whose hull contains the curve) lie.
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
                self.out.quad_to(quad.p1, quad.p2);
                self.at = quad.p2;
            }
            Side::Across if depth < MAX_DEPTH && !Self::small(&points) => {
                let (a, b) = quad.subdivide();
                self.quad(a, depth + 1);
                self.quad(b, depth + 1);
            }
            Side::Outside | Side::Across => self.line(quad.p2),
        }
    }

    fn cubic(&mut self, cubic: CubicBez, depth: u32) {
        let points = [cubic.p0, cubic.p1, cubic.p2, cubic.p3];
        match self.side(&points) {
            Side::Inside => {
                if !self.open {
                    self.move_to(self.at);
                }
                self.out.curve_to(cubic.p1, cubic.p2, cubic.p3);
                self.at = cubic.p3;
            }
            Side::Across if depth < MAX_DEPTH && !Self::small(&points) => {
                let (a, b) = cubic.subdivide();
                self.cubic(a, depth + 1);
                self.cubic(b, depth + 1);
            }
            Side::Outside | Side::Across => self.line(cubic.p3),
        }
    }
}
