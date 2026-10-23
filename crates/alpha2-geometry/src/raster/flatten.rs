//! Curves to lines, with Wang's formula for how many, using only
//! `+ - * /` and `sqrt` so the lines are the same bits on every target.

use kurbo::{BezPath, PathEl, Point};

/// Flattening tolerance, in device pixels.
const TOLERANCE: f64 = 0.2;

/// Most lines one curve is cut into.
const MAX_SUBDIVISIONS: u32 = 256;

fn norm(x: f64, y: f64) -> f64 {
    (x * x + y * y).sqrt()
}

/// Lines a curve needs whose degree-scaled second differences are `scaled`
/// (Wang: `d (d - 1) / 8` times the largest).
fn subdivisions(scaled: f64) -> u32 {
    let n = (scaled / TOLERANCE).sqrt().ceil();
    if n.is_nan() || n < 1.0 {
        1
    } else if n > f64::from(MAX_SUBDIVISIONS) {
        MAX_SUBDIVISIONS
    } else {
        // In 1..=256 here.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let count = n as u32;
        count
    }
}

/// `path` as lines `[x0, y0, x1, y1]` relative to `origin`, every subpath
/// closed as filling closes it. Zero-length lines are left out.
// Bernstein weights and Wang's counts, named as the formulas name them.
#[allow(clippy::many_single_char_names)]
pub(crate) fn flatten(path: &BezPath, origin: Point, out: &mut Vec<[f64; 4]>) {
    out.clear();
    let (ox, oy) = (origin.x, origin.y);
    let line = |out: &mut Vec<[f64; 4]>, a: Point, b: Point| {
        if a != b {
            out.push([a.x - ox, a.y - oy, b.x - ox, b.y - oy]);
        }
    };
    let mut start: Option<Point> = None;
    let mut last = Point::ZERO;
    for element in path.elements() {
        match *element {
            PathEl::MoveTo(p) => {
                if let Some(s) = start {
                    line(out, last, s);
                }
                start = Some(p);
                last = p;
            }
            PathEl::LineTo(p) => {
                line(out, last, p);
                last = p;
            }
            PathEl::QuadTo(p1, p2) => {
                let p0 = last;
                let n =
                    subdivisions(0.25 * norm(p0.x - 2.0 * p1.x + p2.x, p0.y - 2.0 * p1.y + p2.y));
                let mut previous = p0;
                for i in 1..=n {
                    let p = if i == n {
                        p2
                    } else {
                        let t = f64::from(i) / f64::from(n);
                        let mt = 1.0 - t;
                        let (a, b, c) = (mt * mt, 2.0 * mt * t, t * t);
                        Point::new(
                            a * p0.x + b * p1.x + c * p2.x,
                            a * p0.y + b * p1.y + c * p2.y,
                        )
                    };
                    line(out, previous, p);
                    previous = p;
                }
                last = p2;
            }
            PathEl::CurveTo(p1, p2, p3) => {
                let p0 = last;
                let d0 = norm(p0.x - 2.0 * p1.x + p2.x, p0.y - 2.0 * p1.y + p2.y);
                let d1 = norm(p1.x - 2.0 * p2.x + p3.x, p1.y - 2.0 * p2.y + p3.y);
                let n = subdivisions(0.75 * d0.max(d1));
                let mut previous = p0;
                for i in 1..=n {
                    let p = if i == n {
                        p3
                    } else {
                        let t = f64::from(i) / f64::from(n);
                        let mt = 1.0 - t;
                        let (a, b, c, d) =
                            (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
                        Point::new(
                            a * p0.x + b * p1.x + c * p2.x + d * p3.x,
                            a * p0.y + b * p1.y + c * p2.y + d * p3.y,
                        )
                    };
                    line(out, previous, p);
                    previous = p;
                }
                last = p3;
            }
            PathEl::ClosePath => {
                if let Some(s) = start {
                    line(out, last, s);
                    last = s;
                }
            }
        }
    }
    if let Some(s) = start {
        line(out, last, s);
    }
}
