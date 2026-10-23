//! A shapes generator's content tree evaluated at a time.
//!
//! A [`ShapesPlan`] is made once, when the index is built: every subtree
//! with no keyframes is evaluated then and kept, shared, so drawing a frame
//! evaluates only what animates. A tree that does not animate at all is one
//! shared value, the same each frame.

use crate::animation::{sample_paint, sample_path, sample_point, sample_scalar, sample_vec2};
use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{EvaluatedGeometry, EvaluatedItem, EvaluatedTransform};
use alpha2_model::{
    Animated, AnimatedPoint, EllipseGeometry, PathGeometry, PolystarGeometry, RectangleGeometry,
    ShapeGeometry, ShapeItem, ShapeTransform,
};
use alpha2_time::MediaTime;
use std::sync::Arc;

/// A content tree, its parts that do not animate evaluated once.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapesPlan {
    items: Vec<Planned>,
    /// The whole tree, when nothing in it animates.
    fixed: Option<Arc<[EvaluatedItem]>>,
}

#[derive(Clone, Debug, PartialEq)]
enum Planned {
    /// An item with no keyframes, evaluated.
    Fixed(EvaluatedItem),
    /// A group with keyframes somewhere: its own transform, and its items,
    /// themselves planned (or all fixed and shared).
    Group {
        items: Vec<Planned>,
        fixed: Option<Arc<[EvaluatedItem]>>,
        transform: ShapeTransform,
    },
    /// Anything else with keyframes.
    Live(ShapeItem),
}

impl ShapesPlan {
    /// Plans `items`, evaluating every subtree that does not animate.
    #[must_use]
    pub fn new(items: &[ShapeItem]) -> Self {
        let items: Vec<Planned> = items.iter().map(plan).collect();
        let fixed = all_fixed(&items);
        Self { items, fixed }
    }

    /// The tree at `time`, on a picture `aspect` wide per unit high (for
    /// positions moving along spatial curves). A tree that does not animate
    /// gives the same shared value at every time.
    #[must_use]
    pub fn evaluate(&self, time: MediaTime, aspect: f64) -> Arc<[EvaluatedItem]> {
        if let Some(fixed) = &self.fixed {
            return Arc::clone(fixed);
        }
        evaluate_planned(&self.items, time, aspect)
    }

    /// Whether anything in the tree animates.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        self.fixed.is_none()
    }
}

fn plan(item: &ShapeItem) -> Planned {
    if !item.is_animated() {
        // No keyframes: time and aspect change nothing.
        return Planned::Fixed(evaluate_item(item, MediaTime::ZERO, 1.0));
    }
    match item {
        ShapeItem::Group { items, transform } => {
            let items: Vec<Planned> = items.iter().map(plan).collect();
            let fixed = all_fixed(&items);
            Planned::Group {
                items: if fixed.is_some() { Vec::new() } else { items },
                fixed,
                transform: transform.clone(),
            }
        }
        _ => Planned::Live(item.clone()),
    }
}

