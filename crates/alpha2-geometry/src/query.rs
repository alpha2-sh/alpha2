//! What an editor asks of shapes: where items are, which one is under a
//! point, how long a path is, and paths cut, resampled, or converted. Each
//! answer comes from the code the renderer draws with.
//!
//! Inputs and outputs are normalized as the model is, on a picture of a
//! given size; lengths and distances are in its pixels.

use crate::tree::{ItemPath, build_scene_with_items};
use crate::trim::{self, Measured, TrimSpan};
use crate::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedPath, EvaluatedSubpath, EvaluatedVertex,
    VectorOp, affine, arclen, raster,
};
use alpha2_model::FillRule;
use kurbo::{Affine, BezPath, ParamCurve, ParamCurveDeriv, PathSeg, Point, Rect, Vec2};

/// The normalized bounds of every item, in document order: geometry's
/// outline, a paint's area (a stroke's with its width), a group's items'.
/// Trims are not listed; an item with nothing to show has `None`.
#[must_use]
pub fn shape_bounds(items: &[EvaluatedItem], size: CanvasSize) -> Vec<(String, Option<Rect>)> {
    crate::item_paths(items, size)
        .into_iter()
        .map(|record| {
            let empty = record.bounds.width() <= 0.0 && record.bounds.height() <= 0.0;
            let bounds = (!empty).then(|| normalized_rect(record.bounds, size));
            (record.at, bounds)
        })
        .collect()
}

fn normalized_rect(rect: Rect, size: CanvasSize) -> Rect {
    let scale = size.scale();
    Rect::new(
        rect.x0 / scale.x,
        rect.y0 / scale.y,
        rect.x1 / scale.x,
        rect.y1 / scale.y,
    )
}

/// The front-most fill or stroke that paints the normalized `point`, by
/// its fill rule (a stroke by its outline): the item's place in the tree.
#[must_use]
pub fn hit_test(items: &[EvaluatedItem], size: CanvasSize, point: Point) -> Option<String> {
    let device = size.point(point.x, point.y);
    let (scene, owners) = build_scene_with_items(items, size);
    for (op, owner) in scene.ops.iter().zip(owners).rev() {
        if let VectorOp::Fill { path, rule, .. } = op {
            let winding = raster::winding_at(path, device);
            let inside = match rule {
                FillRule::Nonzero => winding != 0,
                FillRule::Evenodd => winding % 2 != 0,
            };
            if inside {
                return Some(owner);
            }
        }
    }
    None
}

/// The point of a path nearest another.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Nearest {
    /// The subpath.
    pub subpath: usize,
    /// The segment within it: segment `i` runs from vertex `i` to the next.
    pub segment: usize,
    /// The parameter along the segment, in `0..=1`.
    pub t: f64,
    /// The nearest point, normalized.
    pub point: Point,
    /// Its distance from the point asked about, in pixels.
    pub distance: f64,
}

/// The pixel subpaths of `path` on a picture of `size`, closed ones with
/// their closing segment.
fn pixel_subpaths(path: &EvaluatedPath, size: CanvasSize) -> Vec<trim::Subpath> {
    path.subpaths
        .iter()
        .map(|subpath| {
            let one = EvaluatedPath {
                subpaths: vec![subpath.clone()],
            };
            let bez = EvaluatedGeometry::Path(one.into()).to_path(size);
            trim::subpaths(&bez)
                .into_iter()
                .next()
                .unwrap_or(trim::Subpath {
                    segments: Vec::new(),
                    closed: subpath.closed,
                    start: subpath.vertices.first().map_or(Point::ZERO, |vertex| {
                        size.point(vertex.point.x, vertex.point.y)
                    }),
                })
        })
        .collect()
}

