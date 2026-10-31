//! Stroke expansion into an outline filled with the nonzero rule.

use alpha2_model::{LineCap, LineJoin};
use kurbo::{Affine, BezPath, Cap, Join, Stroke, StrokeOpts};

/// Maximum deviation of kurbo's outline from the exact stroke, in device pixels.
const STROKE_TOLERANCE: f64 = 0.05;

/// Dash repeats beyond which the stroke is drawn solid.
const MAX_DASH_REPEATS: f64 = 100_000.0;

/// The length of a path's control polygon, an upper bound on its arc length.
fn control_length(path: &BezPath) -> f64 {
    let mut total = 0.0;
    let mut last = kurbo::Point::ZERO;
    let mut start = kurbo::Point::ZERO;
    for element in path.elements() {
        let points: &[kurbo::Point] = match element {
            kurbo::PathEl::MoveTo(p) => {
                last = *p;
                start = *p;
                continue;
            }
            kurbo::PathEl::LineTo(p) => std::slice::from_ref(p),
            kurbo::PathEl::QuadTo(a, b) => &[*a, *b],
            kurbo::PathEl::CurveTo(a, b, c) => &[*a, *b, *c],
            kurbo::PathEl::ClosePath => std::slice::from_ref(&start),
        };
        for point in points {
            total += crate::affine::distance(last, *point);
            last = *point;
        }
    }
    total
}

/// A stroke's style, in the pixels of the space it is drawn in.
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    /// Full width.
    pub width: f64,
    /// How segments meet.
    pub join: LineJoin,
    /// Longest a miter reaches, as a multiple of half the width.
    pub miter_limit: f64,
    /// How open ends look.
    pub cap: LineCap,
    /// Alternating dash and gap lengths; solid when empty or all zero.
    pub dash: Vec<f64>,
    /// How far into the dash pattern the stroke starts.
    pub dash_offset: f64,
}

/// The outline `style` covers along `path`, transformed to device pixels by `to_device`.
///
/// The pen is round in `path`'s space, so a non-uniform transform makes it elliptical, as in
/// After Effects. Empty for a width of zero or less.
#[must_use]
pub fn stroke_outline(path: &BezPath, style: &StrokeStyle, to_device: Affine) -> BezPath {
    if style.width.is_nan() || style.width <= 0.0 || path.elements().is_empty() {
        return BezPath::new();
    }
    let join = match style.join {
        LineJoin::Miter => Join::Miter,
        LineJoin::Round => Join::Round,
        LineJoin::Bevel => Join::Bevel,
    };
    let cap = match style.cap {
        LineCap::Butt => Cap::Butt,
        LineCap::Round => Cap::Round,
        LineCap::Square => Cap::Square,
    };
    let mut stroke = Stroke::new(style.width)
        .with_join(join)
        .with_miter_limit(style.miter_limit.max(1.0))
        .with_caps(cap);
    let pattern_length: f64 = style.dash.iter().sum();
    // Too many dashes for any frame to hold; draw solid instead.
    let repeats = control_length(path) / pattern_length;
    if pattern_length > 0.0 && repeats <= MAX_DASH_REPEATS {
        // An odd pattern repeats once more to pair dashes with gaps, as SVG reads it.
        let mut pattern = style.dash.clone();
        if pattern.len() % 2 == 1 {
            pattern.extend_from_slice(&style.dash);
        }
        stroke = stroke.with_dashes(style.dash_offset, pattern);
    }
    // Convert the device-pixel tolerance into the path's space.
    let scale = crate::affine::max_scale(to_device).max(1e-9);
    let outline = kurbo::stroke(
        path.iter(),
        &stroke,
        &StrokeOpts::default(),
        STROKE_TOLERANCE / scale,
    );
    to_device * outline
}
