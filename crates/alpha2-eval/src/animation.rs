use crate::EvaluatedVisual;
use alpha2_model::{Animated, EffectValue, Keyframe, Vec2, VisualProperties};
use alpha2_time::MediaTime;

/// Evaluates visual properties at a timestamp: a clip's at timeline time,
/// a transform node's at composition time.
///
/// An overshooting timing curve can carry a value past its keyframes. Opacity
/// is clamped to its domain, `0..=1`; position, scale, and rotation have no
/// bounds and keep the overshoot, as far as a finite `f32` goes.
#[must_use]
pub fn sample_visual(properties: &VisualProperties, time: MediaTime) -> EvaluatedVisual {
    EvaluatedVisual {
        position: evaluate_vec2(&properties.position, time),
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

/// Samples one animated two-dimensional value with the project's keyframe
/// semantics; see [`sample_scalar`].
#[must_use]
pub fn sample_vec2(animation: &Animated<Vec2>, time: MediaTime) -> Vec2 {
    evaluate_vec2(animation, time)
}

/// Samples one animated scalar with the project's keyframe semantics.
///
/// Values outside the keyframe sequence clamp to its first or last value, and a
/// [`alpha2_model::Interpolation::Hold`] segment keeps its left value until the
/// next keyframe. Values are interpolated numerically, so a rotation from 0 to
/// 720 degrees turns twice. Audio mixing calls this at control points fixed
/// by the project sample, and on every sample around a keyframe, so that
/// automation does not depend on the host block size.
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

/// One effect parameter's value at a timeline timestamp.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EffectParamValue {
    /// The value at this timestamp.
    pub value: EffectValue,
    /// Whether `value` lies between two keyframes rather than being an
    /// authored value. A timing curve may carry an interpolated value past
    /// the parameter's range, and a renderer clamps it there; an authored
    /// value outside the range is an error.
    pub interpolated: bool,
}

/// Samples one effect parameter with the project's keyframe semantics.
///
/// `Float` and `Vec2` interpolate component-wise. A `Color` (straight,
/// sRGB-encoded) interpolates premultiplied, as CSS colors do, so a fade from
/// transparent to red does not pass through a dark fringe; it is clamped to
/// `0..=1` after an overshooting curve. `Enum` and `Bool` keyframes hold
/// (validation rejects any other interpolation for them).
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

/// Interpolates two straight colors premultiplied and returns the result
/// straight, clamped to `0..=1`.
#[allow(clippy::cast_possible_truncation)]
fn interpolate_color(start: [f32; 4], end: [f32; 4], amount: f64) -> [f32; 4] {
    let (start_alpha, end_alpha) = (f64::from(start[3]), f64::from(end[3]));
    let alpha = (start_alpha + (end_alpha - start_alpha) * amount).clamp(0.0, 1.0);
    let mut color = [0.0; 4];
    for index in 0..3 {
        let from = f64::from(start[index]) * start_alpha;
        let to = f64::from(end[index]) * end_alpha;
        let premultiplied = from + (to - from) * amount;
        // Fully transparent has no color; keep the straight interpolation so the value stays
        // continuous.
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

/// Where a timestamp falls in a keyframe sequence.
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

/// The value `amount` of the way from `start` to `end`, held to the finite
/// `f32` values: an overshooting curve can carry it past what an `f32` holds.
#[allow(clippy::cast_possible_truncation)]
fn interpolate_f32(start: f32, end: f32, amount: f64) -> f32 {
    (f64::from(start) + (f64::from(end) - f64::from(start)) * amount)
        .clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}
