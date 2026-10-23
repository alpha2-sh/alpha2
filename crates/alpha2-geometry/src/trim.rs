//! Trim paths: keeping a span of geometry's length.

use crate::arclen;
use kurbo::{Affine, BezPath, ParamCurve, PathEl, PathSeg, Point};

/// A subpath as its segments, closed ones including the segment back to the
/// start.
#[derive(Clone, Debug, PartialEq)]
pub struct Subpath {
    /// The segments, in order.
    pub segments: Vec<PathSeg>,
    /// Whether the last segment ends where the first starts, joined.
    pub closed: bool,
    /// Where it starts, for a subpath of no segments.
    pub start: Point,
}

/// The subpaths of `path`, each closed one with its closing segment.
#[must_use]
pub fn subpaths(path: &BezPath) -> Vec<Subpath> {
    let mut found: Vec<Subpath> = Vec::new();
    let mut current: Option<Subpath> = None;
    let mut last = Point::ZERO;
    for element in path.elements() {
        match *element {
            PathEl::MoveTo(p) => {
                found.extend(current.take());
                current = Some(Subpath {
                    segments: Vec::new(),
                    closed: false,
                    start: p,
                });
                last = p;
            }
            PathEl::LineTo(p) => {
                let subpath = current.get_or_insert_with(|| open_at(last));
                subpath
                    .segments
                    .push(PathSeg::Line(kurbo::Line::new(last, p)));
                last = p;
            }
            PathEl::QuadTo(a, b) => {
                let subpath = current.get_or_insert_with(|| open_at(last));
                subpath
                    .segments
                    .push(PathSeg::Quad(kurbo::QuadBez::new(last, a, b)));
                last = b;
            }
            PathEl::CurveTo(a, b, c) => {
                let subpath = current.get_or_insert_with(|| open_at(last));
                subpath
                    .segments
                    .push(PathSeg::Cubic(kurbo::CubicBez::new(last, a, b, c)));
                last = c;
            }
            PathEl::ClosePath => {
                if let Some(mut subpath) = current.take() {
                    if last != subpath.start {
                        subpath
                            .segments
                            .push(PathSeg::Line(kurbo::Line::new(last, subpath.start)));
                    }
                    subpath.closed = true;
                    last = subpath.start;
                    found.push(subpath);
                }
            }
        }
    }
    found.extend(current);
    found
}

fn open_at(point: Point) -> Subpath {
    Subpath {
        segments: Vec::new(),
        closed: false,
        start: point,
    }
}

/// Appends `subpath` to `path`.
pub fn append(path: &mut BezPath, subpath: &Subpath) {
    let Some(first) = subpath.segments.first() else {
        return;
    };
    path.move_to(first.start());
    for segment in &subpath.segments {
        match *segment {
            PathSeg::Line(line) => path.line_to(line.p1),
            PathSeg::Quad(quad) => path.quad_to(quad.p1, quad.p2),
            PathSeg::Cubic(cubic) => path.curve_to(cubic.p1, cubic.p2, cubic.p3),
        }
    }
    if subpath.closed {
        path.close_path();
    }
}

/// What a trim keeps.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TrimSpan {
    /// Start, as a fraction of the length, in `0..=1`.
    pub start: f64,
    /// End, as a fraction of the length, in `0..=1`.
    pub end: f64,
    /// Turns the span is shifted by.
    pub offset: f64,
}

impl TrimSpan {
    /// The kept fractions of a length, in order along it: none when the span
    /// is empty, the whole when it covers it, two when it wraps past the end.
    fn ranges(self) -> Ranges {
        if self.start.is_nan() || self.end.is_nan() || self.start >= self.end {
            return Ranges::None;
        }
        if self.end - self.start >= 1.0 {
            return Ranges::Whole;
        }
        let shift = self.offset - self.offset.floor();
        let mut start = self.start + shift;
        let mut end = self.end + shift;
        if start >= 1.0 {
            start -= 1.0;
            end -= 1.0;
        }
        if end > 1.0 {
            Ranges::Wrapped(start, end - 1.0)
        } else {
            Ranges::One(start, end)
        }
    }
}

enum Ranges {
    None,
    Whole,
    One(f64, f64),
    /// `start..1` and then `0..end`.
    Wrapped(f64, f64),
}

/// One subpath and the transform its lengths are measured through.
#[derive(Clone, Debug)]
pub struct Measured {
    /// The subpath, in its own space.
    pub subpath: Subpath,
    /// Maps the subpath's points into the space its lengths are measured in.
    pub measure: Affine,
}

/// Each segment's length through `measure`.
fn segment_lengths(subpath: &Subpath, measure: Affine) -> Vec<f64> {
    subpath
        .segments
        .iter()
        .map(|segment| arclen::length(&transformed(*segment, measure)))
        .collect()
}