fn all_fixed(items: &[Planned]) -> Option<Arc<[EvaluatedItem]>> {
    items
        .iter()
        .map(|item| match item {
            Planned::Fixed(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(Arc::from)
}

fn evaluate_planned(items: &[Planned], time: MediaTime, aspect: f64) -> Arc<[EvaluatedItem]> {
    items
        .iter()
        .map(|item| match item {
            Planned::Fixed(item) => item.clone(),
            Planned::Group {
                items,
                fixed,
                transform,
            } => EvaluatedItem::Group {
                items: fixed
                    .clone()
                    .unwrap_or_else(|| evaluate_planned(items, time, aspect)),
                transform: evaluate_transform(transform, time, aspect),
            },
            Planned::Live(item) => evaluate_item(item, time, aspect),
        })
        .collect()
}

/// A whole content tree at `time`, without a plan.
#[must_use]
pub fn evaluate_items(items: &[ShapeItem], time: MediaTime, aspect: f64) -> Arc<[EvaluatedItem]> {
    items
        .iter()
        .map(|item| evaluate_item(item, time, aspect))
        .collect()
}

fn scalar(animation: &Animated<f32>, time: MediaTime) -> f64 {
    f64::from(sample_scalar(animation, time))
}

fn point(animation: &AnimatedPoint, time: MediaTime, aspect: f64) -> Point {
    let value = sample_point(animation, time, aspect);
    Point::new(f64::from(value.x), f64::from(value.y))
}

fn vector(animation: &Animated<alpha2_model::Vec2>, time: MediaTime) -> Vec2 {
    let value = sample_vec2(animation, time);
    Vec2::new(f64::from(value.x), f64::from(value.y))
}

/// One item at `time`. Opacities, trims, and roundness are clamped to
/// `0..=1` and lengths to zero or more, as an overshooting curve could carry
/// them past.
#[must_use]
pub fn evaluate_item(item: &ShapeItem, time: MediaTime, aspect: f64) -> EvaluatedItem {
    let unit = |value: f64| value.clamp(0.0, 1.0);
    match item {
        ShapeItem::Group { items, transform } => EvaluatedItem::Group {
            items: evaluate_items(items, time, aspect),
            transform: evaluate_transform(transform, time, aspect),
        },
        ShapeItem::Rectangle(geometry) => EvaluatedItem::Geometry(evaluate_geometry(
            &ShapeGeometry::Rectangle(geometry.clone()),
            time,
            aspect,
        )),
        ShapeItem::Ellipse(geometry) => EvaluatedItem::Geometry(ellipse(geometry, time, aspect)),
        ShapeItem::Polystar(geometry) => EvaluatedItem::Geometry(polystar(geometry, time, aspect)),
        ShapeItem::Path(geometry) => EvaluatedItem::Geometry(path(geometry, time)),
        ShapeItem::Fill {
            paint,
            opacity,
            rule,
        } => EvaluatedItem::Fill {
            paint: sample_paint(paint, time),
            opacity: unit(scalar(opacity, time)),
            rule: *rule,
        },
        ShapeItem::Stroke(stroke) => EvaluatedItem::Stroke {
            paint: sample_paint(&stroke.paint, time),
            width: scalar(&stroke.width, time).max(0.0),
            opacity: unit(scalar(&stroke.opacity, time)),
            join: stroke.join,
            miter_limit: f64::from(stroke.miter_limit),
            cap: stroke.cap,
            dash: stroke.dash.as_ref().map_or_else(Vec::new, |dash| {
                dash.pattern
                    .iter()
                    .map(|entry| scalar(entry, time).max(0.0))
                    .collect()
            }),
            dash_offset: stroke
                .dash
                .as_ref()
                .map_or(0.0, |dash| scalar(&dash.offset, time)),
        },
        ShapeItem::Trim(trim) => EvaluatedItem::Trim {
            start: unit(scalar(&trim.start, time)),
            end: unit(scalar(&trim.end, time)),
            offset: scalar(&trim.offset, time),
            mode: trim.mode,
        },
    }
}

/// A geometry at `time`: a mask's outline, or a content tree's.
#[must_use]
pub fn evaluate_geometry(
    geometry: &ShapeGeometry,
    time: MediaTime,
    aspect: f64,
) -> EvaluatedGeometry {
    match geometry {
        ShapeGeometry::Rectangle(RectangleGeometry {
            center,
            size,
            corner_radius,
            rotation_degrees,
        }) => EvaluatedGeometry::Rectangle {
            center: point(center, time, aspect),
            size: vector(size, time),
            corner_radius: scalar(corner_radius, time).max(0.0),
            rotation_degrees: scalar(rotation_degrees, time),
        },
        ShapeGeometry::Ellipse(geometry) => ellipse(geometry, time, aspect),
        ShapeGeometry::Polystar(geometry) => polystar(geometry, time, aspect),
        ShapeGeometry::Path(geometry) => path(geometry, time),
    }
}

fn ellipse(geometry: &EllipseGeometry, time: MediaTime, aspect: f64) -> EvaluatedGeometry {
    EvaluatedGeometry::Ellipse {
        center: point(&geometry.center, time, aspect),
        size: vector(&geometry.size, time),
        rotation_degrees: scalar(&geometry.rotation_degrees, time),
    }
}

fn polystar(geometry: &PolystarGeometry, time: MediaTime, aspect: f64) -> EvaluatedGeometry {
    EvaluatedGeometry::Polystar {
        kind: geometry.kind,
        center: point(&geometry.center, time, aspect),
        points: scalar(&geometry.points, time).clamp(
            f64::from(alpha2_model::MIN_POLYSTAR_POINTS),
            f64::from(alpha2_model::MAX_POLYSTAR_POINTS),
        ),
        outer_radius: scalar(&geometry.outer_radius, time).max(0.0),
        inner_radius: scalar(&geometry.inner_radius, time).max(0.0),
        outer_roundness: scalar(&geometry.outer_roundness, time).clamp(0.0, 1.0),
        inner_roundness: scalar(&geometry.inner_roundness, time).clamp(0.0, 1.0),
        rotation_degrees: scalar(&geometry.rotation_degrees, time),
    }
}

fn path(geometry: &PathGeometry, time: MediaTime) -> EvaluatedGeometry {
    EvaluatedGeometry::Path(Arc::new(sample_path(&geometry.path, time)))
}

fn evaluate_transform(
    transform: &ShapeTransform,
    time: MediaTime,
    aspect: f64,
) -> EvaluatedTransform {
    let anchor = vector(&transform.anchor, time);
    EvaluatedTransform {
        anchor: Point::new(anchor.x, anchor.y),
        position: point(&transform.position, time, aspect),
        scale: vector(&transform.scale, time),
        rotation_degrees: scalar(&transform.rotation_degrees, time),
        opacity: scalar(&transform.opacity, time).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
#[path = "shapes_tests.rs"]
mod tests;