/// The point of `path` nearest the normalized `point`, measured in pixels.
#[must_use]
pub fn nearest_point(path: &EvaluatedPath, size: CanvasSize, point: Point) -> Option<Nearest> {
    let device = size.point(point.x, point.y);
    let scale = size.scale();
    let mut best: Option<Nearest> = None;
    for (index, subpath) in pixel_subpaths(path, size).iter().enumerate() {
        if subpath.segments.is_empty() {
            let distance = affine::distance(subpath.start, device);
            if best.is_none_or(|best| distance < best.distance) {
                best = Some(Nearest {
                    subpath: index,
                    segment: 0,
                    t: 0.0,
                    point: Point::new(subpath.start.x / scale.x, subpath.start.y / scale.y),
                    distance,
                });
            }
        }
        for (segment_index, segment) in subpath.segments.iter().enumerate() {
            let (t, distance) = nearest_on_segment(segment, device);
            if best.is_none_or(|best| distance < best.distance) {
                let at = segment.eval(t);
                best = Some(Nearest {
                    subpath: index,
                    segment: segment_index,
                    t,
                    point: Point::new(at.x / scale.x, at.y / scale.y),
                    distance,
                });
            }
        }
    }
    best
}

/// The parameter of `segment` nearest `point`, and the distance: the best
/// of 64 samples, refined by ternary search around it.
fn nearest_on_segment(segment: &PathSeg, point: Point) -> (f64, f64) {
    const SAMPLES: u32 = 64;
    let distance = |t: f64| affine::distance(segment.eval(t), point);
    let mut best = (0.0, distance(0.0));
    for step in 1..=SAMPLES {
        let t = f64::from(step) / f64::from(SAMPLES);
        let d = distance(t);
        if d < best.1 {
            best = (t, d);
        }
    }
    let span = 1.0 / f64::from(SAMPLES);
    let (mut low, mut high) = ((best.0 - span).max(0.0), (best.0 + span).min(1.0));
    for _ in 0..40 {
        let third = (high - low) / 3.0;
        let (a, b) = (low + third, high - third);
        if distance(a) < distance(b) {
            high = b;
        } else {
            low = a;
        }
    }
    let t = f64::midpoint(low, high);
    let d = distance(t);
    if d < best.1 { (t, d) } else { best }
}

/// Each subpath's length in pixels.
#[must_use]
pub fn path_lengths(path: &EvaluatedPath, size: CanvasSize) -> Vec<f64> {
    pixel_subpaths(path, size)
        .iter()
        .map(|subpath| trim::subpath_length(subpath, Affine::IDENTITY))
        .collect()
}

/// `path` trimmed as a trim item trims it, together: the kept span of its
/// subpaths' lengths in pixels, normalized again.
#[must_use]
pub fn trim_path(
    path: &EvaluatedPath,
    size: CanvasSize,
    start: f64,
    end: f64,
    offset: f64,
) -> EvaluatedPath {
    let parts: Vec<Measured> = pixel_subpaths(path, size)
        .into_iter()
        .map(|subpath| Measured {
            subpath,
            measure: Affine::IDENTITY,
        })
        .collect();
    let kept = trim::trim_together(&parts, TrimSpan { start, end, offset });
    let mut bez = BezPath::new();
    for (_, subpath) in &kept {
        trim::append(&mut bez, subpath);
    }
    from_bez(&bez, size)
}

/// `geometry` as a path of the same outline, normalized: the editor's
/// "convert to path".
#[must_use]
pub fn to_path(geometry: &EvaluatedGeometry, size: CanvasSize) -> EvaluatedPath {
    from_bez(&geometry.to_path(size), size)
}

/// A pixel path as vertices and tangents, normalized. Quadratic segments
/// are raised to cubics; a closed subpath's closing line back to its start
/// is left implicit.
#[must_use]
pub fn from_bez(path: &BezPath, size: CanvasSize) -> EvaluatedPath {
    let scale = size.scale();
    let point = |p: Point| Point::new(p.x / scale.x, p.y / scale.y);
    let vector = |v: Vec2| Vec2::new(v.x / scale.x, v.y / scale.y);
    let subpaths = trim::subpaths(path)
        .into_iter()
        .map(|subpath| {
            let mut segments = subpath.segments.clone();
            // The closing segment, when a straight line, is the implicit one.
            if subpath.closed
                && let Some(PathSeg::Line(_)) = segments.last()
                && segments.len() > 1
            {
                segments.pop();
            }
            let mut vertices = vec![EvaluatedVertex {
                point: point(subpath.start),
                ..EvaluatedVertex::default()
            }];
            for segment in &segments {
                let cubic = match *segment {
                    PathSeg::Line(line) => {
                        vertices.push(EvaluatedVertex {
                            point: point(line.p1),
                            ..EvaluatedVertex::default()
                        });
                        continue;
                    }
                    PathSeg::Quad(quad) => quad.raise(),
                    PathSeg::Cubic(cubic) => cubic,
                };
                if let Some(last) = vertices.last_mut() {
                    last.out_tangent = vector(cubic.p1 - cubic.p0);
                }
                vertices.push(EvaluatedVertex {
                    point: point(cubic.p3),
                    in_tangent: vector(cubic.p2 - cubic.p3),
                    out_tangent: Vec2::ZERO,
                });
            }
            // A closed subpath's last segment ends on its first vertex: fold it in.
            if subpath.closed
                && vertices.len() > 1
                && let Some(last) = vertices.pop()
            {
                if last.point == vertices[0].point {
                    vertices[0].in_tangent = last.in_tangent;
                } else {
                    vertices.push(last);
                }
            }
            EvaluatedSubpath {
                closed: subpath.closed,
                vertices,
            }
        })
        .collect();
    EvaluatedPath { subpaths }
}

