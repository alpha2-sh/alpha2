use crate::EvaluatedVisual;
use alpha2_model::{
    Animated, AnimatedPoint, EffectValue, GradientStop, Insets, Keyframe, LayoutLength,
    LayoutOffset, LayoutSize, Paint, Path, PointKeyframe, Vec2, VisualProperties,
};
use alpha2_time::MediaTime;

/// Evaluates visual properties at a timestamp. `aspect` is the canvas width over height, used
/// to measure motion along spatial curves.
///
/// Opacity is clamped to `0..=1`; position, scale and rotation keep curve overshoot.
#[must_use]
pub fn sample_visual(
    properties: &VisualProperties,
    time: MediaTime,
    aspect: f64,
) -> EvaluatedVisual {
    EvaluatedVisual {
        position: sample_point(&properties.position, time, aspect),
        anchor: properties.anchor,
        scale: evaluate_vec2(&properties.scale, time),
        rotation_degrees: evaluate_scalar(&properties.rotation_degrees, time),
        opacity: evaluate_scalar(&properties.opacity, time).clamp(0.0, 1.0),
    }
}

fn evaluate_vec2(animation: &Animated<Vec2>, time: MediaTime) -> Vec2 {
    match animation {
        Animated::Constant { value } => *value,
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => Vec2::new(
                interpolate_f32(left.value.x, right.value.x, amount),
                interpolate_f32(left.value.y, right.value.y, amount),
            ),
            Segment::At(keyframe) => keyframe.value,
            Segment::Empty => Vec2::default(),
        },
    }
}

/// Samples an animated 2D value like [`sample_scalar`]. Use [`sample_point`] for positions that
/// may follow spatial curves.
#[must_use]
pub fn sample_vec2(animation: &Animated<Vec2>, time: MediaTime) -> Vec2 {
    evaluate_vec2(animation, time)
}

/// Samples one animated scalar with the project's keyframe semantics.
///
/// Times outside the keyframes clamp to the first or last value, and a
/// [`alpha2_model::Interpolation::Hold`] segment keeps its left value. Interpolation is numeric,
/// so a rotation from 0 to 720 degrees turns twice.
#[must_use]
pub fn sample_scalar(animation: &Animated<f32>, time: MediaTime) -> f32 {
    evaluate_scalar(animation, time)
}

fn evaluate_scalar(animation: &Animated<f32>, time: MediaTime) -> f32 {
    match animation {
        Animated::Constant { value } => *value,
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => {
                interpolate_f32(left.value, right.value, amount)
            }
            Segment::At(keyframe) => keyframe.value,
            Segment::Empty => 0.0,
        },
    }
}

/// Samples a position that may move along spatial curves.
///
/// With both facing tangents zero the value moves in a straight line like an `Animated<Vec2>`.
/// Otherwise it follows the cubic Bezier, measured in a space `aspect` wide per unit high, and the
/// eased amount is the fraction of arc length travelled. Overshoot continues along the end
/// tangent.
#[must_use]
pub fn sample_point(animation: &AnimatedPoint, time: MediaTime, aspect: f64) -> Vec2 {
    match animation {
        AnimatedPoint::Constant { value } => *value,
        AnimatedPoint::Keyframes { keyframes } => match point_segment(keyframes, time) {
            PointSegment::At(value) => value,
            PointSegment::Between(left, right, amount) => {
                if left.out_tangent.is_zero() && right.in_tangent.is_zero() {
                    return Vec2::new(
                        interpolate_f32(left.value.x, right.value.x, amount),
                        interpolate_f32(left.value.y, right.value.y, amount),
                    );
                }
                let aspect = if aspect.is_finite() && aspect > 0.0 {
                    aspect
                } else {
                    1.0
                };
                let point = |value: Vec2| {
                    alpha2_geometry::kurbo::Point::new(
                        f64::from(value.x) * aspect,
                        f64::from(value.y),
                    )
                };
                let vector = |value: Vec2| {
                    alpha2_geometry::kurbo::Vec2::new(
                        f64::from(value.x) * aspect,
                        f64::from(value.y),
                    )
                };
                let at = alpha2_geometry::spatial::point_along(
                    point(left.value),
                    vector(left.out_tangent),
                    vector(right.in_tangent),
                    point(right.value),
                    amount,
                );
                Vec2::new(finite_f32(at.x / aspect), finite_f32(at.y))
            }
        },
    }
}

