//! A shapes generator's content tree evaluated at a time.
//!
//! A [`ShapesPlan`] is built with the index and pre-evaluates every subtree without keyframes,
//! so each frame evaluates only what animates.

use crate::SampleContext;
use crate::animation::{sample_paint, sample_path, sample_point, sample_scalar, sample_vec2};
use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{EvaluatedGeometry, EvaluatedItem, EvaluatedTransform};
use alpha2_model::{
    Animated, AnimatedPoint, EllipseGeometry, PathGeometry, PolystarGeometry, RectangleGeometry,
    ShapeGeometry, ShapeItem, ShapeTransform,
};
use alpha2_time::MediaTime;
use std::sync::Arc;

/// A content tree with its static parts evaluated once.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapesPlan {
    items: Vec<Planned>,
    /// The whole tree, when nothing in it animates.
    fixed: Option<Arc<[EvaluatedItem]>>,
}

#[derive(Clone, Debug, PartialEq)]
enum Planned {
    Fixed(EvaluatedItem),
    /// A group with keyframes somewhere inside.
    Group {
        items: Vec<Planned>,
        fixed: Option<Arc<[EvaluatedItem]>>,
        transform: ShapeTransform,
    },
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

    /// The tree at `time`. `aspect` (width per unit height) shapes motion along spatial curves.
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

/// Generator trees are keyed in picture time, where no data binds.
const NO_DATA: &SampleContext<'static> = &SampleContext::NONE;

fn scalar(animation: &Animated<f32>, time: MediaTime, context: &SampleContext<'_>) -> f64 {
    f64::from(sample_scalar(animation, time, context))
}

fn point(
    animation: &AnimatedPoint,
    time: MediaTime,
    aspect: f64,
    context: &SampleContext<'_>,
) -> Point {
    let value = sample_point(animation, time, aspect, context);
    Point::new(f64::from(value.x), f64::from(value.y))
}

fn vector(
    animation: &Animated<alpha2_model::Vec2>,
    time: MediaTime,
    context: &SampleContext<'_>,
) -> Vec2 {
    let value = sample_vec2(animation, time, context);
    Vec2::new(f64::from(value.x), f64::from(value.y))
}

/// One item at `time`. Opacities, trims and roundness clamp to `0..=1` and lengths to `>= 0`,
/// since an overshooting curve can carry them past.
#[must_use]
pub fn evaluate_item(item: &ShapeItem, time: MediaTime, aspect: f64) -> EvaluatedItem {
    let context = NO_DATA;
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
            context,
        )),
        ShapeItem::Ellipse(geometry) => {
            EvaluatedItem::Geometry(ellipse(geometry, time, aspect, context))
        }
        ShapeItem::Polystar(geometry) => {
            EvaluatedItem::Geometry(polystar(geometry, time, aspect, context))
        }
        ShapeItem::Path(geometry) => EvaluatedItem::Geometry(path(geometry, time)),
        ShapeItem::Fill {
            paint,
            opacity,
            rule,
        } => EvaluatedItem::Fill {
            paint: sample_paint(paint, time),
            opacity: unit(scalar(opacity, time, context)),
            rule: *rule,
        },
        ShapeItem::Stroke(stroke) => EvaluatedItem::Stroke {
            paint: sample_paint(&stroke.paint, time),
            width: scalar(&stroke.width, time, context).max(0.0),
            opacity: unit(scalar(&stroke.opacity, time, context)),
            join: stroke.join,
            miter_limit: f64::from(stroke.miter_limit),
            cap: stroke.cap,
            dash: stroke.dash.as_ref().map_or_else(Vec::new, |dash| {
                dash.pattern
                    .iter()
                    .map(|entry| scalar(entry, time, context).max(0.0))
                    .collect()
            }),
            dash_offset: stroke
                .dash
                .as_ref()
                .map_or(0.0, |dash| scalar(&dash.offset, time, context)),
        },
        ShapeItem::Trim(trim) => EvaluatedItem::Trim {
            start: unit(scalar(&trim.start, time, context)),
            end: unit(scalar(&trim.end, time, context)),
            offset: scalar(&trim.offset, time, context),
            mode: trim.mode,
        },
    }
}

/// A geometry at `time`: a mask's outline, or a content tree's. `context` resolves a mask's
/// data bindings.
#[must_use]
pub fn evaluate_geometry(
    geometry: &ShapeGeometry,
    time: MediaTime,
    aspect: f64,
    context: &SampleContext<'_>,
) -> EvaluatedGeometry {
    match geometry {
        ShapeGeometry::Rectangle(RectangleGeometry {
            center,
            size,
            corner_radius,
            rotation_degrees,
        }) => EvaluatedGeometry::Rectangle {
            center: point(center, time, aspect, context),
            size: vector(size, time, context),
            corner_radius: scalar(corner_radius, time, context).max(0.0),
            rotation_degrees: scalar(rotation_degrees, time, context),
        },
        ShapeGeometry::Ellipse(geometry) => ellipse(geometry, time, aspect, context),
        ShapeGeometry::Polystar(geometry) => polystar(geometry, time, aspect, context),
        ShapeGeometry::Path(geometry) => path(geometry, time),
    }
}

fn ellipse(
    geometry: &EllipseGeometry,
    time: MediaTime,
    aspect: f64,
    context: &SampleContext<'_>,
) -> EvaluatedGeometry {
    EvaluatedGeometry::Ellipse {
        center: point(&geometry.center, time, aspect, context),
        size: vector(&geometry.size, time, context),
        rotation_degrees: scalar(&geometry.rotation_degrees, time, context),
    }
}

fn polystar(
    geometry: &PolystarGeometry,
    time: MediaTime,
    aspect: f64,
    context: &SampleContext<'_>,
) -> EvaluatedGeometry {
    EvaluatedGeometry::Polystar {
        kind: geometry.kind,
        center: point(&geometry.center, time, aspect, context),
        points: scalar(&geometry.points, time, context).clamp(
            f64::from(alpha2_model::MIN_POLYSTAR_POINTS),
            f64::from(alpha2_model::MAX_POLYSTAR_POINTS),
        ),
        outer_radius: scalar(&geometry.outer_radius, time, context).max(0.0),
        inner_radius: scalar(&geometry.inner_radius, time, context).max(0.0),
        outer_roundness: scalar(&geometry.outer_roundness, time, context).clamp(0.0, 1.0),
        inner_roundness: scalar(&geometry.inner_roundness, time, context).clamp(0.0, 1.0),
        rotation_degrees: scalar(&geometry.rotation_degrees, time, context),
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
    let context = NO_DATA;
    let anchor = vector(&transform.anchor, time, context);
    EvaluatedTransform {
        anchor: Point::new(anchor.x, anchor.y),
        position: point(&transform.position, time, aspect, context),
        scale: vector(&transform.scale, time, context),
        rotation_degrees: scalar(&transform.rotation_degrees, time, context),
        opacity: scalar(&transform.opacity, time, context).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
#[path = "shapes_tests.rs"]
mod tests;