/// `path` with `counts[i]` vertices on subpath `i`, spaced evenly along its
/// pixel length, with tangents along the curve a third of the spacing long:
/// paths of different vertex counts made ready to morph. A subpath with no
/// count, or a count under 2 (3 when closed), is left as it is.
#[must_use]
pub fn resample_path(path: &EvaluatedPath, size: CanvasSize, counts: &[usize]) -> EvaluatedPath {
    let scale = size.scale();
    let subpaths = pixel_subpaths(path, size)
        .into_iter()
        .zip(&path.subpaths)
        .enumerate()
        .map(|(index, (pixels, original))| {
            let count = counts.get(index).copied().unwrap_or(0);
            let minimum = if original.closed { 3 } else { 2 };
            let total = trim::subpath_length(&pixels, Affine::IDENTITY);
            if count < minimum || total <= 0.0 {
                return original.clone();
            }
            // A closed subpath closes back to its first vertex, so its `count` vertices split it
            // into `count` spans; an open one's into `count - 1`.
            let spans = if original.closed { count } else { count - 1 };
            #[allow(clippy::cast_precision_loss)]
            let spacing = total / spans as f64;
            let vertices = (0..count)
                .map(|step| {
                    #[allow(clippy::cast_precision_loss)]
                    let distance = spacing * step as f64;
                    let (at, direction) = point_and_direction(&pixels, distance);
                    let handle = direction * (spacing / 3.0);
                    EvaluatedVertex {
                        point: Point::new(at.x / scale.x, at.y / scale.y),
                        in_tangent: Vec2::new(-handle.x / scale.x, -handle.y / scale.y),
                        out_tangent: Vec2::new(handle.x / scale.x, handle.y / scale.y),
                    }
                })
                .collect();
            EvaluatedSubpath {
                closed: original.closed,
                vertices,
            }
        })
        .collect();
    EvaluatedPath { subpaths }
}

/// The point `distance` along `subpath` and the unit direction of travel
/// there.
fn point_and_direction(subpath: &trim::Subpath, distance: f64) -> (Point, Vec2) {
    let mut run = 0.0;
    for segment in &subpath.segments {
        let length = arclen::length(segment);
        if distance <= run + length && length > 0.0 {
            let t = arclen::parameter_at(segment, distance - run, length);
            return (segment.eval(t), direction(segment, t));
        }
        run += length;
    }
    subpath
        .segments
        .last()
        .map_or((subpath.start, Vec2::ZERO), |segment| {
            (segment.end(), direction(segment, 1.0))
        })
}

fn direction(segment: &PathSeg, t: f64) -> Vec2 {
    let derivative = match segment {
        PathSeg::Line(line) => line.p1 - line.p0,
        PathSeg::Quad(quad) => quad.deriv().eval(t).to_vec2(),
        PathSeg::Cubic(cubic) => cubic.deriv().eval(t).to_vec2(),
    };
    let length = affine::length(derivative);
    if length > 0.0 {
        derivative / length
    } else {
        Vec2::ZERO
    }
}

/// Every item's [`ItemPath`] (place in the tree, device path, and bounds), in document order.
#[must_use]
pub fn items(items: &[EvaluatedItem], size: CanvasSize) -> Vec<ItemPath> {
    crate::item_paths(items, size)
}