enum PointSegment<'a> {
    At(Vec2),
    Between(&'a PointKeyframe, &'a PointKeyframe, f64),
}

fn point_segment(keyframes: &[PointKeyframe], time: MediaTime) -> PointSegment<'_> {
    let next = keyframes.partition_point(|keyframe| keyframe.time <= time);
    if next == 0 {
        return PointSegment::At(
            keyframes
                .first()
                .map_or_else(Vec2::default, |key| key.value),
        );
    }
    let left = &keyframes[next - 1];
    let Some(right) = keyframes.get(next) else {
        return PointSegment::At(left.value);
    };
    if time == left.time {
        return PointSegment::At(left.value);
    }
    let progress = interpolation_amount(left.time, right.time, time);
    match left.interpolation.amount(progress) {
        Some(amount) => PointSegment::Between(left, right, amount),
        None => PointSegment::At(left.value),
    }
}

/// Nearest finite `f32`; NaN becomes zero.
#[allow(clippy::cast_possible_truncation)]
fn finite_f32(value: f64) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
    }
}

/// Samples an animated path, interpolating points and tangents linearly in `f64`. Mismatched
/// keyframe structures hold the left path.
#[must_use]
pub fn sample_path(animation: &Animated<Path>, time: MediaTime) -> alpha2_geometry::EvaluatedPath {
    use alpha2_geometry::kurbo::{Point, Vec2 as Vector};
    use alpha2_geometry::{EvaluatedPath, EvaluatedSubpath, EvaluatedVertex};
    match animation {
        Animated::Constant { value } => EvaluatedPath::from(value),
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::At(keyframe) => EvaluatedPath::from(&keyframe.value),
            Segment::Empty => EvaluatedPath::default(),
            Segment::Between(left, right, amount) => {
                let structure = |path: &Path| {
                    path.subpaths
                        .iter()
                        .map(|subpath| (subpath.closed, subpath.vertices.len()))
                        .collect::<Vec<_>>()
                };
                if structure(&left.value) != structure(&right.value) {
                    return EvaluatedPath::from(&left.value);
                }
                let lerp = |a: f32, b: f32| f64::from(a) + (f64::from(b) - f64::from(a)) * amount;
                let point = |a: Vec2, b: Vec2| Point::new(lerp(a.x, b.x), lerp(a.y, b.y));
                let vector = |a: Vec2, b: Vec2| Vector::new(lerp(a.x, b.x), lerp(a.y, b.y));
                EvaluatedPath {
                    subpaths: left
                        .value
                        .subpaths
                        .iter()
                        .zip(&right.value.subpaths)
                        .map(|(from, to)| EvaluatedSubpath {
                            closed: from.closed,
                            vertices: from
                                .vertices
                                .iter()
                                .zip(&to.vertices)
                                .map(|(a, b)| EvaluatedVertex {
                                    point: point(a.point, b.point),
                                    in_tangent: vector(a.in_tangent, b.in_tangent),
                                    out_tangent: vector(a.out_tangent, b.out_tangent),
                                })
                                .collect(),
                        })
                        .collect(),
                }
            }
        },
    }
}