fn transformed(segment: PathSeg, affine: Affine) -> PathSeg {
    affine * segment
}

/// The shortest piece of a segment a cut keeps, in measured pixels. A span
/// that ends on a segment boundary can reach past it by rounding; the
/// sliver that leaves has no usable tangent, and stroking it puts NaN in
/// the outline.
const MIN_PIECE_LENGTH: f64 = 1e-6;

/// The part of `subpath` between `from` and `to` (lengths along it, in the
/// measured space), as an open subpath in its own space.
fn cut(subpath: &Subpath, lengths: &[f64], measure: Affine, from: f64, to: f64) -> Subpath {
    let mut segments = Vec::new();
    let mut run = 0.0;
    for (segment, &length) in subpath.segments.iter().zip(lengths) {
        let (start, end) = (run, run + length);
        run = end;
        if to.min(end) - from.max(start) < MIN_PIECE_LENGTH {
            continue;
        }
        let measured = transformed(*segment, measure);
        let t0 = if from > start {
            arclen::parameter_at(&measured, from - start, length)
        } else {
            0.0
        };
        let t1 = if to < end {
            arclen::parameter_at(&measured, to - start, length)
        } else {
            1.0
        };
        if t1 > t0 {
            let whole = t0 <= 0.0 && t1 >= 1.0;
            segments.push(if whole {
                *segment
            } else {
                segment.subsegment(t0..t1)
            });
        }
    }
    let start = segments.first().map_or(subpath.start, PathSeg::start);
    Subpath {
        segments,
        closed: false,
        start,
    }
}

/// Keeps `span` of `parts` together: of their lengths laid end to end in
/// order. A span that wraps past the end continues from the start; around a
/// single closed subpath the two pieces join into one. Each piece comes with
/// the index of the part it was cut from.
#[must_use]
pub fn trim_together(parts: &[Measured], span: TrimSpan) -> Vec<(usize, Subpath)> {
    let lengths: Vec<Vec<f64>> = parts
        .iter()
        .map(|part| segment_lengths(&part.subpath, part.measure))
        .collect();
    let totals: Vec<f64> = lengths.iter().map(|list| list.iter().sum()).collect();
    let total: f64 = totals.iter().sum();
    let pieces = |from: f64, to: f64| -> Vec<(usize, Subpath)> {
        let mut kept = Vec::new();
        let mut offset = 0.0;
        for (index, part) in parts.iter().enumerate() {
            let (start, end) = (offset, offset + totals[index]);
            offset = end;
            if end <= from || start >= to {
                continue;
            }
            if from <= start && to >= end {
                kept.push((index, part.subpath.clone()));
            } else {
                kept.push((
                    index,
                    cut(
                        &part.subpath,
                        &lengths[index],
                        part.measure,
                        from - start,
                        to - start,
                    ),
                ));
            }
        }
        kept
    };
    match span.ranges() {
        Ranges::None => Vec::new(),
        Ranges::Whole => parts
            .iter()
            .enumerate()
            .map(|(index, part)| (index, part.subpath.clone()))
            .collect(),
        Ranges::One(start, end) => pieces(start * total, end * total),
        Ranges::Wrapped(start, end) => {
            let mut tail = pieces(start * total, total);
            let head = pieces(0.0, end * total);
            let joined = parts.len() == 1 && parts[0].subpath.closed;
            if joined && let (Some((_, last)), Some((_, first))) = (tail.last_mut(), head.first()) {
                last.segments.extend(first.segments.iter().copied());
                return tail;
            }
            tail.into_iter().chain(head).collect()
        }
    }
}

/// Keeps `span` of each of `parts` by its own length, each piece with the
/// index of its part.
#[must_use]
pub fn trim_individually(parts: &[Measured], span: TrimSpan) -> Vec<(usize, Subpath)> {
    parts
        .iter()
        .enumerate()
        .flat_map(|(index, part)| {
            trim_together(std::slice::from_ref(part), span)
                .into_iter()
                .map(move |(_, piece)| (index, piece))
        })
        .collect()
}

/// The length of `subpath` through `measure`.
#[must_use]
pub fn subpath_length(subpath: &Subpath, measure: Affine) -> f64 {
    segment_lengths(subpath, measure).iter().sum()
}

/// The point `distance` along `subpath` (in its own space; lengths through
/// `measure`), clamped to its ends.
#[must_use]
pub fn point_at_length(subpath: &Subpath, measure: Affine, distance: f64) -> Point {
    let mut run = 0.0;
    for segment in &subpath.segments {
        let measured = transformed(*segment, measure);
        let length = arclen::length(&measured);
        if distance <= run + length && length > 0.0 {
            let t = arclen::parameter_at(&measured, distance - run, length);
            return segment.eval(t);
        }
        run += length;
    }
    subpath.segments.last().map_or(subpath.start, PathSeg::end)
}