/// Samples an animated paint. Colors interpolate premultiplied; gradient geometry and stop
/// offsets interpolate numerically, then offsets are kept ordered in `0..=1` and radii positive.
/// Mismatched keyframes hold the left paint.
#[must_use]
pub fn sample_paint(animation: &Animated<Paint>, time: MediaTime) -> Paint {
    match animation {
        Animated::Constant { value } => value.clone(),
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::At(keyframe) => keyframe.value.clone(),
            Segment::Empty => Paint::Solid {
                color: alpha2_model::Rgba::new(0.0, 0.0, 0.0, 0.0),
            },
            Segment::Between(left, right, amount) => {
                interpolate_paint(&left.value, &right.value, amount)
                    .unwrap_or_else(|| left.value.clone())
            }
        },
    }
}

fn interpolate_paint(from: &Paint, to: &Paint, amount: f64) -> Option<Paint> {
    let lerp = |a: f32, b: f32| interpolate_f32(a, b, amount);
    let point = |a: Vec2, b: Vec2| Vec2::new(lerp(a.x, b.x), lerp(a.y, b.y));
    let color = |a: alpha2_model::Rgba, b: alpha2_model::Rgba| {
        let [red, green, blue, alpha] = interpolate_color(
            [a.red, a.green, a.blue, a.alpha],
            [b.red, b.green, b.blue, b.alpha],
            amount,
        );
        alpha2_model::Rgba::new(red, green, blue, alpha)
    };
    let stops = |a: &[GradientStop], b: &[GradientStop]| -> Option<Vec<GradientStop>> {
        if a.len() != b.len() {
            return None;
        }
        let mut previous = 0.0_f32;
        Some(
            a.iter()
                .zip(b)
                .map(|(a, b)| {
                    let offset = lerp(a.offset, b.offset).clamp(previous, 1.0);
                    previous = offset;
                    GradientStop {
                        offset,
                        color: color(a.color, b.color),
                    }
                })
                .collect(),
        )
    };
    Some(match (from, to) {
        (Paint::Solid { color: a }, Paint::Solid { color: b }) => Paint::Solid {
            color: color(*a, *b),
        },
        (
            Paint::LinearGradient {
                start: s0,
                end: e0,
                stops: a,
                spread,
            },
            Paint::LinearGradient {
                start: s1,
                end: e1,
                stops: b,
                spread: other,
            },
        ) if spread == other => Paint::LinearGradient {
            start: point(*s0, *s1),
            end: point(*e0, *e1),
            stops: stops(a, b)?,
            spread: *spread,
        },
        (
            Paint::RadialGradient {
                center: c0,
                radius: r0,
                stops: a,
                spread,
            },
            Paint::RadialGradient {
                center: c1,
                radius: r1,
                stops: b,
                spread: other,
            },
        ) if spread == other => Paint::RadialGradient {
            center: point(*c0, *c1),
            radius: lerp(*r0, *r1).max(f32::MIN_POSITIVE),
            stops: stops(a, b)?,
            spread: *spread,
        },
        (
            Paint::ConicGradient {
                center: c0,
                angle_degrees: a0,
                stops: a,
            },
            Paint::ConicGradient {
                center: c1,
                angle_degrees: a1,
                stops: b,
            },
        ) => Paint::ConicGradient {
            center: point(*c0, *c1),
            angle_degrees: lerp(*a0, *a1),
            stops: stops(a, b)?,
        },
        _ => return None,
    })
}

/// One effect parameter's value at a timeline timestamp.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EffectParamValue {
    /// The sampled value.
    pub value: EffectValue,
    /// Whether `value` is interpolated rather than authored. Renderers clamp interpolated values
    /// to the parameter range; an authored out-of-range value is an error.
    pub interpolated: bool,
}

/// Samples one effect parameter with the project's keyframe semantics.
///
/// `Float` and `Vec2` interpolate component-wise. A straight sRGB `Color` interpolates
/// premultiplied, as CSS does, to avoid a dark fringe when fading from transparent, then clamps
/// to `0..=1`. `Enum` and `Bool` hold.
#[must_use]
pub fn sample_effect_param(animation: &Animated<EffectValue>, time: MediaTime) -> EffectParamValue {
    let authored = |value: EffectValue| EffectParamValue {
        value,
        interpolated: false,
    };
    match animation {
        Animated::Constant { value } => authored(*value),
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => {
                interpolate_value(left.value, right.value, amount).map_or_else(
                    || authored(left.value),
                    |value| EffectParamValue {
                        value,
                        interpolated: true,
                    },
                )
            }
            Segment::At(keyframe) => authored(keyframe.value),
            Segment::Empty => authored(EffectValue::Float(0.0)),
        },
    }
}

fn interpolate_value(start: EffectValue, end: EffectValue, amount: f64) -> Option<EffectValue> {
    let lerp = |a: f32, b: f32| interpolate_f32(a, b, amount);
    match (start, end) {
        (EffectValue::Float(a), EffectValue::Float(b)) => Some(EffectValue::Float(lerp(a, b))),
        (EffectValue::Vec2(a), EffectValue::Vec2(b)) => {
            Some(EffectValue::Vec2([lerp(a[0], b[0]), lerp(a[1], b[1])]))
        }
        (EffectValue::Vec4(a), EffectValue::Vec4(b)) => {
            Some(EffectValue::Vec4(std::array::from_fn(|index| {
                lerp(a[index], b[index])
            })))
        }
        (EffectValue::Color(a), EffectValue::Color(b)) => {
            Some(EffectValue::Color(interpolate_color(a, b, amount)))
        }
        _ => None,
    }
}

/// Samples an animated color, interpolated premultiplied.
#[must_use]
pub fn sample_rgba(
    animation: &Animated<alpha2_model::Rgba>,
    time: MediaTime,
) -> alpha2_model::Rgba {
    match animation {
        Animated::Constant { value } => *value,
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::At(keyframe) => keyframe.value,
            Segment::Empty => alpha2_model::Rgba::new(0.0, 0.0, 0.0, 0.0),
            Segment::Between(left, right, amount) => {
                let (a, b) = (left.value, right.value);
                let [red, green, blue, alpha] = interpolate_color(
                    [a.red, a.green, a.blue, a.alpha],
                    [b.red, b.green, b.blue, b.alpha],
                    amount,
                );
                alpha2_model::Rgba::new(red, green, blue, alpha)
            }
        },
    }
}

/// Interpolates straight colors premultiplied; returns straight, clamped to `0..=1`.
#[allow(clippy::cast_possible_truncation)]
fn interpolate_color(start: [f32; 4], end: [f32; 4], amount: f64) -> [f32; 4] {
    let (start_alpha, end_alpha) = (f64::from(start[3]), f64::from(end[3]));
    let alpha = (start_alpha + (end_alpha - start_alpha) * amount).clamp(0.0, 1.0);
    let mut color = [0.0; 4];
    for index in 0..3 {
        let from = f64::from(start[index]) * start_alpha;
        let to = f64::from(end[index]) * end_alpha;
        let premultiplied = from + (to - from) * amount;
        // Fully transparent has no color; keep straight interpolation so the value stays continuous.
        let straight = if alpha > 0.0 {
            premultiplied / alpha
        } else {
            f64::from(start[index]) + (f64::from(end[index]) - f64::from(start[index])) * amount
        };
        color[index] = straight.clamp(0.0, 1.0) as f32;
    }
    color[3] = alpha as f32;
    color
}

enum Segment<'a, T> {
    /// Before the first, after the last, or inside a held segment.
    At(&'a Keyframe<T>),
    /// Inside an interpolated segment, this fraction of the way to the right.
    Between(&'a Keyframe<T>, &'a Keyframe<T>, f64),
    /// No keyframes; validation rejects this.
    Empty,
}

fn segment<T>(keyframes: &[Keyframe<T>], time: MediaTime) -> Segment<'_, T> {
    let next = keyframes.partition_point(|keyframe| keyframe.time <= time);
    if next == 0 {
        return keyframes.first().map_or(Segment::Empty, Segment::At);
    }
    let left = &keyframes[next - 1];
    let Some(right) = keyframes.get(next) else {
        return Segment::At(left);
    };
    // On a keyframe the value is the authored one, whatever the curve.
    if time == left.time {
        return Segment::At(left);
    }
    let progress = interpolation_amount(left.time, right.time, time);
    match left.interpolation.amount(progress) {
        Some(amount) => Segment::Between(left, right, amount),
        None => Segment::At(left),
    }
}

#[allow(clippy::cast_precision_loss)]
fn interpolation_amount(start: MediaTime, end: MediaTime, time: MediaTime) -> f64 {
    (i128::from(time.ticks()) - i128::from(start.ticks())) as f64
        / (i128::from(end.ticks()) - i128::from(start.ticks())) as f64
}

/// Linear interpolation clamped to finite `f32`, since overshoot can exceed its range.
#[allow(clippy::cast_possible_truncation)]
fn interpolate_f32(start: f32, end: f32, amount: f64) -> f32 {
    (f64::from(start) + (f64::from(end) - f64::from(start)) * amount)
        .clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

/// Samples per-side lengths like `sample_size`.
#[must_use]
pub fn sample_insets(animation: &Animated<Insets>, time: MediaTime) -> [f32; 4] {
    match animation {
        Animated::Constant { value } => value.sides().map(LayoutSize::value),
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => {
                let (from, to) = (left.value.sides(), right.value.sides());
                std::array::from_fn(|side| {
                    interpolate_f32(from[side].value(), to[side].value(), amount)
                })
            }
            Segment::At(keyframe) => keyframe.value.sides().map(LayoutSize::value),
            Segment::Empty => [0.0; 4],
        },
    }
}

/// Samples a layout size like [`sample_scalar`]. Plan sizes are fractions.
#[must_use]
pub fn sample_size(animation: &Animated<LayoutSize>, time: MediaTime) -> f32 {
    match animation {
        Animated::Constant { value } => value.value(),
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => {
                interpolate_f32(left.value.value(), right.value.value(), amount)
            }
            Segment::At(keyframe) => keyframe.value.value(),
            Segment::Empty => 0.0,
        },
    }
}

/// Samples a layout offset per axis like [`sample_size`].
#[must_use]
pub fn sample_offset(animation: &Animated<LayoutOffset>, time: MediaTime) -> Vec2 {
    let vector = |offset: &LayoutOffset| Vec2::new(offset.x.value(), offset.y.value());
    match animation {
        Animated::Constant { value } => vector(value),
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => Vec2::new(
                interpolate_f32(left.value.x.value(), right.value.x.value(), amount),
                interpolate_f32(left.value.y.value(), right.value.y.value(), amount),
            ),
            Segment::At(keyframe) => vector(&keyframe.value),
            Segment::Empty => Vec2::default(),
        },
    }
}

/// Samples a layout length. Mismatched keyframe kinds keep the left kind.
#[must_use]
pub fn sample_length(animation: &Animated<LayoutLength>, time: MediaTime) -> LayoutLength {
    match animation {
        Animated::Constant { value } => *value,
        Animated::Keyframes { keyframes } => match segment(keyframes, time) {
            Segment::Between(left, right, amount) => left.value.with_value(interpolate_f32(
                left.value.value(),
                right.value.value(),
                amount,
            )),
            Segment::At(keyframe) => keyframe.value,
            Segment::Empty => LayoutLength::Fraction(0.0),
        },
    }
}

/// Samples a non-interpolating value: the last keyframe at or before `time`, else the first.
#[must_use]
pub fn sample_hold<T: Clone + Default>(animation: &Animated<T>, time: MediaTime) -> T {
    match animation {
        Animated::Constant { value } => value.clone(),
        Animated::Keyframes { keyframes } => {
            let next = keyframes.partition_point(|keyframe| keyframe.time <= time);
            keyframes
                .get(next.saturating_sub(1))
                .map(|keyframe| keyframe.value.clone())
                .unwrap_or_default()
        }
    }
}
